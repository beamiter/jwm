using JwmWaterLily
using Test

jelly_wire_u32(bytes, offset) = ltoh(reinterpret(UInt32, bytes[offset + 1:offset + 4])[1])
jelly_wire_u64(bytes, offset) = ltoh(reinterpret(UInt64, bytes[offset + 1:offset + 8])[1])
jelly_test_pose() = Float32[0.1, -0.2, 0.3, 0.08, 1.1, -2.0, 0.01, -0.1]

@testset "legacy headers keep the version-1/2/3 byte contract" begin
    for (depth, material, version) in ((1, false, 1), (2, false, 2), (2, true, 3))
        header = JwmWaterLily.frame_header(2, 3, 8, 1, 7, 1234; depth, material_aux=material)
        @test length(header) == (depth == 1 ? 64 : 96)
        @test jelly_wire_u32(header, 8) == version
        @test jelly_wire_u32(header, 44) == 1
        @test jelly_wire_u64(header, 48) == 7
        if depth > 1
            @test jelly_wire_u32(header, 64) == depth
            @test jelly_wire_u32(header, 68) == Int(material)
            @test all(iszero, @view(header[73:96]))
        end
    end
end

@testset "jelly poses are per-slot bounded little-endian data" begin
    mktempdir() do directory
        for count in (1, 5), detail in (1, 2)
            path = joinpath(directory, "frame-$count-$detail")
            publisher = FramePublisher(path, 2, 1; depth=2, material_aux=true,
                                       jelly_count=count, jelly_detail=detail)
            color = fill(UInt8(0x40), 16)
            material = fill(UInt8(0x80), 16)
            first = repeat(jelly_test_pose(), count)
            second = copy(first)
            second[1] = -0.4f0
            @test publish!(publisher, color, material, first, 123) == 1
            @test publish!(publisher, reverse(color), reverse(material), second, 456) == 2
            close(publisher)

            bytes = read(path)
            @test jelly_wire_u32(bytes, 8) == 4
            @test jelly_wire_u32(bytes, 12) == 96
            @test jelly_wire_u32(bytes, 64) == 2
            @test jelly_wire_u32(bytes, 68) == 1
            @test jelly_wire_u32(bytes, 72) == count
            @test jelly_wire_u32(bytes, 76) == 32
            @test jelly_wire_u32(bytes, 80) == 1
            @test jelly_wire_u32(bytes, 84) == detail
            @test all(iszero, @view(bytes[89:96]))
            @test jelly_wire_u32(bytes, 44) == 1
            @test jelly_wire_u64(bytes, 48) == 2
            @test jelly_wire_u64(bytes, 56) == 456
            slot_bytes = 32 + 32count
            @test length(bytes) == 96 + 2slot_bytes
            for (slot, poses) in ((0, first), (1, second))
                base = 96 + slot * slot_bytes
                @test bytes[base + 1:base + 16] == color
                @test bytes[base + 17:base + 32] == material
                decoded = reinterpret(Float32, ltoh.(reinterpret(UInt32, bytes[base + 33:base + slot_bytes])))
                @test decoded == poses
            end
            @test (stat(path).mode & 0o077) == 0
        end
    end
end

@testset "invalid jelly metadata and parameters never publish" begin
    mktempdir() do directory
        path = joinpath(directory, "frame")
        for (depth, material, count, detail) in (
            (1, true, 5, 2), (2, false, 5, 2), (2, true, -1, 2),
            (2, true, 6, 2), (2, true, 5, 0), (2, true, 5, 3), (2, true, 0, 2),
        )
            @test_throws ArgumentError FramePublisher(path, 2, 1; depth,
                material_aux=material, jelly_count=count, jelly_detail=detail)
        end
        publisher = FramePublisher(path, 2, 1; depth=2, material_aux=true,
                                   jelly_count=1, jelly_detail=2)
        color, material = fill(UInt8(0x40), 16), fill(UInt8(0x80), 16)
        @test publish!(publisher, color, material, jelly_test_pose(), 1) == 1
        unchanged = read(path)
        @test_throws ArgumentError publish!(publisher, color, material, 2)
        @test_throws DimensionMismatch publish!(publisher, color, material, Float32[], 2)
        @test_throws DimensionMismatch publish!(publisher, color, material, repeat(jelly_test_pose(), 2), 2)
        @test_throws DimensionMismatch publish!(publisher, color[1:15], material, jelly_test_pose(), 2)
        @test_throws DimensionMismatch publish!(publisher, color, material[1:15], jelly_test_pose(), 2)
        @test_throws ArgumentError publish!(publisher, color, material, jelly_test_pose(), -1)
        for index in 1:8, value in (Float32(NaN), Float32(Inf), -Float32(Inf))
            invalid = jelly_test_pose()
            invalid[index] = value
            @test_throws ArgumentError publish!(publisher, color, material, invalid, 2)
        end
        for (index, value) in ((1, 0.51), (2, -0.51), (3, 0.51), (4, 0.0),
            (4, -0.01), (4, 0.251), (5, 0.79), (5, 1.21), (6, 6.3),
            (6, -6.3), (7, 0.251), (7, -0.251), (8, 0.501), (8, -0.501))
            invalid = jelly_test_pose()
            invalid[index] = value
            @test_throws ArgumentError publish!(publisher, color, material, invalid, 2)
        end
        @test publisher.sequence == 1
        @test read(path) == unchanged
        close(publisher)
    end
end

