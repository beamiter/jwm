abstract type AbstractWaterLilyCase end

# Every diverging palette shares the same near-white midpoint (0xfa,0xfa,0xfd).
# The compositor shader keys bright, low-chroma pixels out to its frosted
# backdrop, so quiescent fluid becomes translucent for every case while the
# saturated vortex colors stay opaque.
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
const OCEAN_PALETTE = (
    (UInt8(0x00), UInt8(0x3d), UInt8(0x3a)),
    (UInt8(0x00), UInt8(0x69), UInt8(0x63)),
    (UInt8(0x24), UInt8(0x95), UInt8(0x8d)),
    (UInt8(0x7d), UInt8(0xc4), UInt8(0xbc)),
    (UInt8(0xc8), UInt8(0xe8), UInt8(0xe4)),
    (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd)),
    (UInt8(0xff), UInt8(0xe2), UInt8(0xc2)),
    (UInt8(0xff), UInt8(0xc4), UInt8(0x85)),
    (UInt8(0xf6), UInt8(0x9a), UInt8(0x3e)),
    (UInt8(0xd9), UInt8(0x6f), UInt8(0x00)),
    (UInt8(0x8f), UInt8(0x45), UInt8(0x00)),
)
const VIOLET_PALETTE = (
    (UInt8(0x40), UInt8(0x00), UInt8(0x66)),
    (UInt8(0x6a), UInt8(0x24), UInt8(0x9c)),
    (UInt8(0x94), UInt8(0x5f), UInt8(0xc4)),
    (UInt8(0xc0), UInt8(0x9d), UInt8(0xe4)),
    (UInt8(0xe3), UInt8(0xd3), UInt8(0xf3)),
    (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd)),
    (UInt8(0xd6), UInt8(0xf0), UInt8(0xd0)),
    (UInt8(0xa4), UInt8(0xdd), UInt8(0x9c)),
    (UInt8(0x5f), UInt8(0xbd), UInt8(0x5a)),
    (UInt8(0x22), UInt8(0x8b), UInt8(0x22)),
    (UInt8(0x0b), UInt8(0x54), UInt8(0x0f)),
)
const EMBER_PALETTE = (
    (UInt8(0x1a), UInt8(0x1a), UInt8(0x73)),
    (UInt8(0x2f), UInt8(0x39), UInt8(0xb0)),
    (UInt8(0x5b), UInt8(0x6a), UInt8(0xdb)),
    (UInt8(0x95), UInt8(0xa3), UInt8(0xf0)),
    (UInt8(0xd0), UInt8(0xd6), UInt8(0xf9)),
    (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd)),
    (UInt8(0xff), UInt8(0xe9), UInt8(0xc4)),
    (UInt8(0xff), UInt8(0xd0), UInt8(0x7e)),
    (UInt8(0xf7), UInt8(0xa8), UInt8(0x2c)),
    (UInt8(0xd1), UInt8(0x7a), UInt8(0x00)),
    (UInt8(0x7f), UInt8(0x46), UInt8(0x00)),
)
const GLACIER_PALETTE = (
    (UInt8(0x03), UInt8(0x35), UInt8(0x5c)),
    (UInt8(0x0a), UInt8(0x5c), UInt8(0x8f)),
    (UInt8(0x2e), UInt8(0x86), UInt8(0xba)),
    (UInt8(0x7c), UInt8(0xb4), UInt8(0xd8)),
    (UInt8(0xc6), UInt8(0xe0), UInt8(0xef)),
    (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd)),
    (UInt8(0xea), UInt8(0xdf), UInt8(0xc8)),
    (UInt8(0xd4), UInt8(0xbe), UInt8(0x92)),
    (UInt8(0xb3), UInt8(0x93), UInt8(0x57)),
    (UInt8(0x8a), UInt8(0x6a), UInt8(0x2c)),
    (UInt8(0x5c), UInt8(0x44), UInt8(0x14)),
)
const BERRY_PALETTE = (
    (UInt8(0x6e), UInt8(0x00), UInt8(0x4c)),
    (UInt8(0xa1), UInt8(0x14), UInt8(0x74)),
    (UInt8(0xc9), UInt8(0x4f), UInt8(0xa4)),
    (UInt8(0xe4), UInt8(0x92), UInt8(0xcc)),
    (UInt8(0xf4), UInt8(0xd0), UInt8(0xe8)),
    (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd)),
    (UInt8(0xe4), UInt8(0xf2), UInt8(0xc2)),
    (UInt8(0xc8), UInt8(0xe2), UInt8(0x86)),
    (UInt8(0x9c), UInt8(0xc4), UInt8(0x3b)),
    (UInt8(0x6f), UInt8(0x94), UInt8(0x0f)),
    (UInt8(0x45), UInt8(0x5e), UInt8(0x06)),
)
const COSMOS_PALETTE = (
    (UInt8(0x7a), UInt8(0x00), UInt8(0x33)),
    (UInt8(0xad), UInt8(0x1a), UInt8(0x53)),
    (UInt8(0xd4), UInt8(0x54), UInt8(0x7f)),
    (UInt8(0xea), UInt8(0x96), UInt8(0xae)),
    (UInt8(0xf7), UInt8(0xd2), UInt8(0xdc)),
    (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd)),
    (UInt8(0xd8), UInt8(0xdf), UInt8(0xf2)),
    (UInt8(0xad), UInt8(0xbb), UInt8(0xe4)),
    (UInt8(0x77), UInt8(0x8b), UInt8(0xcb)),
    (UInt8(0x48), UInt8(0x5d), UInt8(0xa6)),
    (UInt8(0x27), UInt8(0x35), UInt8(0x70)),
)
const ALL_PALETTES = (
    SEISMIC_PALETTE,
    OCEAN_PALETTE,
    VIOLET_PALETTE,
    EMBER_PALETTE,
    GLACIER_PALETTE,
    BERRY_PALETTE,
    COSMOS_PALETTE,
)

