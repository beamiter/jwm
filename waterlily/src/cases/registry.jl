const CASE_REGISTRY = Dict{String,Function}(
    "hover" => build_hover_case,
    "cylinder" => build_cylinder_case,
    "dance" => build_dance_case,
    "flap" => build_flap_case,
    "tandem" => build_tandem_case,
    "diamond" => build_diamond_case,
    "orbit" => build_orbit_case,
)

available_cases() = sort!(collect(keys(CASE_REGISTRY)))

function build_case(
    name::AbstractString,
    dimensions::Tuple{Int,Int};
    memory=Array,
)
    factory = get(CASE_REGISTRY, String(name), nothing)
    factory === nothing &&
        throw(
            ArgumentError(
                "unknown case '$name'; available cases: $(join(available_cases(), ", "))",
            ),
        )
    return factory(dimensions; memory)
end
