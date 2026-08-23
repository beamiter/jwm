using JwmWaterLily
using Random
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
    # CUDA is the default backend; CPU and auto remain explicit opt-ins.
    @test parse_cli(String[]).device == :cuda
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

@testset "volumetric frame protocol" begin
    mktempdir() do directory
        path = joinpath(directory, "frame")
        publisher = FramePublisher(path, 2, 2; depth=2)
        front = UInt8[repeat([0x11, 0x22, 0x33, 0x40], 4)...]
        back = UInt8[repeat([0x44, 0x55, 0x66, 0x80], 4)...]
        voxels = vcat(front, back)
        @test publish!(publisher, voxels, 789) == 1
        close(publisher)

        bytes = read(path)
        @test bytes[1:8] == UInt8[codeunits("JWMLILY\0")...]
        # Version 2 with the 96-byte extended header carrying the depth.
        @test read_u32_le(bytes, 8) == 2
        @test read_u32_le(bytes, 12) == 96
        @test read_u32_le(bytes, 16) == 2
        @test read_u32_le(bytes, 20) == 2
        @test read_u32_le(bytes, 24) == 8
        @test read_u32_le(bytes, 64) == 2
        @test read_u32_le(bytes, 44) == 0
        @test read_u64_le(bytes, 48) == 1
        @test read_u64_le(bytes, 56) == 789
        # The published slot sits straight behind the extended header and
        # holds both slices front to back.
        @test bytes[97:128] == voxels
        @test length(bytes) == 96 + 2 * 32

        # A geometry switch replaces the file and seeds the next publisher
        # with the previous sequence, so the consumer's monotonic view
        # survives the swap and the next publication continues the count.
        replacement = FramePublisher(path, 2, 2; start_sequence=UInt64(1))
        @test publish!(replacement, UInt8[repeat([0x01, 0x02, 0x03, 0xff], 4)...], 5) == 2
        close(replacement)
        replaced = read(path)
        @test read_u32_le(replaced, 8) == 1
        @test read_u32_le(replaced, 12) == 64
        @test read_u64_le(replaced, 48) == 2
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
        "jelly",
        "orbit",
        "puddle",
        "rain",
        "stylus",
        "tandem",
        "turbulence",
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

# The rain and puddle cases are host-side models with translucent output,
# and the 3D jelly tank needs real spin-up time before its projection shows
# structure, so they have their own testsets below instead of the fluid
# smoke.
@testset "CPU simulation smoke: $name" for name in
                                           filter(
    n -> !(n in ("rain", "puddle", "jelly")),
    available_cases(),
)
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

    # Bodiless cases (turbulence) advertise no bounds; every case with a body
    # must keep it inside them.
    bounds = JwmWaterLily.body_bounds(simulation_case, pose_time)
    if bounds !== nothing
        xmin, xmax, ymin, ymax = bounds
        center_x, center_y = (xmin + xmax) / 2, (ymin + ymax) / 2
        @test JwmWaterLily.body_distance(simulation_case, center_x, center_y, pose_time) <
              (xmax - xmin)
    end
end

@testset "jelly wake display filter attenuates grid impulses conservatively" begin
    sigma = zeros(Float32, 5, 5, 5)
    sigma[3, 3, 3] = 1.0f0
    @test JwmWaterLily.jelly_filtered_vorticity(sigma, 2, 2, 2) == 0.52f0
    @test JwmWaterLily.jelly_filtered_vorticity(sigma, 1, 2, 2) == 0.08f0
    @test JwmWaterLily.jelly_filtered_vorticity(sigma, 2, 1, 2) == 0.08f0
    @test JwmWaterLily.jelly_filtered_vorticity(sigma, 2, 2, 1) == 0.08f0
    sigma[3, 3, 3] = Float32(NaN)
    @test JwmWaterLily.jelly_filtered_vorticity(sigma, 2, 2, 2) == 0.0f0

    # The normalized kernel preserves a constant field and remains finite at
    # the edge of Float32; summing the six neighbours before weighting would
    # overflow here and feed NaN into the wake opacity knee.
    fill!(sigma, 2.5f0)
    @test JwmWaterLily.jelly_filtered_vorticity(sigma, 2, 2, 2) ≈ 2.5f0
    fill!(sigma, floatmax(Float32))
    extreme = JwmWaterLily.jelly_filtered_vorticity(sigma, 2, 2, 2)
    @test isfinite(extreme)
    @test 0.0f0 < extreme <= floatmax(Float32)

    fill!(sigma, 0.0f0)
    sigma[3, 3, 3] = Float32(Inf)
    sigma[2, 3, 3] = Float32(-Inf)
    sigma[4, 3, 3] = -1.0f0
    @test JwmWaterLily.jelly_filtered_vorticity(sigma, 2, 2, 2) == 0.0f0
