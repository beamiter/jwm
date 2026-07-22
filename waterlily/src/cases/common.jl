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
const AURORA_PALETTE = (
    (UInt8(0x00), UInt8(0x4d), UInt8(0x40)),
    (UInt8(0x00), UInt8(0x77), UInt8(0x63)),
    (UInt8(0x2a), UInt8(0xa1), UInt8(0x88)),
    (UInt8(0x7f), UInt8(0xc9), UInt8(0xb4)),
    (UInt8(0xc9), UInt8(0xe9), UInt8(0xdf)),
    (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd)),
    (UInt8(0xf6), UInt8(0xd8), UInt8(0xef)),
    (UInt8(0xea), UInt8(0xa8), UInt8(0xdc)),
    (UInt8(0xd4), UInt8(0x6e), UInt8(0xc0)),
    (UInt8(0xa8), UInt8(0x37), UInt8(0x96)),
    (UInt8(0x6b), UInt8(0x12), UInt8(0x60)),
)
const ALL_PALETTES = (
    SEISMIC_PALETTE,
    OCEAN_PALETTE,
    VIOLET_PALETTE,
    EMBER_PALETTE,
    GLACIER_PALETTE,
    BERRY_PALETTE,
    COSMOS_PALETTE,
    AURORA_PALETTE,
)

const BODY_LAVENDER = (UInt8(0x91), UInt8(0x87), UInt8(0xff))
const BODY_SLATE = (UInt8(0x4a), UInt8(0x5f), UInt8(0x6d))
const BODY_ROSE = (UInt8(0xe0), UInt8(0x63), UInt8(0x8f))
const BODY_TEAL = (UInt8(0x00), UInt8(0x89), UInt8(0x7b))
const BODY_COPPER = (UInt8(0xb0), UInt8(0x72), UInt8(0x3a))
const BODY_PLUM = (UInt8(0x8e), UInt8(0x44), UInt8(0x85))
const BODY_GOLD = (UInt8(0xd4), UInt8(0xa5), UInt8(0x1d))
const BODY_INDIGO = (UInt8(0x5c), UInt8(0x6b), UInt8(0xc0))

# Case interface: a concrete case owns `simulation` and `dimensions` fields
# and implements `body_distance`; palette, body color, remeasure policy, and
# body bounds have sensible defaults.
function body_distance end
case_palette(::AbstractWaterLilyCase) = SEISMIC_PALETTE
body_color(::AbstractWaterLilyCase) = BODY_LAVENDER
remeasure_on_step(::AbstractWaterLilyCase) = true

"""
Loose axis-aligned `(xmin, xmax, ymin, ymax)` bounds of the body and its
anti-aliasing feather at dimensionless time `τ`, or `nothing` when the body
could be anywhere. The renderer only evaluates the signed distance inside
these bounds, which matters at megapixel sizes where the body covers a tiny
fraction of the canvas.
"""
body_bounds(::AbstractWaterLilyCase, ::Real) = nothing

simulation_time(case::AbstractWaterLilyCase) = WaterLily.sim_time(case.simulation)

function advance!(case::AbstractWaterLilyCase, dimensionless_step::Real)
    target = WaterLily.sim_time(case.simulation) + dimensionless_step
    WaterLily.sim_step!(case.simulation, target; remeasure=remeasure_on_step(case))
    return case
end

"""
Advance toward `dimensionless_step` more simulation time, but stop after the
substep that crosses `deadline_ns` (`time_ns()` clock). At least one substep
always runs. Returns the dimensionless time actually gained.

This keeps the publish cadence fixed under load: when the solver cannot
reach real time within the frame budget the simulation clock dilates into
smooth slow motion instead of stalling the frame stream.
"""
function advance_budgeted!(
    case::AbstractWaterLilyCase,
    dimensionless_step::Real,
    deadline_ns::UInt64,
)
    simulation = case.simulation
    remeasure = remeasure_on_step(case)
    start = WaterLily.sim_time(simulation)
    target = start + dimensionless_step
    while true
        WaterLily.sim_step!(simulation; remeasure)
        achieved = WaterLily.sim_time(simulation)
        (achieved >= target || time_ns() >= deadline_ns) && return achieved - start
    end
end

