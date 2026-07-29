"""
Rain falling into a puddle that covers the whole canvas, reviving the
pre-WaterLily Rust postprocess (shallow-water ripples, drop impacts, gesture
wakes, foam) as a case in this framework. A damped wave equation runs on a
half-resolution grid: raindrops kick the surface with impulsive dimples whose
rings spread and interfere, the pointer drags a wake through the water, and
fast-moving crests froth into foam.

Rendering uses the compositor's water-lens contract end to end: alpha encodes
the surface height as optical thickness, so the shader refracts the live
desktop through the ripple slopes — the actual screen content sways under the
rain. The producer color only carries the water tint and foam, both kept
chroma-rich enough to stay clear of the frosted-mist keying.

Like the rain case this is a host-side model; the GPU `memory` backend of the
fluid cases is accepted and ignored.
"""
struct PuddleCase <: AbstractWaterLilyCase
    dimensions::Tuple{Int,Int}
    # Wave state on a half-resolution grid: surface height and its rate.
    # Half resolution quarters the cell count, and the compositor's bicubic
    # magnification hides the difference entirely for smooth wave fields.
    grid::Tuple{Int,Int}
    height::Matrix{Float32}
    rate::Matrix{Float32}
    curvature::Matrix{Float32}
    clock::Base.RefValue{Float64}
    # Previous pointer position in grid coordinates; NaN until the first
    # event so a fresh stream cannot inject one long catch-up wake.
    pointer::Base.RefValue{NTuple{2,Float32}}
    # Per-column and per-row factors of the separable ambient swell, refilled
    # each frame; the product form keeps the per-pixel cost to one multiply
    # instead of two transcendentals.
    swell_column::Vector{Float32}
    swell_row::Vector{Float32}
end

# Wave speed is in grid cells per second (one cell is two display pixels).
# The explicit leapfrog scheme is stable below c·Δt ≤ ~0.5 cells, which the
# substep count enforces per frame.
const PUDDLE_WAVE_SPEED = 60.0
const PUDDLE_MAX_STEP = 0.45 / PUDDLE_WAVE_SPEED
# Ring energy fades over a couple of seconds, like the old open lossy
# boundaries kept the pond from ringing forever.
const PUDDLE_DAMPING_SECONDS = 2.2
# Rings fade fast — damping plus 1/√r geometric spreading — so the rate is
# what keeps the pond alive: high enough that a dozen fresh rings are always
# in flight over a full display.
const PUDDLE_DROPS_PER_MEGAPIXEL = 24.0
const PUDDLE_FOAM_WHITE = (UInt8(0xf6), UInt8(0xf0), UInt8(0xc8))

function build_puddle_case(dimensions::Tuple{Int,Int}; memory=Array)
    _ = memory
    width, height = dimensions
    grid = (max(width ÷ 2, 8), max(height ÷ 2, 8))
    return PuddleCase(
        dimensions,
        grid,
        zeros(Float32, grid),
        zeros(Float32, grid),
        zeros(Float32, grid),
        Ref(0.0),
        Ref((NaN32, NaN32)),
        Vector{Float32}(undef, width),
        Vector{Float32}(undef, height),
    )
end

simulation_time(case::PuddleCase) = case.clock[]

case_palette_name(::PuddleCase) = "ocean"

body_distance(::PuddleCase, ::Real, ::Real, ::Real) = Inf

compute_vorticity!(scratch::RenderScratch, ::PuddleCase) = scratch

"""
Stamp a Mexican-hat dimple straight into the surface height: a hollow ringed
by a bulge, which the wave equation immediately starts propagating as an
expanding circular ripple. Displacing the height (rather than kicking the
rate) makes the impact visible the same frame at its full amplitude, and the
shape integrates to roughly zero so impacts do not pump net volume into the
pond.
"""
function splash!(case::PuddleCase, cx::Real, cy::Real, sigma::Real, amplitude::Real)
    grid_w, grid_h = case.grid
    surface = case.height
    σ = Float32(sigma)
    reach = 3.0f0 * σ
    x0 = clamp(floor(Int, cx - reach), 1, grid_w)
    x1 = clamp(ceil(Int, cx + reach), 1, grid_w)
    y0 = clamp(floor(Int, cy - reach), 1, grid_h)
    y1 = clamp(ceil(Int, cy + reach), 1, grid_h)
    inv2σ2 = 1.0f0 / (2.0f0 * σ * σ)
    @inbounds for y in y0:y1, x in x0:x1
        dx = Float32(x) - Float32(cx)
        dy = Float32(y) - Float32(cy)
        r2 = (dx * dx + dy * dy) * inv2σ2
        surface[x, y] -= Float32(amplitude) * (1.0f0 - r2) * exp(-r2)
    end
    return nothing
