struct CylinderCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    center::SVector{2,T}
    radius::T
end

"""
A static circular cylinder in a uniform stream shedding the classic von
Kármán vortex street. The body never moves, so the solver skips re-measuring
the geometry on every step.
"""
function build_cylinder_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=250,
)
    width, height = dimensions
    T = Float32
    radius = max(T(4), T(height * 0.08))
    diameter = radius * T(2)
    # The slight vertical offset seeds the shedding instability sooner than a
    # perfectly symmetric impulsive start would.
    center = SA[T(width * 0.28), T(height * 0.50) + T(0.6)]

    cylinder_distance = let center=center, radius=radius
        function (x, _time)
            dx = x[1] - center[1]
            dy = x[2] - center[2]
            return sqrt(dx * dx + dy * dy) - radius
        end
    end

    body = WaterLily.AutoBody(cylinder_distance)
    viscosity = diameter / T(reynolds)
    simulation = WaterLily.Simulation(
        dimensions,
        (T(1), T(0)),
        diameter;
        ν=viscosity,
        body,
        T,
        mem=memory,
        exitBC=true,
    )
    return CylinderCase(simulation, dimensions, center, radius)
end

body_distance(case::CylinderCase, x::Real, y::Real, _dimensionless_time::Real) =
    hypot(x - case.center[1], y - case.center[2]) - case.radius

case_palette(::CylinderCase) = OCEAN_PALETTE
body_color(::CylinderCase) = BODY_SLATE
remeasure_on_step(::CylinderCase) = false

function body_bounds(case::CylinderCase, _dimensionless_time::Real)
    reach = case.radius + 2
    return (
        case.center[1] - reach,
        case.center[1] + reach,
        case.center[2] - reach,
        case.center[2] + reach,
    )
end
