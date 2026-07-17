abstract type AbstractWaterLilyCase end

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

function advance!(case::HoverCase, dimensionless_step::Real)
    target = WaterLily.sim_time(case.simulation) + dimensionless_step
    WaterLily.sim_step!(case.simulation, target; remeasure=true)
    return case
end

function plate_distance(case::HoverCase, x::Real, y::Real, dimensionless_time::Real)
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

const SEISMIC_PALETTE = (
    (UInt8(0x00), UInt8(0x18), UInt8(0x8f)),
    (UInt8(0x00), UInt8(0x45), UInt8(0xd8)),
    (UInt8(0x36), UInt8(0x7c), UInt8(0xf3)),
    (UInt8(0x85), UInt8(0xad), UInt8(0xff)),
    (UInt8(0xc9), UInt8(0xda), UInt8(0xff)),
    (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd)),
    (UInt8(0xff), UInt8(0xd0), UInt8(0xd0)),
    (UInt8(0xff), UInt8(0x8c), UInt8(0x8c)),
    (UInt8(0xf4), UInt8(0x42), UInt8(0x42)),
    (UInt8(0xc9), UInt8(0x00), UInt8(0x20)),
    (UInt8(0x78), UInt8(0x00), UInt8(0x13)),
)
const BODY_LAVENDER = (UInt8(0x91), UInt8(0x87), UInt8(0xff))

function vorticity_field(case::HoverCase)
    velocity = Array(case.simulation.flow.u)
    width, height = case.dimensions
    vorticity = Matrix{Float32}(undef, width, height)
    scale = Float32(case.simulation.L / case.simulation.U)
    @inbounds for y in 1:height, x in 1:width
        index = CartesianIndex(x + 1, y + 1)
        vorticity[x, y] = Float32(WaterLily.curl(3, index, velocity) * scale)
    end
    return vorticity
end

function palette_scale(vorticity::AbstractMatrix)
    energy = 0.0
    peak = 0.0
    count = 0
    @inbounds for value in vorticity
        isfinite(value) || continue
        magnitude = abs(Float64(value))
        energy += magnitude * magnitude
        peak = max(peak, magnitude)
        count += 1
    end
    count == 0 && return 1.0
    # RMS resists a handful of extreme cells while the peak bound keeps the
    # strongest vortices on the palette instead of clipping the whole wake.
    return max(0.35, min(peak, 3.5 * sqrt(energy / count)))
end

function seismic_color(value::Real, scale::Real)
    normalized = clamp(Float64(value) / scale, -1.0, 1.0)
    index = round(Int, (normalized + 1.0) * 0.5 * (length(SEISMIC_PALETTE) - 1)) + 1
    return SEISMIC_PALETTE[clamp(index, 1, length(SEISMIC_PALETTE))]
end

function render_rgba(case::HoverCase)
    width, height = case.dimensions
    vorticity = vorticity_field(case)
    color_scale = palette_scale(vorticity)
    simulation_time = WaterLily.sim_time(case.simulation)
    rgba = Vector{UInt8}(undef, width * height * 4)

    # Rows are emitted top-to-bottom. WaterLily's second coordinate increases
    # upward, hence the explicit vertical flip into the top-left protocol.
    output = 1
    @inbounds for row in 1:height
        y = height - row + 1
        for x in 1:width
            color = if plate_distance(case, x + 0.5, y + 0.5, simulation_time) <= 0
                BODY_LAVENDER
            else
                seismic_color(vorticity[x, y], color_scale)
            end
            rgba[output] = color[1]
            rgba[output + 1] = color[2]
            rgba[output + 2] = color[3]
            rgba[output + 3] = 0xff
            output += 4
        end
    end
    return rgba
end