end

@testset "jelly smack pulses in a 3D tank" begin
    case = build_case("jelly", (64, 64); memory=Array)
    @test JwmWaterLily.case_palette_name(case) == "violet"
    # The tank derives from the canvas: multiples of 16, display aspect.
    @test case.domain == (32, 16, 32)
    @test JwmWaterLily.jelly_domain((1280, 800)) == (96, 32, 64)
    @test JwmWaterLily.jelly_domain((1280, 800); accelerated=true) ==
          (128, 48, 80)
    # Keep the transport's (width, vertical, depth) order observable on a
    # non-square domain; the main 64x64 fixture has nx == nz and cannot catch
    # an accidental width/height swap by itself.
    rectangular_case = build_case("jelly", (128, 64); memory=Array)
    @test rectangular_case.domain == (64, 16, 32)
    @test JwmWaterLily.frame_geometry(rectangular_case) == (64, 32, 16)
    @test length(rectangular_case.volume_rgba) == 4 * 64 * 32 * 16
    @test JwmWaterLily.jelly_volume_offset(64, 32, 1, 1, 32) == 1
    @test JwmWaterLily.jelly_volume_offset(64, 32, 64, 1, 32) == 4 * 63 + 1
    @test JwmWaterLily.jelly_volume_offset(64, 32, 1, 1, 1) ==
          4 * 64 * 31 + 1
    @test JwmWaterLily.jelly_volume_offset(64, 32, 1, 2, 32) ==
          4 * 64 * 32 + 1
    @test length(case.jellies) == 5
    @test all(isbits, case.jellies)
    # Five spatial lanes and independent smooth 3D paths keep the smack
    # distributed rather than converging into one central clump. Base heights
    # cover the safe water column instead of forming a row below the surface.
    @test issorted(getfield.(case.jellies, :x))
    @test case.jellies[end].x - case.jellies[1].x > 0.5 * case.domain[1]
    @test maximum(getfield.(case.jellies, :height)) -
          minimum(getfield.(case.jellies, :height)) > 0.15 * case.domain[3]
    for (index, jelly) in enumerate(case.jellies)
        lowest_center = 2.62f0 * jelly.radius + jelly.sway_height + 1.25f0
        highest_center =
            0.92f0 * case.domain[3] -
            1.62f0 * jelly.radius -
            jelly.sway_height -
            1.25f0
        height_fraction = mod(0.12f0 + (index - 1) * 0.618_034f0, 1.0f0)
        @test isapprox(
            jelly.height,
            lowest_center + height_fraction * (highest_center - lowest_center);
            atol=2eps(Float32),
        )
        # The bounded roaming path plus the full pulse envelope leaves the
        # tentacle tip in the tank and the bell crown submerged below 0.92nz.
        @test jelly.height - jelly.sway_height - 2.60f0 * jelly.radius > 0
        @test jelly.height + jelly.sway_height + 1.604f0 * jelly.radius <
              0.92f0 * case.domain[3]

        samples = [JwmWaterLily.jelly_center(jelly, time) for time in 0.0:2.0:80.0]
        ranges = ntuple(
            axis -> maximum(center[axis] for center in samples) -
                    minimum(center[axis] for center in samples),
            3,
        )
        @test ranges[1] > 0.5 * jelly.sway_x
        @test ranges[2] > 0.5 * jelly.sway_depth
        @test ranges[3] > 0.5 * jelly.sway_height
        center = JwmWaterLily.jelly_center(jelly, 7.0)
        @test JwmWaterLily.jelly_lateral_center(jelly, 7.0) ==
              (center[1], center[2])
    end
    @test length(unique(getfield.(case.jellies, :sway_rate_x))) == 5
    @test length(unique(getfield.(case.jellies, :sway_rate_depth))) == 5
    @test length(unique(getfield.(case.jellies, :sway_rate_height))) == 5
    @test JwmWaterLily.body_bounds(case, 0.0) === nothing
    # WaterLily passes the body into device kernels, so every captured value
    # must remain plain isbits data even when this test runs on the CPU.
    @test isbits(case.simulation.body)

    JwmWaterLily.advance!(case, 0.3)
    @test JwmWaterLily.simulation_time(case) >= 0.3
    rgba = render_rgba(case)
    @test length(rgba) == 64 * 64 * 4
    @test all(==(0xff), @view rgba[4:4:end])
    @test length(unique(Iterators.partition(rgba, 4))) > 2

    # The pulsing bells shed measurable vorticity, and the projection stays
    # inside the deterministic sub-autoscale band.
    @test maximum(case.projected) > 0
    @test maximum(case.projected) < 0.35

    # Native volume publication: tank-shaped geometry (width, vertical
    # extent, front-to-back slices) and a colorized RGBA volume.
    @test JwmWaterLily.frame_geometry(case) == (32, 32, 16)
    volume = JwmWaterLily.render_volume!(case)
    @test volume === case.volume_rgba
    @test length(volume) == 4 * 32 * 32 * 16
    alphas = @view volume[4:4:end]
    # The bells shed vorticity above the wake floor while every cell remains
    # translucent. Wake opacity is bounded below the dense anatomy range, the
    # analytic tissue feather remains continuous, and even the densest gonad
    # voxel publishes well under half opacity.
    @test maximum(alphas) > 0x00
    @test maximum(alphas) <= 0x5a
    # The published material contains the analytic bell itself, not only its
    # surrounding vorticity.  Sampling the animated crown lands inside the
    # coherent membrane density used for the stable shallow-interface cue and
    # scene refraction.
    jelly = first(case.jellies)
    τ = Float32(JwmWaterLily.simulation_time(case))
    jelly_x, jelly_depth, jelly_height = JwmWaterLily.jelly_center(jelly, τ)
    # Published voxel coordinates must match WaterLily's physical cell
    # centres, and the duplicated analytic distance must stay registered with
    # the exact AutoBody union that generated the wake.
    nx, ny, nz = case.domain
    voxel_indices = [(1, 1, 1), (nx, ny, nz)]
    append!(
        voxel_indices,
        map(case.jellies) do swimmer
            center = JwmWaterLily.jelly_center(swimmer, τ)
            return (
                clamp(round(Int, center[1] + 0.5f0), 1, nx),
                clamp(round(Int, center[2] + 0.5f0), 1, ny),
                clamp(round(Int, center[3] + 0.5f0), 1, nz),
            )
        end,
    )
    for (x, y, z) in voxel_indices
        storage_index = CartesianIndex(x + 1, y + 1, z + 1)
        physical = JwmWaterLily.WaterLily.loc(0, storage_index, Float32)
        expected = (
            JwmWaterLily.jelly_voxel_center(x),
            JwmWaterLily.jelly_voxel_center(y),
            JwmWaterLily.jelly_voxel_center(z),
        )
        @test Tuple(physical) == expected
        analytic_distance = minimum(
            JwmWaterLily.jelly_signed_distance(
                swimmer,
                physical[1],
                physical[2],
                physical[3],
                τ,
            ) for swimmer in case.jellies
        )
        @test isapprox(
            JwmWaterLily.WaterLily.sdf(case.simulation.body, physical, τ),
            analytic_distance;
            atol=2.0f-4,
        )
    end
    # At the cut rim the mouth plane wins the set-difference measurement.
    # Although its SDF depends only on Z, it must report the same lateral
    # translation as the bell or the solver sees a stationary slit while the
    # rendered rim swims sideways.
    squeeze = 1.0f0 - cos(jelly.angular_velocity * τ + jelly.phase) / 10.0f0
    heave = sin(jelly.angular_velocity * τ + jelly.phase) * jelly.radius / 4.0f0
    local_z = (cos(jelly.angular_velocity * τ + jelly.phase) - 1.0f0) *
              jelly.radius / 4.0f0
    rim_radius = sqrt(jelly.radius^2 - local_z^2) / squeeze
    rim_point = Float32[
        jelly_x + rim_radius,
        jelly_depth,
        jelly_height - heave,
    ]
    _, _, rim_velocity = JwmWaterLily.WaterLily.measure(
        case.simulation.body,
        rim_point,
        τ;
        fastd²=Float32(Inf),
    )
    delta_t = 1.0f-3
    center_before = JwmWaterLily.jelly_center(jelly, τ - delta_t)
    center_after = JwmWaterLily.jelly_center(jelly, τ + delta_t)
    center_velocity = ntuple(
        axis -> (center_after[axis] - center_before[axis]) / (2delta_t),
        3,
    )
    heave_velocity =
        cos(jelly.angular_velocity * τ + jelly.phase) *
        jelly.angular_velocity * jelly.radius / 4.0f0
    expected_rim_velocity = (
        center_velocity[1],
        center_velocity[2],
        center_velocity[3] - heave_velocity,
    )
    @test all(isapprox.(Tuple(rim_velocity), expected_rim_velocity; atol=2.0f-3))
    θ = jelly.angular_velocity * τ + jelly.phase
    heave = sin(θ) * jelly.radius / 4
    crown_z =
        jelly_height - (cos(θ) - 1) * jelly.radius / 4 - heave + jelly.radius
    @test JwmWaterLily.jelly_signed_distance(
        jelly,
        jelly_x,
        jelly_depth,
        crown_z,
        τ,
    ) < 0
    @test JwmWaterLily.jelly_surface_coverage(
        case,
        jelly_x,
        jelly_depth,
        crown_z,
        τ,
    ) > 0.5
    # Five independently curved filaments trail through the depth volume;
    # sampling the center of one strand must produce coherent material.
    tentacle_x, tentacle_y, tentacle_z, _ =
        JwmWaterLily.jelly_tentacle_center(jelly, 0, 0.45f0, τ)
    @test JwmWaterLily.jelly_tentacle_coverage(
        jelly,
        tentacle_x,
        tentacle_y,
        tentacle_z,
        τ,
    ) > 0.5
    # The rose gonad crown sits inside the bell cavity and the four thick
    # oral arms trail below the mouth; sampling their centers lands in
    # coherent material distinct from the thin filaments.
    organ_x, organ_y, organ_z = JwmWaterLily.jelly_organ_center(jelly, 0, τ)
    @test JwmWaterLily.jelly_organ_coverage(
        jelly,
        organ_x,
        organ_y,
        organ_z,
        τ,
    ) > 0.5
    arm_x, arm_y, arm_z, _ = JwmWaterLily.jelly_arm_center(jelly, 0, 0.4f0, τ)
    @test JwmWaterLily.jelly_arm_coverage(jelly, arm_x, arm_y, arm_z, τ) > 0.5
    # Quiescent water stays fully transparent so the compositor's ray-marcher
    # reveals the frosted desktop through the empty tank.
    @test count(==(0x00), alphas) > length(alphas) ÷ 2
    # Transparent voxels still carry the palette's keyed near-white midpoint.
    quiet = findfirst(==(0x00), alphas)
    @test quiet !== nothing
    quiet_base = 4 * (quiet - 1)
    @test all(>=(0xf0), volume[quiet_base + 1:quiet_base + 3])
