using JwmWaterLily
using Sockets
using Test

@testset "CLI whitelist and dimensions" begin
    options = parse_cli([
        "--case=hover",
        "--device",
        "cpu",
        "--fps",
        "24",
        "--socket",
        "/tmp/jwm-waterlily-test.sock",
        "--frame-file",
        "/tmp/jwm-waterlily-test.frame",
        "--sim-size",
        "320x200",
    ])
    @test options.case_name == "hover"
    @test options.device == :cpu
    @test options.requested_size == (320, 200)
    @test options.simulation_size == (320, 208)
    @test normalize_size((128, 128)) == (128, 128)
    @test_throws ArgumentError parse_cli(["--case", "../../arbitrary.jl"])
    @test_throws ArgumentError parse_cli(["--device", "metal"])
    @test_throws ArgumentError parse_cli(["--unknown", "value"])
end

function read_u32_le(bytes, offset)
    return ltoh(reinterpret(UInt32, bytes[offset + 1:offset + 4])[1])
end

function read_u64_le(bytes, offset)
    return ltoh(reinterpret(UInt64, bytes[offset + 1:offset + 8])[1])
end

@testset "double-buffer frame protocol" begin
    mktempdir() do directory
        path = joinpath(directory, "frame")
        publisher = FramePublisher(path, 2, 2)
        first = UInt8[repeat([0x10, 0x20, 0x30, 0xff], 4)...]
        second = UInt8[repeat([0x40, 0x50, 0x60, 0xff], 4)...]

        @test publish!(publisher, first, 123) == 1
        @test publish!(publisher, second, 456) == 2
        close(publisher)

        bytes = read(path)
        @test bytes[1:8] == UInt8[codeunits("JWMLILY\0")...]
        @test read_u32_le(bytes, 8) == 1
        @test read_u32_le(bytes, 12) == 64
        @test read_u32_le(bytes, 16) == 2
        @test read_u32_le(bytes, 20) == 2
        @test read_u32_le(bytes, 24) == 8
        @test read_u32_le(bytes, 28) == 1
        @test read_u32_le(bytes, 32) == 1
        @test read_u32_le(bytes, 36) == 1
        @test read_u32_le(bytes, 40) == 1
        @test read_u32_le(bytes, 44) == 1
        @test read_u64_le(bytes, 48) == 2
        @test read_u64_le(bytes, 56) == 456
        @test bytes[65:80] == first
        @test bytes[81:96] == second
        @test (stat(path).mode & 0o077) == 0
    end
end

@testset "RGBA renderer helpers" begin
    @test JwmWaterLily.seismic_color(-1, 1) !=
          JwmWaterLily.seismic_color(1, 1)
    @test JwmWaterLily.seismic_color(0, 1) ==
          JwmWaterLily.SEISMIC_PALETTE[6]
end

@testset "palettes share the compositor keying contract" begin
    @test length(JwmWaterLily.ALL_PALETTES) == 11
    @test allunique(JwmWaterLily.ALL_PALETTES)
    for palette in JwmWaterLily.ALL_PALETTES
        @test length(palette) == 11
        # The compositor shader replaces bright, low-chroma pixels with the
        # frosted backdrop; every palette midpoint must stay in that key.
        center = palette[6]
        @test minimum(center) >= 0xf0
        @test maximum(center) - minimum(center) <= 6
        # The extremes must stay saturated so vortices remain opaque.
        for extreme in (palette[1], palette[end])
            @test maximum(extreme) - minimum(extreme) > 0x30
        end
    end
end

@testset "case registry lists every effect" begin
    @test available_cases() == [
        "cylinder",
        "dance",
        "diamond",
        "flap",
        "hover",
        "orbit",
        "rain",
        "stylus",
        "tandem",
        "waltz",
        "wander",
    ]
end

@testset "palette registry backs the hot-swap command" begin
    @test available_palettes() == [
        "aurora",
        "berry",
        "cosmos",
        "ember",
        "fluent",
        "glacier",
        "mica",
        "ocean",
        "seismic",
        "sith",
        "violet",
    ]
    # Every registered palette obeys the same keying contract as ALL_PALETTES.
    for name in available_palettes()
        @test JwmWaterLily.PALETTE_REGISTRY[name] in JwmWaterLily.ALL_PALETTES
    end
    @test JwmWaterLily.palette_shimmer("mica")
    @test !JwmWaterLily.palette_shimmer("fluent")
    @test !JwmWaterLily.palette_shimmer("seismic")

    @test JwmWaterLily.resolve_palette_command("fluent", "seismic") == "fluent"
    @test JwmWaterLily.resolve_palette_command("auto", "seismic") === nothing
    @test JwmWaterLily.resolve_palette_command("next", "aurora") == "berry"
    # `next` wraps the sorted registry and recovers from unknown current names.
    @test JwmWaterLily.resolve_palette_command("next", "violet") == "aurora"
    @test JwmWaterLily.resolve_palette_command("next", "retired") == "aurora"
    @test JwmWaterLily.resolve_palette_command("../../etc", "seismic") === missing