const BODY_LAVENDER = (UInt8(0x91), UInt8(0x87), UInt8(0xff))
const BODY_SLATE = (UInt8(0x4a), UInt8(0x5f), UInt8(0x6d))
const BODY_ROSE = (UInt8(0xe0), UInt8(0x63), UInt8(0x8f))
const BODY_TEAL = (UInt8(0x00), UInt8(0x89), UInt8(0x7b))
const BODY_COPPER = (UInt8(0xb0), UInt8(0x72), UInt8(0x3a))
const BODY_PLUM = (UInt8(0x8e), UInt8(0x44), UInt8(0x85))
const BODY_GOLD = (UInt8(0xd4), UInt8(0xa5), UInt8(0x1d))

# Case interface: a concrete case owns `simulation` and `dimensions` fields
# and implements `body_distance`; palette, body color, and remeasure policy
# have sensible defaults.
function body_distance end
case_palette(::AbstractWaterLilyCase) = SEISMIC_PALETTE
body_color(::AbstractWaterLilyCase) = BODY_LAVENDER
remeasure_on_step(::AbstractWaterLilyCase) = true

function advance!(case::AbstractWaterLilyCase, dimensionless_step::Real)
    target = WaterLily.sim_time(case.simulation) + dimensionless_step
    WaterLily.sim_step!(case.simulation, target; remeasure=remeasure_on_step(case))
    return case
end

function vorticity_field(case::AbstractWaterLilyCase)
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

function palette_color(palette::Tuple, value::Real, scale::Real)
    normalized = clamp(Float64(value) / scale, -1.0, 1.0)
    index = round(Int, (normalized + 1.0) * 0.5 * (length(palette) - 1)) + 1
    return palette[clamp(index, 1, length(palette))]
end

seismic_color(value::Real, scale::Real) = palette_color(SEISMIC_PALETTE, value, scale)

function render_rgba(case::AbstractWaterLilyCase)
    width, height = case.dimensions
    vorticity = vorticity_field(case)
    color_scale = palette_scale(vorticity)
    simulation_time = WaterLily.sim_time(case.simulation)
    palette = case_palette(case)
    body = body_color(case)
    rgba = Vector{UInt8}(undef, width * height * 4)

    # Rows are emitted top-to-bottom. WaterLily's second coordinate increases
    # upward, hence the explicit vertical flip into the top-left protocol.
    output = 1
    @inbounds for row in 1:height
        y = height - row + 1
        for x in 1:width
            color = if body_distance(case, x + 0.5, y + 0.5, simulation_time) <= 0
                body
            else
                palette_color(palette, vorticity[x, y], color_scale)
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
