struct DiamondCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    center::SVector{2,T}
    half_side::T
end

"""
A square prism rotated 45 degrees to the stream. The sharp upstream edges fix
the separation points, so the street is wider and more angular than the one a
smooth cylinder sheds.
"""
function build_diamond_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=300,
)
    width, height = dimensions
    T = Float32
    half_side = max(T(3), T(height * 0.055))
    # The frontal projection of the rotated square is its full diagonal.
    frontal_height = half_side * T(2) * sqrt(T(2))
    center = SA[T(width * 0.26), T(height * 0.50) + T(0.6)]

    diamond_signed_distance = let center=center, half_side=half_side
        function (x, _time)
            dx = x[1] - center[1]
            dy = x[2] - center[2]
            # Exact box distance evaluated in the 45-degree rotated frame.
            inv_sqrt2 = sqrt(eltype(dx)(0.5))
            u = abs((dx + dy) * inv_sqrt2) - half_side
            v = abs((dy - dx) * inv_sqrt2) - half_side
            outside_u = max(u, zero(u))
            outside_v = max(v, zero(v))
            outside = sqrt(outside_u * outside_u + outside_v * outside_v)
            inside = min(max(u, v), zero(u))
            return outside + inside
        end
    end

    body = WaterLily.AutoBody(diamond_signed_distance)
    viscosity = frontal_height / T(reynolds)
    simulation = WaterLily.Simulation(
        dimensions,
        (T(1), T(0)),
        frontal_height;
        ν=viscosity,
        body,
        T,
        mem=memory,
        exitBC=true,
    )
    return DiamondCase(simulation, dimensions, center, half_side)
end

function body_distance(case::DiamondCase, x::Real, y::Real, _dimensionless_time::Real)
    dx = x - case.center[1]
    dy = y - case.center[2]
    inv_sqrt2 = sqrt(0.5)
    u = abs((dx + dy) * inv_sqrt2) - case.half_side
    v = abs((dy - dx) * inv_sqrt2) - case.half_side
    outside = hypot(max(u, 0.0), max(v, 0.0))
    inside = min(max(u, v), 0.0)
    return outside + inside
end

case_palette(::DiamondCase) = BERRY_PALETTE
body_color(::DiamondCase) = BODY_PLUM
remeasure_on_step(::DiamondCase) = false
