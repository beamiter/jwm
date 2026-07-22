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
    @test length(JwmWaterLily.ALL_PALETTES) == 8
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
    @test available_cases() ==
          ["cylinder", "dance", "diamond", "flap", "hover", "orbit", "tandem", "wander"]
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

@testset "CPU simulation smoke: $name" for name in available_cases()
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