end

@testset "pointer command parsing" begin
    @test JwmWaterLily.parse_pointer_command(["pointer", "0.25", "0.75"]) == (0.25, 0.75)
    # Out-of-range samples clamp instead of being dropped.
    @test JwmWaterLily.parse_pointer_command(["pointer", "-1.5", "2.0"]) == (0.0, 1.0)
    @test JwmWaterLily.parse_pointer_command(["pointer", "0.5"]) === nothing
    @test JwmWaterLily.parse_pointer_command(["pointer", "0.5", "NaN"]) === nothing
    @test JwmWaterLily.parse_pointer_command(["pointer", "0.5", "bogus"]) === nothing
end

@testset "hot-switch command resolution" begin
    @test JwmWaterLily.resolve_case_command("case dance", "hover") == "dance"
    @test JwmWaterLily.resolve_case_command("case next", "cylinder") == "dance"
    # `next` wraps the sorted registry and recovers from unknown current names.
    @test JwmWaterLily.resolve_case_command("case next", "wander") == "cylinder"
    @test JwmWaterLily.resolve_case_command("case next", "retired") == "cylinder"
    @test JwmWaterLily.resolve_case_command("case ../../etc", "hover") === nothing
    @test JwmWaterLily.resolve_case_command("bogus", "hover") === nothing
end

@testset "budgeted advance dilates instead of stalling" begin
    case = build_case("wander", (64, 64); memory=Array)
    # A generous deadline reaches the requested step.
    full = JwmWaterLily.advance_budgeted!(case, 0.02, time_ns() + UInt64(30_000_000_000))
    @test full >= 0.02
    # An expired deadline still takes exactly one substep and makes progress.
    partial = JwmWaterLily.advance_budgeted!(case, 10.0, time_ns())
    @test 0 < partial < 10.0
end

@testset "wandering body stays inside the canvas" begin
    case = build_case("wander", (128, 64); memory=Array)
    margin = case.radius
    for time in 0.0:0.25:120.0
        x, y = JwmWaterLily.wander_position(case, time)
        @test margin <= x <= 128 - margin
        @test margin <= y <= 64 - margin
    end
    # The non-repeating path must sweep most of the canvas over time.
    xs = [JwmWaterLily.wander_position(case, t)[1] for t in 0.0:0.5:600.0]
    ys = [JwmWaterLily.wander_position(case, t)[2] for t in 0.0:0.5:600.0]
    @test maximum(xs) - minimum(xs) > 0.7 * 128
    @test maximum(ys) - minimum(ys) > 0.5 * 64
end