@testset "wake command parser is bounded and requires complete lines" begin
    stream = IOBuffer("capabilities jelly-pose-v1\n case jelly \r\n\ncase next")
    @test JwmWaterLily.read_wake_command(stream) == "capabilities jelly-pose-v1"
    @test JwmWaterLily.read_wake_command(stream) == "case jelly"
    @test JwmWaterLily.read_wake_command(stream) == ""
    @test JwmWaterLily.read_wake_command(stream) === nothing
    @test JwmWaterLily.read_wake_command(IOBuffer("")) === nothing
    @test JwmWaterLily.read_wake_command(IOBuffer("x"^256 * "\n")) == "x"^256
    @test_throws ArgumentError JwmWaterLily.read_wake_command(IOBuffer("x"^257 * "\n"))
    @test_throws ArgumentError JwmWaterLily.read_wake_command(IOBuffer("x"^257))
end

@testset "jelly negotiation belongs only to the current wake connection" begin
    client = JwmWaterLily.WakeClient("/unused-jelly-pose-test.sock")
    @test !client.jelly_pose_supported
    @test !JwmWaterLily.apply_capabilities!(client, "case jelly")
    @test !JwmWaterLily.apply_capabilities!(client, "")
    @test JwmWaterLily.apply_capabilities!(client, "capabilities jelly-pose-v10")
    @test !client.jelly_pose_supported
    @test JwmWaterLily.apply_capabilities!(client, "capabilities unknown jelly-pose-v1")
    @test client.jelly_pose_supported
    @test JwmWaterLily.take_command!(client) === nothing
    @test JwmWaterLily.apply_capabilities!(client, "capabilities jelly-pose-v0")
    @test !client.jelly_pose_supported
    @test JwmWaterLily.apply_capabilities!(client, "capabilities")
    @test !client.jelly_pose_supported
    JwmWaterLily.apply_capabilities!(client, "capabilities jelly-pose-v1")
    put!(client.commands, "case next")
    old_commands = client.commands
    JwmWaterLily.disconnect!(client)
    @test !client.jelly_pose_supported
    @test client.stream === nothing
    @test !isopen(old_commands)
    @test JwmWaterLily.take_command!(client) === nothing
    @test !notify!(client; allow_reconnect=false)
    @test !client.jelly_pose_supported
    close(client)
end

@testset "equal-size pose mode changes replace files and retain sequence" begin
    mktempdir() do directory
        path = joinpath(directory, "frame")
        publisher = FramePublisher(path, 2, 1; depth=2, material_aux=true)
        color, material = fill(UInt8(0x40), 16), fill(UInt8(0x80), 16)
        @test publish!(publisher, color, material, 1) == 1
        legacy = (; geometry=(2, 1, 2), jelly_count=0, jelly_detail=0)
        @test JwmWaterLily.reconfigure_publisher(publisher, legacy) === publisher
        unchanged = read(path)
        @test_throws ArgumentError JwmWaterLily.reconfigure_publisher(publisher,
            (; geometry=(2, 1, 2), jelly_count=6, jelly_detail=1))
        @test !publisher.closed
        @test read(path) == unchanged
        modern = (; geometry=(2, 1, 2), jelly_count=1, jelly_detail=1)
        previous = publisher
        publisher = JwmWaterLily.reconfigure_publisher(publisher, modern)
        @test previous.closed
        @test publisher.sequence == 1
        @test jelly_wire_u32(read(path), 8) == 4
        @test publish!(publisher, color, material, jelly_test_pose(), 2) == 2
        @test !JwmWaterLily.remove_owned_frame_file!(previous)
        publisher = JwmWaterLily.reconfigure_publisher(publisher,
            (; modern..., jelly_detail=2))
        @test publisher.sequence == 2
        @test publisher.jelly_detail == 2
        publisher = JwmWaterLily.reconfigure_publisher(publisher, legacy)
        @test publisher.sequence == 2
        @test publisher.jelly_count == 0
        @test jelly_wire_u32(read(path), 8) == 3
        @test publish!(publisher, color, material, 3) == 3
        publisher = JwmWaterLily.reconfigure_publisher(publisher,
            (; geometry=(2, 1, 1), jelly_count=0, jelly_detail=0))
        @test publish!(publisher, color[1:8], 4) == 4
        @test jelly_wire_u32(read(path), 8) == 1
        close(publisher)
    end
end

@testset "hot case layouts gate poses and keep the planar escape hatch" begin
    jelly = build_case("jelly", (64, 64); memory=Array, jelly_detail=1)
    cylinder = build_case("cylinder", (64, 64); memory=Array)
    legacy = JwmWaterLily.publish_layout(jelly, false, false, 1)
    @test legacy.geometry == JwmWaterLily.frame_geometry(jelly)
    @test legacy.jelly_count == 0
    for detail in (1, 2)
        modern = JwmWaterLily.publish_layout(jelly, false, true, detail)
        @test modern.geometry == JwmWaterLily.jelly_wake_geometry(jelly)
        @test modern.jelly_count == 5
        @test modern.jelly_detail == detail
        planar = JwmWaterLily.publish_layout(jelly, true, true, detail)
        @test planar.geometry == (64, 64, 1)
        @test planar.jelly_count == 0
        other = JwmWaterLily.publish_layout(cylinder, false, true, detail)
        @test other.geometry == (64, 64, 1)
        @test other.jelly_count == 0
        @test other.jelly_detail == 0
    end
    @test_throws ArgumentError JwmWaterLily.publish_layout(jelly, false, true, 3)
end
