struct DanceCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    center::SVector{2,T}
    radius::T
    heave_amplitude::T
    period::T
end

"""
A cylinder oscillating transversely to a uniform stream, in the spirit of
vortex-induced-vibration studies. The heaving body weaves a wide, braided
wake that looks clearly different from the static vortex street.
"""
function build_dance_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=300,
)
    width, height = dimensions
    T = Float32
    radius = max(T(3), T(height * 0.055))
    diameter = radius * T(2)
    center = SA[T(width * 0.30), T(height * 0.50)]
    heave_amplitude = T(height * 0.16)
    period = T(3)
    angular_rate = T(2pi) / (period * diameter)

    cylinder_distance = let radius=radius
        function (x, _time)
            return sqrt(x[1] * x[1] + x[2] * x[2]) - radius
        end
    end
    heave_motion = let center=center,
        heave_amplitude=heave_amplitude,
        angular_rate=angular_rate
        function (x, time)
            vertical = center[2] + heave_amplitude * sin(angular_rate * time)
            return SA[x[1] - center[1], x[2] - vertical]
        end
    end

    body = WaterLily.AutoBody(cylinder_distance, heave_motion)
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
    return DanceCase(simulation, dimensions, center, radius, heave_amplitude, period)
end

function body_distance(case::DanceCase, x::Real, y::Real, dimensionless_time::Real)
    phase = 2pi * dimensionless_time / case.period
    vertical = case.center[2] + case.heave_amplitude * sin(phase)
    return hypot(x - case.center[1], y - vertical) - case.radius
end

case_palette(::DanceCase) = VIOLET_PALETTE
body_color(::DanceCase) = BODY_ROSE
