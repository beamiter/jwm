struct FlapCase{S,T} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    pivot::SVector{2,T}
    chord::T
    half_thickness::T
    pitch_amplitude::T
    period::T
end

"""
A thin plate pitching about its leading edge in a uniform stream: a
flag/fish-tail style flapper whose thrust-type reverse Kármán wake reads as
an alternating jet rather than a drag street.
"""
function build_flap_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=400,
)
    width, height = dimensions
    T = Float32
    chord = T(min(width * 0.28, height * 0.45))
    pivot = SA[T(width * 0.30), T(height * 0.50)]
    half_thickness = max(T(1.5), chord * T(0.03))
    pitch_amplitude = T(pi / 8)
    period = T(2.5)
    angular_rate = T(2pi) / (period * chord)

    # Local coordinates run downstream from the leading-edge pivot.
    plate_distance = let chord=chord, half_thickness=half_thickness
        function (x, _time)
            dx = x[1] - clamp(x[1], zero(chord), chord)
            return sqrt(dx * dx + x[2] * x[2]) - half_thickness
        end
    end
    pitch_motion = let pivot=pivot,
        pitch_amplitude=pitch_amplitude,
        angular_rate=angular_rate
        function (x, time)
            angle = pitch_amplitude * sin(angular_rate * time)
            sine, cosine = sincos(angle)
            dx = x[1] - pivot[1]
            dy = x[2] - pivot[2]
            return SA[cosine * dx + sine * dy, -sine * dx + cosine * dy]
        end
    end

    body = WaterLily.AutoBody(plate_distance, pitch_motion)
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
    return FlapCase(
        simulation,
        dimensions,
        pivot,
        chord,
        half_thickness,
        pitch_amplitude,
        period,
    )
end

function body_distance(case::FlapCase, x::Real, y::Real, dimensionless_time::Real)
    phase = 2pi * dimensionless_time / case.period
    angle = case.pitch_amplitude * sin(phase)
    sine, cosine = sincos(angle)
    dx = x - case.pivot[1]
    dy = y - case.pivot[2]
    local_x = cosine * dx + sine * dy
    local_y = -sine * dx + cosine * dy
    segment_dx = local_x - clamp(local_x, zero(case.chord), case.chord)
    return hypot(segment_dx, local_y) - case.half_thickness
end

case_palette(::FlapCase) = EMBER_PALETTE
body_color(::FlapCase) = BODY_TEAL
