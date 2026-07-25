"""
Closed-form critically damped spring segment for one axis pair. The body
trajectory between pointer updates is a pure function of raw solver time
given these captured scalars, which keeps the `AutoBody` map closure isbits
and therefore exact under `remeasure` and safe inside GPU kernels — the same
contract the wander case relies on, extended to interactive retargeting.
"""
struct SpringSegment{T}
    start_time::T
    position::SVector{2,T}
    velocity::SVector{2,T}
    target::SVector{2,T}
    rate::T
end

# Time stays generic: WaterLily measures body velocity by forward-mode
# differentiation of the map, so these must accept ForwardDiff duals.
function segment_position(segment::SpringSegment, time)
    elapsed = time - segment.start_time
    Δt = max(zero(elapsed), elapsed)
    decay = exp(-segment.rate * Δt)
    offset = segment.position - segment.target
    slope = segment.velocity + segment.rate * offset
    return segment.target + (offset + slope * Δt) * decay
end

function segment_velocity(segment::SpringSegment, time)
    elapsed = time - segment.start_time
    Δt = max(zero(elapsed), elapsed)
    decay = exp(-segment.rate * Δt)
    offset = segment.position - segment.target
    slope = segment.velocity + segment.rate * offset
    return (slope - segment.rate * (offset + slope * Δt)) * decay
end

mutable struct StylusCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    radius::T
    margin::T
    # Where the pointer actually is (grid coordinates, y up); the spring
    # chases a reach-limited waypoint toward it so a screen-wide mouse flick
    # cannot demand a body speed the solver's CFL condition would choke on.
    goal::SVector{2,T}
    segment::SpringSegment{T}
end

# Spring response rate in 1/dimensionless-time: the body covers most of a
# retarget in ~4/STYLUS_RATE dimensionless units (≈ wall seconds at real
# time), so 9.0 feels attached to the cursor without teleporting.
const STYLUS_RATE = 9.0
# Peak body speed as a multiple of the velocity scale U. WaterLily's adaptive
# time step keeps a fast body stable but pays for it in substeps; 3U keeps
# the chase brisk at real-time frame budgets.
const STYLUS_MAX_SPEED = 3.0

"""
A cylinder that chases the mouse pointer through quiescent fluid: the
compositor streams `pointer X Y` and the body follows on a critically damped
spring, so every stroke of the cursor writes vorticity onto the canvas.
Between updates the trajectory is analytic, and each retarget swaps
`simulation.body` for a fresh closure over plain scalars.
"""
function build_stylus_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=300,
)
    width, height = dimensions
    T = Float32
    radius = max(T(3), T(height * 0.05))
    diameter = radius * T(2)
    margin = radius + T(8)
    center = SA[T(width) / T(2), T(height) / T(2)]
    # Raw solver time is dimensionless time times L/U with U = 1, so the
    # dimensionless spring rate shrinks by the diameter.
    rate = T(STYLUS_RATE) / diameter
    segment = SpringSegment{T}(T(0), center, SA[T(0), T(0)], center, rate)

    body = WaterLily.AutoBody(stylus_sdf(radius), stylus_map(segment))
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
    return StylusCase(simulation, dimensions, radius, margin, center, segment)
end

stylus_sdf(radius) =
    let radius = radius
        (x, _time) -> sqrt(x[1] * x[1] + x[2] * x[2]) - radius
    end

stylus_map(segment::SpringSegment) =
    let segment = segment
        (x, time) -> x - segment_position(segment, time)
    end

raw_time(case::StylusCase, dimensionless_time::Real) =
    dimensionless_time * case.simulation.L / case.simulation.U

"""
Re-aim the spring at a reach-limited waypoint toward `goal`, splicing the new
segment at the current analytic position and velocity so the trajectory stays
C¹-continuous — a velocity jump would slam the pressure solver.
"""
function retarget!(case::StylusCase{S,T}) where {S,T}
    segment = case.segment
    now = T(WaterLily.time(case.simulation.flow))
    position = segment_position(segment, now)
    velocity = segment_velocity(segment, now)
    speed_cap = T(STYLUS_MAX_SPEED) * T(case.simulation.U)
    speed = hypot(velocity[1], velocity[2])
    speed > speed_cap && (velocity = velocity * (speed_cap / speed))

    offset = case.goal - position
    distance = hypot(offset[1], offset[2])
    # A rest-to-rest spring peaks at rate·d/e, so this reach keeps the chase
    # under the speed cap no matter how far the pointer jumped. Re-clamping
    # every frame turns a distant goal into a rate-limited pursuit.
    reach = speed_cap * T(Base.MathConstants.e) / segment.rate
    target = distance > reach ? position + offset * (reach / distance) : case.goal

    case.segment = SpringSegment{T}(now, position, velocity, target, segment.rate)
    case.simulation.body =
        WaterLily.AutoBody(stylus_sdf(case.radius), stylus_map(case.segment))
    return case
end

function handle_pointer!(case::StylusCase{S,T}, x::Real, y::Real) where {S,T}
    width, height = case.dimensions
    # Normalized top-left display coordinates → grid coordinates with y up.
    goal_x = clamp(T(x) * T(width), case.margin, T(width) - case.margin)
    goal_y = clamp((T(1) - T(y)) * T(height), case.margin, T(height) - case.margin)
    case.goal = SA[goal_x, goal_y]
    retarget!(case)
    return nothing
end

# Even without fresh pointer events the reach-limited waypoint must keep
# extending toward a distant goal, so re-splice once per published frame.
frame_tick!(case::StylusCase) = (retarget!(case); nothing)

function stylus_position(case::StylusCase, dimensionless_time::Real)
    position = segment_position(case.segment, raw_time(case, dimensionless_time))
    return (position[1], position[2])
end

function body_distance(case::StylusCase, x::Real, y::Real, dimensionless_time::Real)
    body_x, body_y = stylus_position(case, dimensionless_time)
    return hypot(x - body_x, y - body_y) - case.radius
end

case_palette_name(::StylusCase) = "fluent"
body_color(::StylusCase) = BODY_SLATE

function body_bounds(case::StylusCase, dimensionless_time::Real)
    body_x, body_y = stylus_position(case, dimensionless_time)
    reach = case.radius + 2
    return (body_x - reach, body_x + reach, body_y - reach, body_y + reach)
end