end

@testset "publish geometry honors the planar override" begin
    @test !JwmWaterLily.planar_frames_forced()
    withenv("JWM_WATERLILY_PLANAR" => "1") do
        @test JwmWaterLily.planar_frames_forced()
    end
    withenv("JWM_WATERLILY_PLANAR" => "off") do
        @test !JwmWaterLily.planar_frames_forced()
    end
    case = build_case("jelly", (64, 64); memory=Array)
    @test JwmWaterLily.publish_geometry(case, false) == (32, 32, 16)
    @test JwmWaterLily.publish_geometry(case, true) == (64, 64, 1)
    planar_case = build_case("cylinder", (64, 64); memory=Array)
    @test JwmWaterLily.publish_geometry(planar_case, false) == (64, 64, 1)
    @test JwmWaterLily.publish_geometry(planar_case, true) == (64, 64, 1)
end

@testset "puddle ripples spread, foam, and follow the pointer" begin
    case = build_case("puddle", (128, 96); memory=Array)
    @test JwmWaterLily.case_palette_name(case) == "ocean"
    @test case.grid == (64, 48)

    # A splash disturbs the surface and the ring spreads outward over time.
    JwmWaterLily.splash!(case, 32.0, 24.0, 2.5, 4.0)
    JwmWaterLily.advance!(case, 0.2)
    @test JwmWaterLily.simulation_time(case) ≈ 0.2
    disturbed = maximum(abs, case.height)
    @test disturbed > 0

    # Render while the ring is still inside the small test pond — it crosses
    # the open boundaries and gets absorbed within another half second.
    rgba = render_rgba(case)
    @test length(rgba) == 128 * 96 * 4
    # The whole pond is a water lens: every pixel stays translucent.
    @test all(<(0xf7), @view rgba[4:4:end])
    @test length(unique(Iterators.partition(rgba, 4))) > 2

    far_before = abs(case.height[56, 24])
    JwmWaterLily.advance!(case, 0.25)
    @test abs(case.height[56, 24]) != far_before

    # Pointer wakes only start once a previous event defines the stroke;
    # splashes stamp the surface height directly.
    energy_before = sum(abs2, case.height)
    JwmWaterLily.handle_pointer!(case, 0.3, 0.5)
    @test sum(abs2, case.height) ≈ energy_before
    JwmWaterLily.handle_pointer!(case, 0.6, 0.5)
    @test sum(abs2, case.height) > energy_before

    achieved = JwmWaterLily.advance_budgeted!(
        case,
        0.1,
        time_ns() + UInt64(30_000_000_000),
    )
    @test achieved ≈ 0.1

    # Keep a small high-frequency component alive for long enough to catch
    # timestep-pattern instabilities. At 30 fps the old remainder stepping
    # alternated 7.5 ms and 3.3 ms updates: a normal splash stayed plausible
    # for about three seconds, then exploded into an almost entirely opaque
    # foam frame.
    stability_case = build_case("puddle", (128, 96); memory=Array)
    JwmWaterLily.splash!(stability_case, 32.0, 24.0, 2.5, 4.0)
    for _ in 1:90
        JwmWaterLily.advance!(stability_case, 1 / 30)
    end
    @test all(isfinite, stability_case.height)
    @test all(isfinite, stability_case.rate)
    @test maximum(abs, stability_case.height) < 2
    @test maximum(abs, stability_case.rate) < 100

    stable_rgba = render_rgba(stability_case)
    stable_alpha = @view stable_rgba[4:4:end]
    @test count(>=(0xc0), stable_alpha) < length(stable_alpha) ÷ 20
    stable_pixels = eachcol(reshape(stable_rgba, 4, :))
    pale_pixels = count(
        pixel -> pixel[1] >= 0xc0 && pixel[2] >= 0xc0 && pixel[3] >= 0x98,
        stable_pixels,
    )
    @test pale_pixels < length(stable_alpha) ÷ 20
