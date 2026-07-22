struct WanderCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    center::SVector{2,T}
    radius::T
    amplitudes::SVector{4,T}
    periods::SVector{4,T}
    phases::SVector{4,T}
end

# Incommensurate periods (in dimensionless simulation seconds) make the
# two-tone Lissajous path non-repeating, so the body eventually visits the
# whole canvas without ever jumping.
const WANDER_PERIODS = (34.0, 13.0, 26.0, 11.0)

"""
A cylinder wandering through quiescent fluid on a smooth, non-repeating
Lissajous path: two incommensurate sine tones per axis. Every ripple in the
frame is shed by the roaming body, and because the trajectory is a pure
function of time it stays exact under `remeasure` and is GPU-kernel safe,
unlike a stateful random walk.

Designed for the full-screen canvas mode: the quiescent background keys out
to the compositor's frosted blur across the whole display while the wake
trails the wandering body everywhere it goes.
"""
function build_wander_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=250,
)
    width, height = dimensions
    T = Float32
    # The body diameter is also the solver's length scale: raw time per
    # published frame is L/fps, so a leaner body directly cuts the number of
    # pressure solves needed per frame at large resolutions.
    radius = max(T(3), T(height * 0.045))
    diameter = radius * T(2)
    center = SA[T(width) / T(2), T(height) / T(2)]
    margin = radius + T(8)
    range_x = max(T(0), T(width) / T(2) - margin)
    range_y = max(T(0), T(height) / T(2) - margin)
    amplitudes = SA[
        range_x * T(0.65),
        range_x * T(0.35),
        range_y * T(0.65),
        range_y * T(0.35),
    ]
    periods = SA[T.(WANDER_PERIODS)...]
    # Random phases give each worker run its own path without introducing
    # mutable trajectory state.
    phases = SA[ntuple(_ -> T(2pi) * rand(T), 4)...]
    # Raw solver time is dimensionless time multiplied by L/U with U = 1.
    rates = SA[ntuple(i -> T(2pi) / (periods[i] * diameter), 4)...]

    cylinder_distance = let radius=radius
        function (x, _time)
            return sqrt(x[1] * x[1] + x[2] * x[2]) - radius
        end
    end
    wander_motion = let center=center, amplitudes=amplitudes, rates=rates, phases=phases
        function (x, time)
            body_x = center[1] +
                     amplitudes[1] * sin(rates[1] * time + phases[1]) +
                     amplitudes[2] * sin(rates[2] * time + phases[2])
            body_y = center[2] +
                     amplitudes[3] * sin(rates[3] * time + phases[3]) +
                     amplitudes[4] * sin(rates[4] * time + phases[4])
            return SA[x[1] - body_x, x[2] - body_y]
        end
    end

    body = WaterLily.AutoBody(cylinder_distance, wander_motion)
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
    return WanderCase(simulation, dimensions, center, radius, amplitudes, periods, phases)
end

function wander_position(case::WanderCase, dimensionless_time::Real)
    angles = ntuple(
        i -> 2pi * dimensionless_time / case.periods[i] + case.phases[i],
        4,
    )
    x = case.center[1] +
        case.amplitudes[1] * sin(angles[1]) +
        case.amplitudes[2] * sin(angles[2])
    y = case.center[2] +
        case.amplitudes[3] * sin(angles[3]) +
        case.amplitudes[4] * sin(angles[4])
    return (x, y)
end

function body_distance(case::WanderCase, x::Real, y::Real, dimensionless_time::Real)
    body_x, body_y = wander_position(case, dimensionless_time)
    return hypot(x - body_x, y - body_y) - case.radius
end

case_palette(::WanderCase) = AURORA_PALETTE
body_color(::WanderCase) = BODY_INDIGO

function body_bounds(case::WanderCase, dimensionless_time::Real)
    body_x, body_y = wander_position(case, dimensionless_time)
    reach = case.radius + 2
    return (body_x - reach, body_x + reach, body_y - reach, body_y + reach)
end
