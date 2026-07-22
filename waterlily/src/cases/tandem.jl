struct TandemCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    front_center::SVector{2,T}
    rear_center::SVector{2,T}
    radius::T
end

"""
Two static cylinders in tandem: the rear body sits inside the front body's
wake, so the two vortex streets interfere and merge into a wider, less
regular pattern than a single cylinder produces.
"""
function build_tandem_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=200,
)
    width, height = dimensions
    T = Float32
    radius = max(T(3), T(height * 0.06))
    diameter = radius * T(2)
    # A slight vertical stagger between the bodies breaks the symmetric start
    # and lets the interfering streets establish themselves sooner.
    front_center = SA[T(width * 0.20), T(height * 0.50) + T(0.6)]
    rear_center = SA[front_center[1] + diameter * T(3.5), front_center[2] - T(1.2)]

    pair_distance = let front_center=front_center, rear_center=rear_center, radius=radius
        function (x, _time)
            front_dx = x[1] - front_center[1]
            front_dy = x[2] - front_center[2]
            rear_dx = x[1] - rear_center[1]
            rear_dy = x[2] - rear_center[2]
            front = sqrt(front_dx * front_dx + front_dy * front_dy)
            rear = sqrt(rear_dx * rear_dx + rear_dy * rear_dy)
            return min(front, rear) - radius
        end
    end

    body = WaterLily.AutoBody(pair_distance)
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
    return TandemCase(simulation, dimensions, front_center, rear_center, radius)
end

function body_distance(case::TandemCase, x::Real, y::Real, _dimensionless_time::Real)
    front = hypot(x - case.front_center[1], y - case.front_center[2])
    rear = hypot(x - case.rear_center[1], y - case.rear_center[2])
    return min(front, rear) - case.radius
end

case_palette(::TandemCase) = GLACIER_PALETTE
body_color(::TandemCase) = BODY_COPPER
remeasure_on_step(::TandemCase) = false

function body_bounds(case::TandemCase, _dimensionless_time::Real)
    reach = case.radius + 2
    return (
        case.front_center[1] - reach,
        case.rear_center[1] + reach,
        min(case.front_center[2], case.rear_center[2]) - reach,
        max(case.front_center[2], case.rear_center[2]) + reach,
    )
end