end

@testset "turbulence publishes sparse signed 3D vortices" begin
    @test JwmWaterLily.turbulence_domain((64, 64)) == (32, 16, 32)
    @test JwmWaterLily.turbulence_domain((128, 64)) == (64, 16, 32)
    @test JwmWaterLily.turbulence_domain((1280, 800)) == (96, 32, 64)
    @test JwmWaterLily.turbulence_domain((1280, 800); accelerated=true) ==
          (128, 48, 80)

    # A rectangular fixture makes every transport axis independently
    # observable: solver (x, depth, vertical) becomes frame
    # (width, height, front-to-back slices).
    Random.seed!(0x4a574d5455524233)
    case = build_case("turbulence", (128, 64); memory=Array)
    @test JwmWaterLily.case_palette_name(case) == "fluent"
    @test JwmWaterLily.body_bounds(case, 0.0) === nothing
    @test case.domain == (64, 16, 32)
    @test JwmWaterLily.frame_geometry(case) == (64, 32, 16)
    @test JwmWaterLily.publish_geometry(case, false) == (64, 32, 16)
    @test JwmWaterLily.publish_geometry(case, true) == (128, 64, 1)
    @test length(case.volume_rgba) == 4 * 64 * 32 * 16

    @test JwmWaterLily.turbulence_volume_offset(64, 32, 1, 1, 32) == 1
    @test JwmWaterLily.turbulence_volume_offset(64, 32, 64, 1, 32) ==
          4 * 63 + 1
    @test JwmWaterLily.turbulence_volume_offset(64, 32, 1, 1, 1) ==
          4 * 64 * 31 + 1
    @test JwmWaterLily.turbulence_volume_offset(64, 32, 1, 2, 32) ==
          4 * 64 * 32 + 1

    # The simulation is genuinely three-dimensional: its staggered velocity
    # field has three spatial axes and all three components start alive.
    velocity = case.simulation.flow.u
    @test ndims(velocity) == 4
    @test size(velocity, 4) == 3
    component_energy = ntuple(3) do component
        sum(abs2, @view velocity[:, :, :, component])
    end
    @test all(>(0), component_energy)

    # Both display metrics must overwrite the complete reused scalar scratch,
    # including all periodic ghost planes. A stale ghost feeds directly into
    # the seven-point reconstruction at the tank boundary.
    sentinel = 1234.0f0
    fill!(case.simulation.flow.σ, sentinel)
    magnitude, signed = JwmWaterLily.download_turbulence_vorticity!(case)
    for field in (magnitude, signed)
        @test all(!=(sentinel), field)
        @views begin
            @test field[1, :, :] == field[end - 1, :, :]
            @test field[end, :, :] == field[2, :, :]
            @test field[:, 1, :] == field[:, end - 1, :]
            @test field[:, end, :] == field[:, 2, :]
            @test field[:, :, 1] == field[:, :, end - 1]
            @test field[:, :, end] == field[:, :, 2]
        end
    end

    # The first pointer event only anchors the stroke; the second stirs a
    # depth-localized 3D dipole in along the motion.
    energy_before = sum(abs2, velocity)
    JwmWaterLily.handle_pointer!(case, 0.3, 0.5)
    @test sum(abs2, velocity) ≈ energy_before
    JwmWaterLily.handle_pointer!(case, 0.7, 0.5)
    @test sum(abs2, velocity) > energy_before

    # Edge injections move the complete Gaussian pair inward instead of
    # clipping a core against the tile boundary, and refresh all velocity
    # ghosts before the same-frame vorticity render.
    JwmWaterLily.inject_dipole!(case, 0.5, 0.5, 0.5, 1.0, 0.0, 0.5)
    @views begin
        @test velocity[1, :, :, :] == velocity[end - 1, :, :, :]
        @test velocity[end, :, :, :] == velocity[2, :, :, :]
        @test velocity[:, 1, :, :] == velocity[:, end - 1, :, :]
        @test velocity[:, end, :, :] == velocity[:, 2, :, :]
        @test velocity[:, :, 1, :] == velocity[:, :, end - 1, :]
        @test velocity[:, :, end, :] == velocity[:, :, 2, :]
    end
    @test all(isfinite, velocity)

    # Prefilling the alpha lane catches a renderer that skips quiet voxels
    # and leaks uninitialized data (or anatomy from the preceding jelly
    # frame) into a geometry-compatible volume.
    @views case.volume_rgba[4:4:end] .= 0xff
    volume = JwmWaterLily.render_volume!(case)
    @test volume === case.volume_rgba
    @test length(volume) == 4 * prod(JwmWaterLily.frame_geometry(case))
    first_volume = copy(volume)
    alphas = first_volume[4:4:end]
    @test all(<=(JwmWaterLily.TURBULENCE_MAX_ALPHA), alphas)
    @test any(==(0x00), alphas)
    @test any(>(0x00), alphas)

    # Signed depth-axis vorticity must exercise both halves of the Fluent
    # palette. Only occupied voxels count; zero-alpha RGB deliberately keeps
    # its local hue for stable tricubic reconstruction at filament edges.
    pixels = reshape(first_volume, 4, :)
    active = findall(>(0x00), alphas)
    @test any(index -> pixels[3, index] > pixels[1, index], active)
    @test any(index -> pixels[1, index] > pixels[3, index], active)

    # At least two front-to-back slabs differ, ruling out a replicated planar
    # field masquerading as a version-2 volume.
    frame_width, frame_height, frame_depth = JwmWaterLily.frame_geometry(case)
    slice_bytes = 4 * frame_width * frame_height
    front_slice = @view first_volume[1:slice_bytes]
    depth_varies = any(2:frame_depth) do depth
        start = (depth - 1) * slice_bytes + 1
        @view(first_volume[start:(start + slice_bytes - 1)]) != front_slice
    end
    @test depth_varies

    # Rendering an unchanged CPU field is byte-deterministic. Palette
    # hot-swapping changes occupied emission colors but never the physical
    # opacity transfer used by the compositor's wake branch.
    @test copy(JwmWaterLily.render_volume!(case)) == first_volume
    violet_volume = copy(
        JwmWaterLily.render_volume!(
            case;
            palette=JwmWaterLily.VIOLET_PALETTE,
        ),
    )
    @test violet_volume[4:4:end] == alphas
    violet_pixels = reshape(violet_volume, 4, :)
    @test any(active) do index
        violet_pixels[1:3, index] != pixels[1:3, index]
    end

    # The ambient reseed fires once its deadline passes.
    case.reseed_time = -1.0
    energy_before = sum(abs2, velocity)
    JwmWaterLily.frame_tick!(case)
    @test sum(abs2, velocity) != energy_before
    @test case.reseed_time > JwmWaterLily.simulation_time(case)

    JwmWaterLily.advance!(case, 0.02)
    @test JwmWaterLily.simulation_time(case) >= 0.02
    @test all(isfinite, velocity)
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
    critical = JwmWaterLily.rain_critical_radius(1.0f0) * case.resolution
    runner = JwmWaterLily.rain_stuck_drop(case, 64.0, 10.0, 1.4f0 * critical)
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

