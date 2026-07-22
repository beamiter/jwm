struct OrbitCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    center::SVector{2,T}
    radius::T
    orbit_radius::T
    period::T
end

"""
A cylinder stirring quiescent fluid along a circular orbit. There is no free
stream: every vortex in the frame is spun off by the moving body, curling
into spiral arms around the orbit.

The orbit radius is tied to the period so the body's rim speed equals the
`U = 1` velocity scale, keeping the Reynolds number and the adaptive time
step honest for a zero-inflow domain.
"""
function build_orbit_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=250,
)
    width, height = dimensions
    T = Float32
    radius = max(T(3), T(height * 0.09))
    diameter = radius * T(2)
    period = T(5)
    orbit_radius = period * diameter / T(2pi)
    center = SA[T(width * 0.45), T(height * 0.50)]
    angular_rate = T(2pi) / (period * diameter)

    cylinder_distance = let radius=radius
        function (x, _time)
            return sqrt(x[1] * x[1] + x[2] * x[2]) - radius
        end
    end
    orbit_motion = let center=center, orbit_radius=orbit_radius, angular_rate=angular_rate
        function (x, time)
            sine, cosine = sincos(angular_rate * time)
            return SA[
                x[1] - center[1] - orbit_radius * cosine,
                x[2] - center[2] - orbit_radius * sine,
            ]
        end
    end

    body = WaterLily.AutoBody(cylinder_distance, orbit_motion)
    viscosity = diameter / T(reynolds)
    simulation = WaterLily.Simulation(
        dimensions,
        (T(0), T(0)),
        diameter;
        U=T(1),
        ν=viscosity,
        body,
        T,
        mem=memory,
    )
    return OrbitCase(simulation, dimensions, center, radius, orbit_radius, period)
end

function body_distance(case::OrbitCase, x::Real, y::Real, dimensionless_time::Real)
    phase = 2pi * dimensionless_time / case.period
    sine, cosine = sincos(phase)
    body_x = case.center[1] + case.orbit_radius * cosine
    body_y = case.center[2] + case.orbit_radius * sine
    return hypot(x - body_x, y - body_y) - case.radius
end

case_palette(::OrbitCase) = COSMOS_PALETTE
body_color(::OrbitCase) = BODY_GOLD