end

function puddle_step!(case::PuddleCase, dt::Float64)
    case.clock[] += dt
    grid_w, grid_h = case.grid
    height = case.height
    rate = case.rate
    curvature = case.curvature

    # Raindrops arrive as a Poisson stream over the display area.
    expected =
        PUDDLE_DROPS_PER_MEGAPIXEL * case.dimensions[1] * case.dimensions[2] / 1.0e6 * dt
    drops = rain_spawn_count(expected)
    for _ in 1:drops
        splash!(
            case,
            1 + rand(Float32) * (grid_w - 2),
            1 + rand(Float32) * (grid_h - 2),
            1.5f0 + 2.0f0 * rand(Float32),
            0.6f0 + 0.6f0 * rand(Float32),
        )
    end

    accel_dt = Float32(PUDDLE_WAVE_SPEED^2 * dt)
    damp = Float32(exp(-dt / PUDDLE_DAMPING_SECONDS))
    Threads.@threads :static for y in 2:(grid_h - 1)
        @inbounds for x in 2:(grid_w - 1)
            curvature[x, y] =
                height[x - 1, y] + height[x + 1, y] + height[x, y - 1] +
                height[x, y + 1] - 4.0f0 * height[x, y]
        end
    end
    Threads.@threads :static for y in 2:(grid_h - 1)
        @inbounds for x in 2:(grid_w - 1)
            updated = (rate[x, y] + accel_dt * curvature[x, y]) * damp
            rate[x, y] = updated
            height[x, y] += updated * Float32(dt)
        end
    end
    # Open boundaries: edges copy their inner neighbour, absorbing outgoing
    # rings instead of reflecting them back across the pond.
    @inbounds for y in 1:grid_h
        height[1, y] = height[2, y]
        height[grid_w, y] = height[grid_w - 1, y]
        rate[1, y] = rate[2, y]
        rate[grid_w, y] = rate[grid_w - 1, y]
    end
    @inbounds for x in 1:grid_w
        height[x, 1] = height[x, 2]
        height[x, grid_h] = height[x, grid_h - 1]
        rate[x, 1] = rate[x, 2]
        rate[x, grid_h] = rate[x, grid_h - 1]
    end
    return case
end

function advance_budgeted!(case::PuddleCase, dimensionless_step::Real, deadline_ns::UInt64)
    remaining = Float64(dimensionless_step)
    achieved = 0.0
    while remaining > 0.0
        dt = min(remaining, PUDDLE_MAX_STEP)
        puddle_step!(case, dt)
        achieved += dt
        remaining -= dt
        time_ns() >= deadline_ns && break
    end
    return achieved
end

function advance!(case::PuddleCase, dimensionless_step::Real)
    advance_budgeted!(case, dimensionless_step, typemax(UInt64))
    return case
end

"""
Drag a wake through the water: impulses along the pointer's path since the
last event, spaced so a fast flick reads as a continuous furrow rather than
a dotted line of pokes.
"""
function handle_pointer!(case::PuddleCase, x::Real, y::Real)
    grid_w, grid_h = case.grid
    gx = clamp(Float32(x) * grid_w, 1.0f0, Float32(grid_w))
    gy = clamp(Float32(y) * grid_h, 1.0f0, Float32(grid_h))
    px, py = case.pointer[]
    case.pointer[] = (gx, gy)
    isnan(px) && return nothing
    distance = hypot(gx - px, gy - py)
    steps = max(1, ceil(Int, distance / 2.0f0))
    for step in 1:steps
        f = Float32(step) / steps
        splash!(case, px + (gx - px) * f, py + (gy - py) * f, 2.2f0, 0.35f0)
    end
    return nothing
end

