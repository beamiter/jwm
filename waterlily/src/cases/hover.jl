struct HoverCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    center::SVector{2,T}
    half_span::T
    half_thickness::T
    heave_amplitude::T
    pitch_amplitude::T
    period::T
end

"""
An independently authored hovering thin-plate case using WaterLily's public
`AutoBody` and `Simulation` APIs.

WaterLily-Examples is useful as a catalogue of possible future adapters:
https://github.com/WaterLily-jl/WaterLily-Examples
No equations or implementation are copied from that GPL-licensed repository.
"""
function build_hover_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=500,
)
    width, height = dimensions
    T = Float32
    chord = T(min(width * 0.30, height * 0.55))
    center = SA[T(width * 0.38), T(height * 0.50)]
    half_span = chord / T(2)
    half_thickness = max(T(1.5), chord * T(0.025))
    heave_amplitude = T(height * 0.14)
    pitch_amplitude = T(pi / 7)
    period = T(4)
    angular_rate = T(2pi) / (period * chord)
    pitch_phase = T(pi / 2)

    plate_distance = let half_span=half_span, half_thickness=half_thickness
        function (x, _time)
            closest_x = clamp(x[1], -half_span, half_span)
            dx = x[1] - closest_x
            return sqrt(dx * dx + x[2] * x[2]) - half_thickness
        end
    end
    plate_motion = let center=center,
        heave_amplitude=heave_amplitude,
        pitch_amplitude=pitch_amplitude,
        angular_rate=angular_rate,
        pitch_phase=pitch_phase
        function (x, time)
            phase = angular_rate * time
            vertical = center[2] + heave_amplitude * sin(phase)
            angle = pitch_amplitude * sin(phase + pitch_phase)
            sine, cosine = sincos(angle)
            dx = x[1] - center[1]
            dy = x[2] - vertical
            return SA[cosine * dx + sine * dy, -sine * dx + cosine * dy]
        end
    end

    body = WaterLily.AutoBody(plate_distance, plate_motion)
    viscosity = chord / T(reynolds)
    simulation = WaterLily.Simulation(
        dimensions,
        (T(1), T(0)),
        chord;
        ν=viscosity,
        body,
        T,
        mem=memory,
        exitBC=true,
    )
    return HoverCase(
        simulation,
        dimensions,
        center,
        half_span,
        half_thickness,
        heave_amplitude,
        pitch_amplitude,
        period,
    )
end

function body_distance(case::HoverCase, x::Real, y::Real, dimensionless_time::Real)
    phase = 2pi * dimensionless_time / case.period
    vertical = case.center[2] + case.heave_amplitude * sin(phase)
    angle = case.pitch_amplitude * sin(phase + pi / 2)
    sine, cosine = sincos(angle)
    dx = x - case.center[1]
    dy = y - vertical
    local_x = cosine * dx + sine * dy
    local_y = -sine * dx + cosine * dy
    segment_dx = local_x - clamp(local_x, -case.half_span, case.half_span)
    return hypot(segment_dx, local_y) - case.half_thickness
end

case_palette(::HoverCase) = SEISMIC_PALETTE
body_color(::HoverCase) = BODY_LAVENDER
