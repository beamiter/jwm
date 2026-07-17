using JwmWaterLily
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

@testset "CPU hover simulation smoke" begin
    simulation_case = build_case("hover", (64, 64); memory=Array)
    JwmWaterLily.advance!(simulation_case, 0.01)
    rgba = render_rgba(simulation_case)

    @test length(rgba) == 64 * 64 * 4
    @test all(==(0xff), @view rgba[4:4:end])
    @test length(unique(Iterators.partition(rgba, 4))) > 2
end