"""
Colorize the pond. Alpha maps the bilinearly upsampled surface height into
the lens-thickness contract, so the refraction the compositor computes from
its gradient is exactly the ripple slope; the color carries a palette-derived
water tint with foam on fast-moving crests.
"""
function render_rgba!(
    scratch::RenderScratch,
    case::PuddleCase,
    τ::Real;
    palette::Tuple=case_palette(case),
    shimmer::Bool=false,
)
    _ = τ
    _ = shimmer
    width, height_px = case.dimensions
    grid_w, grid_h = case.grid
    rgba = scratch.rgba
    surface = case.height
    rate = case.rate

    tint = blend_color(palette[3], (UInt8(0x3c), UInt8(0x46), UInt8(0x52)), 0.45)
    deep = blend_color(palette[2], (UInt8(0x28), UInt8(0x30), UInt8(0x3a)), 0.5)
    glint = blend_color(palette[5], (UInt8(0xff), UInt8(0xf4), UInt8(0xd0)), 0.5)

    # A gentle broad swell keeps the pond alive between raindrops. It only
    # exists in the render: two slow traveling sine factors whose product
    # modulates the height field, never touching the wave state.
    τf = Float32(simulation_time(case))
    swell_column = case.swell_column
    swell_row = case.swell_row
    @inbounds for x in 1:width
        swell_column[x] = sin(0.011f0 * x + 1.1f0 * τf)
    end
    @inbounds for y in 1:height_px
        swell_row[y] = sin(0.008f0 * y - 0.7f0 * τf)
    end

    scale_x = Float32(grid_w) / width
    scale_y = Float32(grid_h) / height_px
    Threads.@threads :static for row in 1:height_px
        gy = clamp((Float32(row) - 0.5f0) * scale_y + 0.5f0, 1.0f0, Float32(grid_h))
        iy = min(floor(Int, gy), grid_h - 1)
        fy = gy - iy
        output = (row - 1) * width * 4 + 1
        @inbounds for x in 1:width
            gx = clamp((Float32(x) - 0.5f0) * scale_x + 0.5f0, 1.0f0, Float32(grid_w))
            ix = min(floor(Int, gx), grid_w - 1)
            fx = gx - ix
            w00 = (1 - fx) * (1 - fy)
            w10 = fx * (1 - fy)
            w01 = (1 - fx) * fy
            w11 = fx * fy
            h = surface[ix, iy] * w00 + surface[ix + 1, iy] * w10 +
                surface[ix, iy + 1] * w01 + surface[ix + 1, iy + 1] * w11 +
                0.12f0 * swell_column[x] * swell_row[row]
            churn = rate[ix, iy] * w00 + rate[ix + 1, iy] * w10 +
                    rate[ix, iy + 1] * w01 + rate[ix + 1, iy + 1] * w11

            # Crests thicken the lens (lower alpha), troughs thin it; the
            # gain is what turns wave slopes into visible refraction, so it
            # runs high and the clamp keeps every pixel inside the shader's
            # lens branch.
            alpha = round(UInt8, clamp(52.0f0 - 90.0f0 * h, 8.0f0, 235.0f0))
            depth = clamp(0.5 - 0.6 * Float64(h), 0.0, 1.0)
            color = blend_color(tint, deep, depth)
            # Crests catch the light: without the sheen a ring is pure
            # refraction, invisible over flat scene content. The alpha floor
            # gives the pale color enough weight to read while the trough
            # side stays dark glass.
            sheen = rain_smoothstep(0.06f0, 0.4f0, h)
            if sheen > 0.01f0
                color = blend_color(color, glint, 0.85 * Float64(sheen))
                alpha = max(alpha, round(UInt8, 130.0f0 * sheen))
            end
            foam = rain_smoothstep(6.0f0, 16.0f0, abs(churn))
            if foam > 0.01f0
                color = blend_color(color, PUDDLE_FOAM_WHITE, 0.85 * Float64(foam))
                alpha = blend_channel(alpha, UInt8(0xe6), Float64(foam))
            end

            rgba[output] = color[1]
            rgba[output + 1] = color[2]
            rgba[output + 2] = color[3]
            rgba[output + 3] = alpha
            output += 4
        end
    end
    return rgba
end