@testset "stylus spring chases the pointer without teleporting" begin
    case = build_case("stylus", (128, 64); memory=Array)
    T = Float32
    @test JwmWaterLily.case_palette_name(case) == "fluent"

    # A rest-to-rest segment must peak below the CFL-safe speed cap and
    # settle at its target.
    segment = case.segment
    far = JwmWaterLily.SpringSegment{T}(
        T(0),
        segment.position,
        JwmWaterLily.SA[T(0), T(0)],
        segment.position + JwmWaterLily.SA[
            T(JwmWaterLily.STYLUS_MAX_SPEED * Base.MathConstants.e) / segment.rate,
            T(0),
        ],
        segment.rate,
    )
    cap = T(JwmWaterLily.STYLUS_MAX_SPEED) * T(case.simulation.U)
    horizon = 8.0 / Float64(segment.rate)
    for time in range(0.0, horizon; length=400)
        velocity = JwmWaterLily.segment_velocity(far, time)
        @test hypot(velocity[1], velocity[2]) <= cap * 1.001
    end
    settled = JwmWaterLily.segment_position(far, horizon)
    @test isapprox(settled[1], far.target[1]; atol=0.5)

    # Pointer updates map top-left normalized coordinates into the y-up grid
    # and clamp inside the margin.
    JwmWaterLily.handle_pointer!(case, 1.0, 1.0)
    @test case.goal[1] ≈ 128 - case.margin
    @test case.goal[2] ≈ case.margin
    JwmWaterLily.handle_pointer!(case, 0.5, 0.0)
    @test case.goal[1] ≈ 64
    @test case.goal[2] ≈ 64 - case.margin

    # Retargeting splices the new segment at the current pose: the body must
    # not jump when the pointer does.
    now = Float64(JwmWaterLily.WaterLily.time(case.simulation.flow))
    before = JwmWaterLily.segment_position(case.segment, now)
    JwmWaterLily.handle_pointer!(case, 0.9, 0.8)
    after = JwmWaterLily.segment_position(case.segment, now)
    @test isapprox(before[1], after[1]; atol=1e-3)
    @test isapprox(before[2], after[2]; atol=1e-3)

    # The chase makes real progress toward the goal as the simulation runs.
    start = JwmWaterLily.segment_position(case.segment, now)
    start_gap = hypot((case.goal - start)...)
    for _ in 1:12
        JwmWaterLily.frame_tick!(case)
        JwmWaterLily.advance!(case, 0.05)
    end
    later = Float64(JwmWaterLily.WaterLily.time(case.simulation.flow))
    position = JwmWaterLily.segment_position(case.segment, later)
    @test hypot((case.goal - position)...) < start_gap
end

@testset "waltz heave rides the chase spring within the speed budget" begin
    case = build_case("waltz", (128, 64); memory=Array)
    T = Float32
    @test JwmWaterLily.case_palette_name(case) == "mica"

    # The chase spring must peak below its 2U share of the CFL budget while
    # the heave contributes exactly its budgeted 1U peak, so the combined
    # body speed stays inside the 3U envelope the stylus case proves out.
    segment = case.segment
    far = JwmWaterLily.SpringSegment{T}(
        T(0),
        segment.position,
        JwmWaterLily.SA[T(0), T(0)],
        segment.position + JwmWaterLily.SA[
            T(JwmWaterLily.WALTZ_CHASE_MAX_SPEED * Base.MathConstants.e) / segment.rate,
            T(0),
        ],
        segment.rate,
    )
    chase_cap = T(JwmWaterLily.WALTZ_CHASE_MAX_SPEED) * T(case.simulation.U)
    horizon = 8.0 / Float64(segment.rate)
    for time in range(0.0, horizon; length=400)
        velocity = JwmWaterLily.segment_velocity(far, time)
        @test hypot(velocity[1], velocity[2]) <= chase_cap * 1.001
    end
    heave_peak = case.heave_amplitude * case.heave_rate
    @test heave_peak ≈ T(JwmWaterLily.WALTZ_HEAVE_PEAK_SPEED) * T(case.simulation.U)

    # Pointer updates map top-left normalized coordinates into the y-up grid;
    # the vertical clamp reserves the heave amplitude on top of the margin so
    # the oscillating body never leaves the canvas.
    JwmWaterLily.handle_pointer!(case, 1.0, 1.0)
    @test case.goal[1] ≈ 128 - case.margin
    @test case.goal[2] ≈ case.margin + case.heave_amplitude
    JwmWaterLily.handle_pointer!(case, 0.5, 0.0)
    @test case.goal[1] ≈ 64
    @test case.goal[2] ≈ 64 - case.margin - case.heave_amplitude

    # Retargeting splices the new spring segment at the current pose, and the
    # heave depends only on absolute solver time: the body must not jump when
    # the pointer does.
    now = Float64(JwmWaterLily.WaterLily.time(case.simulation.flow))
    τ = JwmWaterLily.simulation_time(case)
    before = JwmWaterLily.waltz_position(case, τ)
    JwmWaterLily.handle_pointer!(case, 0.9, 0.8)
    after = JwmWaterLily.waltz_position(case, τ)
    @test isapprox(before[1], after[1]; atol=1e-3)
    @test isapprox(before[2], after[2]; atol=1e-3)

    # The chase makes real progress toward the goal as the simulation runs.
    start = JwmWaterLily.segment_position(case.segment, now)
    start_gap = hypot((case.goal - start)...)
    for _ in 1:12
        JwmWaterLily.frame_tick!(case)
        JwmWaterLily.advance!(case, 0.05)
    end
    later = Float64(JwmWaterLily.WaterLily.time(case.simulation.flow))
    position = JwmWaterLily.segment_position(case.segment, later)
    @test hypot((case.goal - position)...) < start_gap
