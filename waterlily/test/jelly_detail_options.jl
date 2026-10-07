using JwmWaterLily
using Test

@testset "bounded jelly display detail option" begin
    @test parse_cli(String[]).jelly_detail == 2
    @test parse_cli(["--jelly-detail", "1"]).jelly_detail == 1
    @test parse_cli(["--jelly-detail=2"]).jelly_detail == 2
    # It is retained even when the worker starts with another case so a later
    # compositor case switch uses the same display setting.
    options = parse_cli(["--case", "hover", "--jelly-detail", "1"])
    @test options.case_name == "hover"
    @test options.jelly_detail == 1
    @test options.simulation_size == parse_cli(String[]).simulation_size

    for value in ("0", "3", "-1", "detail", "1.5", "2.0", "NaN", "Inf", "999999999999999999999999999999")
        @test_throws ArgumentError parse_cli(["--jelly-detail", value])
    end
    @test_throws ArgumentError parse_cli(["--jelly-detail"])
    @test_throws ArgumentError parse_cli(["--jelly-detail="])
    @test_throws ArgumentError parse_cli(["--jelly-detail", "--case", "jelly"])
    @test occursin("--jelly-detail", sprint(JwmWaterLily.usage))

    legacy_options = RunnerOptions(
        "jelly", :cpu, 30.0, "/tmp/jelly.sock", "/tmp/jelly.frame", (64, 64), (64, 64),
    )
    @test legacy_options.jelly_detail == 2
end

@testset "jelly detail is routed only to the jelly builder" begin
    registry = JwmWaterLily.CASE_REGISTRY
    original_factories = copy(registry)
    try
        # Strict keyword signatures catch accidental display-scale forwarding
        # to any of the existing non-jelly case constructors.
        registry["jelly"] = (dimensions; memory, display_scale) ->
            (; dimensions, memory, display_scale)
        for name in keys(registry)
            name == "jelly" && continue
            registry[name] = (dimensions; memory) -> (; dimensions, memory)
        end

        @test build_case("jelly", (64, 64)).display_scale == 2
        for detail in (1, 2)
            options = parse_cli(["--case", "hover", "--sim-size", "128x64", "--jelly-detail", string(detail)])
            for name in available_cases()
                result = build_case(
                    name, options.simulation_size; memory=Array, jelly_detail=options.jelly_detail,
                )
                @test result.dimensions == (128, 64)
                @test result.memory === Array
                if name == "jelly"
                    @test result.display_scale == detail
                else
                    @test !hasproperty(result, :display_scale)
                end
            end
        end
    finally
        empty!(registry)
        merge!(registry, original_factories)
    end
end