"""
Reusable host-side buffers for the frame renderer. Allocating fresh
megabyte-scale arrays every frame produced enough garbage to stall the
publish loop, so the worker keeps one scratch for its lifetime.
"""
mutable struct RenderScratch
    padded_vorticity::Matrix{Float32}
    rgba::Vector{UInt8}
end

RenderScratch(dimensions::Tuple{Int,Int}) = RenderScratch(
    Matrix{Float32}(undef, dimensions[1] + 2, dimensions[2] + 2),
    Vector{UInt8}(undef, 4 * dimensions[1] * dimensions[2]),
)

"""
Evaluate vorticity on the simulation device and download only the scalar
result. The previous approach copied the full velocity field to the host and
ran `curl` cell-by-cell in a scalar loop, which alone blew the frame budget
at megapixel sizes.
"""
function compute_vorticity!(scratch::RenderScratch, case::AbstractWaterLilyCase)
    simulation = case.simulation
    u = simulation.flow.u
    σ = simulation.flow.σ
    scale = eltype(σ)(simulation.L / simulation.U)
    WaterLily.@inside σ[I] = WaterLily.curl(3, I, u) * scale
    copyto!(scratch.padded_vorticity, σ)
    return scratch
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

blend_channel(a::UInt8, b::UInt8, fraction::Float64) =
    round(UInt8, Float64(a) + (Float64(b) - Float64(a)) * clamp(fraction, 0.0, 1.0))

blend_color(a::Tuple, b::Tuple, fraction::Float64) = (
    blend_channel(a[1], b[1], fraction),
    blend_channel(a[2], b[2], fraction),
    blend_channel(a[3], b[3], fraction),
)

# Linear interpolation between palette stops keeps the vorticity field a
# continuous gradient. Nearest-stop quantization used to draw hard iso-band
# contours that turned into blocky staircases once the compositor stretched
# the frame to the full display.
function palette_color(palette::Tuple, value::Real, scale::Real)
    normalized = clamp(Float64(value) / scale, -1.0, 1.0)
    position = (normalized + 1.0) * 0.5 * (length(palette) - 1)
    index = clamp(floor(Int, position), 0, length(palette) - 2)
    return blend_color(palette[index + 1], palette[index + 2], position - index)
end

seismic_color(value::Real, scale::Real) = palette_color(SEISMIC_PALETTE, value, scale)

"""
Colorize the vorticity snapshot in `scratch` into its RGBA buffer, using the
body pose at dimensionless time `τ`. The pose time is passed explicitly so
the worker can render one frame while the solver already advances the next.
"""
function render_rgba!(scratch::RenderScratch, case::AbstractWaterLilyCase, τ::Real)
    width, height = case.dimensions
    padded = scratch.padded_vorticity
    rgba = scratch.rgba
    color_scale = palette_scale(@view padded[2:(end - 1), 2:(end - 1)])
    palette = case_palette(case)
    body = body_color(case)
    bounds = body_bounds(case, τ)
    body_xmin, body_xmax, body_ymin, body_ymax =
        bounds === nothing ? (-Inf, Inf, -Inf, Inf) : Float64.(bounds)

    # Rows are emitted top-to-bottom. WaterLily's second coordinate increases
    # upward, hence the explicit vertical flip into the top-left protocol.
    Threads.@threads :static for row in 1:height
        y = height - row + 1
        py = y + 0.5
        row_may_touch_body = body_ymin <= py <= body_ymax
        output = (row - 1) * width * 4 + 1
        @inbounds for x in 1:width
            color = palette_color(palette, padded[x + 1, y + 1], color_scale)
            if row_may_touch_body && body_xmin <= x + 0.5 <= body_xmax
                distance = body_distance(case, x + 0.5, py, τ)
                # The signed distance doubles as pixel coverage: feathering
                # the body rim over about two source pixels anti-aliases the
                # edge before the compositor magnifies it.
                if distance <= -1.0
                    color = body
                elseif distance < 1.0
                    color = blend_color(color, body, 0.5 - distance * 0.5)
                end
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

function render_rgba(case::AbstractWaterLilyCase)
    scratch = RenderScratch(case.dimensions)
    compute_vorticity!(scratch, case)
    return render_rgba!(scratch, case, simulation_time(case))
end