# The drop model is a force balance in SI units rather than an animation
# curve, so its thresholds and rates are pinned against the physics they
# claim to implement instead of against a rendered look.
@testset "rain drop physics" begin
    # Spherical-cap geometry: an impacting sphere of diameter D relaxes into
    # a cap of the same volume.
    for diameter in (0.8f-3, 1.5f-3, 2.5f-3)
        radius = JwmWaterLily.rain_impact_radius(diameter)
        @test JwmWaterLily.rain_drop_volume(radius) ≈ Float32(pi / 6) * diameter^3 rtol =
            1.0f-5
    end
    volume = JwmWaterLily.rain_drop_volume(2.0f-3)
    @test JwmWaterLily.rain_radius_from_volume(volume) ≈ 2.0f-3 rtol = 1.0f-5

    # Furmidge: the critical contact radius is where gravity exactly balances
    # the retention force, and for water on glass it lands near 2 mm.
    critical = JwmWaterLily.rain_critical_radius(1.0f0)
    @test JwmWaterLily.rain_gravity_force(critical) ≈
          JwmWaterLily.rain_retention_force(critical, 1.0f0) rtol = 1.0f-5
    @test 1.5f-3 < critical < 3.0f-3
    # Stickier glass holds bigger drops.
    @test JwmWaterLily.rain_critical_radius(1.3f0) > critical

    # Terminal velocity from the Cox-Voinov drag: measured slide speeds of
    # millimetric drops on vertical glass are centimetres per second, and
    # they grow with drop size.
    terminal(a) =
        (JwmWaterLily.rain_gravity_force(a) - JwmWaterLily.rain_retention_force(a, 1.0f0)) /
        JwmWaterLily.rain_drag_coefficient(a)
    @test terminal(3.0f-3) < terminal(4.0f-3)
    @test 0.02f0 < terminal(3.0f-3) < 0.30f0

    case = build_case("rain", (192, 800); memory=Array)
    # Pin the defect landscape so the threshold under test is the physical
    # one rather than a sample of the field.
    fill!(case.pinning, 1.0f0)
    fill!(case.wetness, 0.0f0)
    empty!(case.drops)
    critical_px = critical * case.resolution
    spawned = JwmWaterLily.Raindrop[]

    # Just under the threshold the drop hangs; just over it, it releases on
    # its own. Nothing sets `sliding` — the force balance does.
    held = JwmWaterLily.rain_stuck_drop(case, 96.0, 100.0, 0.92f0 * critical_px)
    held.pinning = 1.0f0
    JwmWaterLily.rain_dynamics!(case, held, spawned, 0.05f0)
    @test !held.sliding
    @test held.speed == 0.0f0
    @test held.y == 100.0f0

    released = JwmWaterLily.rain_stuck_drop(case, 96.0, 100.0, 1.15f0 * critical_px)
    released.pinning = 1.0f0
    for _ in 1:20
        JwmWaterLily.rain_dynamics!(case, released, spawned, 0.05f0)
    end
    @test released.sliding
    @test released.speed > 0.0f0
    @test released.y > 100.0f0
    # The swept contact area wipes the mist over the full contact width.
    @test case.wetness[96, 105] > 0.5f0
    @test case.wetness[
        clamp(96 + round(Int, 2.5f0 * released.radius), 1, 192),
        105,
    ] < 0.2f0

    # Contact-angle hysteresis is a loop: a drop that drains back under the
    # retention force re-pins by itself instead of coasting forever.
    draining = JwmWaterLily.rain_stuck_drop(case, 40.0, 100.0, 0.7f0 * critical_px)
    draining.pinning = 1.0f0
    draining.speed = 300.0f0
    draining.sliding = true
    for _ in 1:120
        JwmWaterLily.rain_dynamics!(case, draining, spawned, 1.0f0 / 60.0f0)
    end
    @test !draining.sliding
    @test draining.speed == 0.0f0

    # Coalescence conserves volume and mixes momentum.
    empty!(case.drops)
    big = JwmWaterLily.rain_stuck_drop(case, 96.0, 300.0, 0.8f0 * critical_px)
    small = JwmWaterLily.rain_stuck_drop(case, 96.0 + 0.9f0 * critical_px, 300.0,
                                         0.5f0 * critical_px)
    big.speed = 200.0f0
    total = JwmWaterLily.rain_drop_volume(big.radius / case.resolution) +
            JwmWaterLily.rain_drop_volume(small.radius / case.resolution)
    append!(case.drops, (big, small))
    JwmWaterLily.rain_merge!(case, spawned)
    survivors = filter(d -> d.radius > 0.0f0, case.drops)
    @test length(survivors) == 1
    merged = only(survivors)
    @test JwmWaterLily.rain_drop_volume(merged.radius / case.resolution) ≈ total rtol =
        1.0f-4
    @test 0.0f0 < merged.speed < 200.0f0

    # Coalescence tests the segment a drop swept this substep, so a fast
    # runner absorbs what it passes instead of tunnelling through it.
    empty!(case.drops)
    runner = JwmWaterLily.rain_stuck_drop(case, 96.0, 400.0, 1.2f0 * critical_px)
    runner.previous_y = 200.0f0
    victim = JwmWaterLily.rain_stuck_drop(case, 96.0, 300.0, 0.4f0 * critical_px)
    append!(case.drops, (runner, victim))
    JwmWaterLily.rain_merge!(case, spawned)
    @test victim.radius == 0.0f0
    @test runner.radius > 1.2f0 * critical_px

    # Marshall-Palmer: a gust raises the rain rate, which both flattens the
    # size distribution (smaller Λ, so bigger drops) and lifts the impact
    # flux. The phases below pin the gust to its trough and its crest.
    gusting(phases) = JwmWaterLily.RainCase(
        case.dimensions,
        JwmWaterLily.Raindrop[],
        case.wetness,
        case.grain,
        case.nucleation,
        case.pinning,
        case.pinning_step,
        Ref(0.0),
        phases,
        case.resolution,
    )
    calm_rate, calm_lambda = JwmWaterLily.rain_impact_flux(gusting((pi / 2, -pi / 2)), 0.0)
    heavy_rate, heavy_lambda = JwmWaterLily.rain_impact_flux(gusting((pi / 2, pi / 2)), 0.0)
    @test calm_rate > 0.0f0
    @test heavy_lambda < calm_lambda
    @test heavy_rate > calm_rate
end