end

@testset "wake client receives hot-switch commands" begin
    mktempdir() do directory
        path = joinpath(directory, "wake.sock")
        server = Sockets.listen(path)
        client = JwmWaterLily.WakeClient(path)
        @test notify!(client)
        consumer = Sockets.accept(server)
        @test read(consumer, UInt8) == 0x01

        write(consumer, "case dance\ncase next\n")
        flush(consumer)
        deadline = time() + 5.0
        received = String[]
        while length(received) < 2 && time() < deadline
            command = JwmWaterLily.take_command!(client)
            command === nothing ? sleep(0.01) : push!(received, command)
        end
        @test received == ["case dance", "case next"]
        @test JwmWaterLily.take_command!(client) === nothing

        close(client)
        close(consumer)
        close(server)
    end
end

# The rain case is a host-side particle model with translucent output and no
# immersed body, so it has its own testset below instead of the fluid smoke.
@testset "CPU simulation smoke: $name" for name in
                                           filter(!=("rain"), available_cases())
    simulation_case = build_case(name, (64, 64); memory=Array)
    JwmWaterLily.advance!(simulation_case, 0.01)
    rgba = render_rgba(simulation_case)

    @test length(rgba) == 64 * 64 * 4
    @test all(==(0xff), @view rgba[4:4:end])
    @test length(unique(Iterators.partition(rgba, 4))) > 2

    # The reusable-scratch fast path must colorize exactly like the
    # allocating wrapper, and its body bounds must contain the body.
    scratch = JwmWaterLily.RenderScratch((64, 64))
    JwmWaterLily.compute_vorticity!(scratch, simulation_case)
    pose_time = JwmWaterLily.simulation_time(simulation_case)
    scratch_rgba = JwmWaterLily.render_rgba!(scratch, simulation_case, pose_time)
    @test scratch_rgba === scratch.rgba
    @test scratch_rgba == rgba

    bounds = JwmWaterLily.body_bounds(simulation_case, pose_time)
    @test bounds !== nothing
    xmin, xmax, ymin, ymax = bounds
    center_x, center_y = (xmin + xmax) / 2, (ymin + ymax) / 2
    @test JwmWaterLily.body_distance(simulation_case, center_x, center_y, pose_time) <
          (xmax - xmin)
end

@testset "rain drops pin, run, and wipe the mist" begin
    case = build_case("rain", (128, 96); memory=Array)
    @test JwmWaterLily.case_palette_name(case) == "glacier"
    @test !isempty(case.drops)

    JwmWaterLily.advance!(case, 2.0)
    @test JwmWaterLily.simulation_time(case) ≈ 2.0

    rgba = render_rgba(case)
    @test length(rgba) == 128 * 96 * 4
    @test length(unique(Iterators.partition(rgba, 4))) > 2
    # Drop interiors and wet film are translucent so the sharp scene behind
    # the canvas shows through them; the mist itself stays opaque key-white.
    @test any(<(0xff), @view rgba[4:4:end])
    @test any(==(0xff), @view rgba[4:4:end])

    # The budgeted advance reaches a full step under a generous deadline and
    # still makes progress under an expired one.
    full = JwmWaterLily.advance_budgeted!(case, 0.5, time_ns() + UInt64(30_000_000_000))
    @test full ≈ 0.5
    partial = JwmWaterLily.advance_budgeted!(case, 10.0, time_ns())
    @test 0 < partial < 10.0

    # A forced runner sheds a wet trail as it slides.
    runner = JwmWaterLily.rain_stuck_drop(64.0, 10.0, 0.0, case.unit)
    runner.radius = runner.release_radius * 1.2f0
    runner.sliding = true
    push!(case.drops, runner)
    JwmWaterLily.advance!(case, 1.0)
    trail = maximum(
        @view case.wetness[:, 10:min(96, round(Int, 10 + runner.speed + 3))]
    )
    @test trail > 0.5

    # The pointer wipes the mist clear and sweeps drops out of the swath;
    # the fog then re-forms on its own.
    JwmWaterLily.handle_pointer!(case, 0.5, 0.5)
    @test case.wetness[64, 48] ≈ 1.0
    wipe = 0.055f0 * 96
    @test all(d -> hypot(d.x - 64, d.y - 48) > wipe, case.drops)
    JwmWaterLily.advance!(case, 5.0)
    @test case.wetness[64, 48] < 1.0
end
