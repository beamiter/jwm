mutable struct WaltzCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    radius::T
    margin::T
    heave_amplitude::T
    # Raw-solver-time angular rate paired with the amplitude so the heave
    # never spends more than WALTZ_HEAVE_PEAK_SPEED of the CFL budget.
    heave_rate::T
    # Where the pointer actually is (grid coordinates, y up); the spring
    # chases a reach-limited waypoint toward it, exactly like the stylus.
    goal::SVector{2,T}
    segment::SpringSegment{T}
end

# Spring response rate in 1/dimensionless-time, matching the stylus feel.
const WALTZ_RATE = 9.0
# The CFL speed budget splits between the chase and the heave: the spring is
# capped at 2U and the heave amplitude/rate pair peaks at exactly 1U. Note a
# 3U combined peak dilates a megapixel canvas to roughly a third of real time
# in a developed wake (measured on the stylus, whose cap dropped to 1.5U for
# it); the waltz keeps the wider envelope because the finale reads as dreamy
# rather than laggy, and the heave rarely peaks in phase with the chase.
const WALTZ_CHASE_MAX_SPEED = 2.0
const WALTZ_HEAVE_PEAK_SPEED = 1.0

"""
The dance case's transverse heave riding on the stylus case's critically
damped chase spring, in a uniform stream: the body follows the mouse pointer
while oscillating across the flow, so dance's braided vortex-induced-vibration
wake trails downstream from wherever the cursor leads. Both trajectory terms
are closed-form in raw solver time, keeping the `AutoBody` map closure isbits
and exact under `remeasure`.
"""
function build_waltz_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=300,
)
    width, height = dimensions
    T = Float32
    radius = max(T(3), T(height * 0.05))
    diameter = radius * T(2)
    margin = radius + T(8)
    heave_amplitude = max(T(2), T(height * 0.08))
    center = SA[T(width) / T(2), T(height) / T(2)]
    # Raw solver time is dimensionless time times L/U with U = 1, so the
    # dimensionless spring rate shrinks by the diameter.
    rate = T(WALTZ_RATE) / diameter
    segment = SpringSegment{T}(T(0), center, SA[T(0), T(0)], center, rate)
    # Peak heave speed is amplitude times rate; fixing it at the budgeted
    # fraction of U makes the oscillation period follow the amplitude.
    heave_rate = T(WALTZ_HEAVE_PEAK_SPEED) / heave_amplitude

    body = WaterLily.AutoBody(
        waltz_sdf(radius),
        waltz_map(segment, heave_amplitude, heave_rate),
    )
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
    return WaltzCase(
        simulation,
        dimensions,
        radius,
        margin,
        heave_amplitude,
        heave_rate,
        center,
        segment,
    )
end

waltz_sdf(radius) =
    let radius = radius
        (x, _time) -> sqrt(x[1] * x[1] + x[2] * x[2]) - radius
    end

waltz_map(segment::SpringSegment, amplitude, rate) =
    let segment = segment, amplitude = amplitude, rate = rate
        (x, time) -> begin
            center = segment_position(segment, time)
            return SA[x[1] - center[1], x[2] - center[2] - amplitude * sin(rate * time)]
        end
    end

raw_time(case::WaltzCase, dimensionless_time::Real) =
    dimensionless_time * case.simulation.L / case.simulation.U

"""
Re-aim the spring at a reach-limited waypoint toward `goal`, splicing the new
segment at the current analytic position and velocity so the chase trajectory
stays C¹-continuous. The heave term depends only on absolute solver time, so
it is untouched by a retarget and never jumps either.
"""
function retarget!(case::WaltzCase{S,T}) where {S,T}
    segment = case.segment
    now = T(WaterLily.time(case.simulation.flow))
    position = segment_position(segment, now)
    velocity = segment_velocity(segment, now)
    speed_cap = T(WALTZ_CHASE_MAX_SPEED) * T(case.simulation.U)
    speed = hypot(velocity[1], velocity[2])
    speed > speed_cap && (velocity = velocity * (speed_cap / speed))

    offset = case.goal - position
    distance = hypot(offset[1], offset[2])
    # A rest-to-rest spring peaks at rate·d/e, so this reach keeps the chase
    # under its share of the speed budget no matter how far the pointer
    # jumped; re-clamping every frame turns a distant goal into a
    # rate-limited pursuit.
    reach = speed_cap * T(Base.MathConstants.e) / segment.rate
    target = distance > reach ? position + offset * (reach / distance) : case.goal

    case.segment = SpringSegment{T}(now, position, velocity, target, segment.rate)
    case.simulation.body = WaterLily.AutoBody(
        waltz_sdf(case.radius),
        waltz_map(case.segment, case.heave_amplitude, case.heave_rate),
    )
    return case
end

function handle_pointer!(case::WaltzCase{S,T}, x::Real, y::Real) where {S,T}
    width, height = case.dimensions
    # Normalized top-left display coordinates → grid coordinates with y up.
    # The heave swings the body vertically around the spring position, so the
    # vertical clamp reserves the amplitude on top of the body margin.
    vertical_margin = case.margin + case.heave_amplitude
    goal_x = clamp(T(x) * T(width), case.margin, T(width) - case.margin)
    goal_y = clamp((T(1) - T(y)) * T(height), vertical_margin, T(height) - vertical_margin)
    case.goal = SA[goal_x, goal_y]
    retarget!(case)
    return nothing
end

# Even without fresh pointer events the reach-limited waypoint must keep
# extending toward a distant goal, so re-splice once per published frame.
frame_tick!(case::WaltzCase) = (retarget!(case); nothing)

function waltz_position(case::WaltzCase, dimensionless_time::Real)
    time = raw_time(case, dimensionless_time)
    position = segment_position(case.segment, time)
    heave = case.heave_amplitude * sin(case.heave_rate * time)
    return (position[1], position[2] + heave)
end

function body_distance(case::WaltzCase, x::Real, y::Real, dimensionless_time::Real)
    body_x, body_y = waltz_position(case, dimensionless_time)
    return hypot(x - body_x, y - body_y) - case.radius
end

case_palette_name(::WaltzCase) = "mica"
body_color(::WaltzCase) = BODY_PLUM

function body_bounds(case::WaltzCase, dimensionless_time::Real)
    body_x, body_y = waltz_position(case, dimensionless_time)
    reach = case.radius + 2
    return (body_x - reach, body_x + reach, body_y - reach, body_y + reach)
end
