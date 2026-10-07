@testset "jelly display detail leaves the actual solver unchanged" begin
    Random.seed!(7651)
    coarse = JwmWaterLily.build_jelly_case((128, 64); display_scale=1)
    Random.seed!(7651)
    fine = JwmWaterLily.build_jelly_case((128, 64); display_scale=2)
    @test coarse.domain == fine.domain == (64, 16, 32)
    @test coarse.display_domain == coarse.domain
    @test fine.display_domain == 2 .* fine.domain
    @test coarse.jellies == fine.jellies
    @test size(coarse.simulation.flow.u) == size(fine.simulation.flow.u)
    @test size(fine.wake_filtered) == fine.domain
    @test length(fine.volume_rgba) == 8 * length(coarse.volume_rgba)
    @test JwmWaterLily.frame_geometry(fine) == (128, 64, 32)
    @test_throws ArgumentError JwmWaterLily.build_jelly_case((128, 64); display_scale=0)
    @test_throws ArgumentError JwmWaterLily.build_jelly_case((128, 64); display_scale=3)
    # Worst supported accelerated aspect/domain remains under the reader's
    # 64 MiB color-plane budget. Both material planes together are bounded.
    largest = JwmWaterLily.jelly_domain((4096, 4096); accelerated=true)
    @test 4 * prod(2 .* largest) < 64 * 1024^2
    @test 4 * prod((448, 128, 192)) < 64 * 1024^2
    for t in (0.0f0, 0.7f0, 6.1f0), (a, b) in zip(coarse.jellies, fine.jellies)
        @test JwmWaterLily.jelly_center(a, t) == JwmWaterLily.jelly_center(b, t)
    end
end

@testset "jelly wake sampling preserves coordinates and constants" begin
    field = Array{Float32}(undef, 5, 4, 3)
    for z in 1:3, y in 1:4, x in 1:5
        field[x, y, z] = (x - 0.5f0) + 2.0f0 * (y - 0.5f0) + 3.0f0 * (z - 0.5f0)
        @test JwmWaterLily.jelly_voxel_center(2x) / 2 == x - 0.25f0
    end
    for x in (0.5f0, 1.25f0, 4.5f0), y in (0.5f0, 2.25f0, 3.5f0), z in (0.5f0, 1.75f0, 2.5f0)
        @test JwmWaterLily.jelly_sample_wake(field, x, y, z) ≈ x + 2y + 3z
    end
    @test JwmWaterLily.jelly_sample_wake(field, -9.0f0, -9.0f0, -9.0f0) == field[1, 1, 1]
    @test JwmWaterLily.jelly_sample_wake(field, 99.0f0, 99.0f0, 99.0f0) == field[end, end, end]
    fill!(field, 2.5f0)
    @test JwmWaterLily.jelly_sample_wake(field, 1.25f0, 2.75f0, 0.8f0) ≈ 2.5f0
end

@testset "jelly optical depth and analytic footprint do not dilate" begin
    for alpha in (0.0f0, 0.02f0, 0.15f0, 0.45f0, 1.0f0), scale in (1, 2)
        sampled = JwmWaterLily.jelly_display_alpha(alpha, scale)
        @test 0.0f0 <= sampled <= 1.0f0
        @test (1.0f0 - sampled)^scale ≈ 1.0f0 - alpha atol=2.0f-7
    end
    for width in (0.5f0, 1.0f0)
        @test JwmWaterLily.jelly_coverage(0.0f0, width) == 0.5f0
        @test JwmWaterLily.jelly_coverage(width, width) == 0.0f0
        @test JwmWaterLily.jelly_coverage(-width, width) == 1.0f0
        for distance in (-0.3f0, -0.1f0, 0.1f0, 0.3f0)
            @test JwmWaterLily.jelly_coverage(distance, width) +
                  JwmWaterLily.jelly_coverage(-distance, width) ≈ 1.0f0
        end
    end
    case = JwmWaterLily.build_jelly_case((64, 64))
    pose = JwmWaterLily.jelly_pose(first(case.jellies), 0.0f0)
    @test length(pose.strand_dir) == 12
    @test length(unique(pose.strand_dir)) == 12
    # Neither finer display sampling nor a static rerender changes geometry.
    @test JwmWaterLily.pose_tentacle_center(pose, 11, 0.6f0) ==
          JwmWaterLily.pose_tentacle_center(pose, 11, 0.6f0)
    JwmWaterLily.render_volume!(case)
    rgba = copy(case.volume_rgba)
    material = copy(case.volume_material)
    JwmWaterLily.render_volume!(case)
    @test rgba == case.volume_rgba
    @test material == case.volume_material
    @test count(==(0x00), @view rgba[4:4:end]) > length(rgba) ÷ 8
    @test any(==(0xff), @view material[4:4:end])
end

@testset "jelly shell normals are coherent across both membrane walls" begin
    case = JwmWaterLily.build_jelly_case((1280, 800))
    for time in (0.0f0, 0.5f0, 3.0f0)
        pose = JwmWaterLily.jelly_pose(first(case.jellies), time)
        poses = [pose]
        crown = pose.center_z - pose.axis_shift + pose.spec.radius
        # Inner and outer shell samples share the outward ellipsoid normal;
        # density-gradient normals would have opposite signs here.
        normals = [JwmWaterLily.jelly_bell_normal(
            poses, pose.center_x, pose.center_y, crown + offset, 0.5f0,
        ) for offset in (-0.12f0, 0.12f0)]
        @test all(n -> n[2] > 0.0f0, normals)
        @test all(n -> abs(n[1]) < 1.0f-5 && abs(n[3]) < 1.0f-5, normals)
        @test all(n -> all(isfinite, n), normals)
        # Long display appendages remain within the reserved 2.60R envelope.
        for strand in 0:11, fraction in (0.0f0, 0.5f0, 1.0f0)
            x, y, z, radius = JwmWaterLily.pose_tentacle_center(pose, strand, fraction)
            @test pose.z_lo <= z - radius <= pose.z_hi
            @test (x - pose.center_x)^2 + (y - pose.center_y)^2 < pose.reach_sq
            @test z >= pose.mouth_z - 2.60f0 * pose.spec.radius
        end
    end
end
