"""
One raindrop stuck to (or running down) the misted glass. Radii are in
simulation pixels with a top-left origin, so gravity is simply +y; velocities
are pixels per second of wall-clock-paced simulation time.
"""
mutable struct Raindrop
    x::Float32
    y::Float32
    radius::Float32
    speed::Float32
    sliding::Bool
    # Contact-line pinning threshold with per-drop jitter: a drop releases
    # once its radius grows past this, and re-pins well below it (hysteresis),
    # like real contact-angle hysteresis makes drops stick, then suddenly run.
    release_radius::Float32
    meander_phase::Float32
    meander_rate::Float32
    # Distance left to travel before shedding the next residual droplet.
    deposit_debt::Float32
end

"""
Rain on a fogged window, as a droplet particle model instead of a WaterLily
solve: drops hit the glass and pin, grow by condensation and merging, then
release past a critical size and run down, meandering, feeding on drops in
their path and leaving a wet trail plus residual droplets behind.

Rendering leans on the compositor keying contract: the misted pane is the
near-white key color, so it frosts out to the blurred scene, while wet trails
and drop interiors are emitted dark and translucent so the *sharp* scene
shows through them — the rain literally wipes the frost clear. Pointer events
wipe the mist by hand; the fog then slowly re-forms.

The model is host-side and cheap (a few hundred particles plus one wetness
field), so the `memory` backend of the GPU cases is accepted and ignored.
"""
struct RainCase <: AbstractWaterLilyCase
    dimensions::Tuple{Int,Int}
    drops::Vector{Raindrop}
    # Per-pixel wetness in [0, 1]: 0 is fully misted glass, 1 is wiped/wet
    # and therefore clear. Trails and pointer wipes raise it; it decays as
    # the fog re-forms.
    wetness::Matrix{Float32}
    # Static mist mottling baked at build time so the frost is not one flat
    # key color; the amplitude stays small enough to remain inside the
    # compositor's bright/low-chroma keying band.
    grain::Matrix{UInt8}
    clock::Base.RefValue{Float64}
    # Random phases for the two incommensurate gust tones that modulate the
    # spawn rate, so showers come in bursts instead of a constant drizzle.
    gust_phases::NTuple{2,Float64}
    # Pixel scale relative to the 800-tall reference canvas, floored so the
    # tiny test canvases still produce visible drops.
    unit::Float32
end

# All rates are per second of simulation time and scale with `unit`, so the
# look is resolution- and fps-independent.
const RAIN_REFERENCE_HEIGHT = 800.0
const RAIN_REGROW_SECONDS = 40.0
const RAIN_SPAWN_PER_MEGAPIXEL = 20.0
const RAIN_RUNNER_FRACTION = 0.012
const RAIN_CONDENSE_RATE = 0.06
const RAIN_SLIDE_SPEED = 95.0
const RAIN_MAX_SUBSTEP = 1.0 / 24.0
const RAIN_GUST_PERIODS = (47.0, 13.0)

# Warm ivory specular dot: bright but chroma-rich, so the compositor's
# bright/low-chroma key never frosts the highlight out.
const RAIN_HIGHLIGHT = (UInt8(0xf7), UInt8(0xee), UInt8(0xc6))
const RAIN_KEY_WHITE = (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd))

rain_unit(height::Int) = Float32(max(0.35, height / RAIN_REFERENCE_HEIGHT))

rain_release_radius(unit::Float32) = unit * (5.6f0 + 2.4f0 * rand(Float32))

rain_meander_rate() = Float32(0.4 + 1.0 * rand())

function rain_stuck_drop(x::Real, y::Real, radius::Real, unit::Float32)
    return Raindrop(
        Float32(x),
        Float32(y),
        Float32(radius),
        0.0f0,
        false,
        rain_release_radius(unit),
        Float32(2pi) * rand(Float32),
        rain_meander_rate(),
        0.0f0,
    )
end

"""
Blend two coarse random grids into a static per-pixel mottle. Pure white
noise sparkles when the compositor magnifies the frame, so most of the
amplitude comes from a bilinearly smoothed coarse grid with only a small
per-pixel speck on top.
"""
function rain_grain(width::Int, height::Int)
    coarse_step = 9
    coarse = rand(Float32, cld(width, coarse_step) + 2, cld(height, coarse_step) + 2)
    grain = Matrix{UInt8}(undef, width, height)
    for y in 1:height
        gy = (y - 1) / coarse_step + 1
        iy = floor(Int, gy)
        fy = Float32(gy - iy)
        for x in 1:width
            gx = (x - 1) / coarse_step + 1
            ix = floor(Int, gx)
            fx = Float32(gx - ix)
            smooth =
                coarse[ix, iy] * (1 - fx) * (1 - fy) +
                coarse[ix + 1, iy] * fx * (1 - fy) +
                coarse[ix, iy + 1] * (1 - fx) * fy +
                coarse[ix + 1, iy + 1] * fx * fy
            grain[x, y] =
                round(UInt8, 3.0f0 * (0.85f0 * smooth + 0.15f0 * rand(Float32)))
        end
    end
    return grain
end

function build_rain_case(dimensions::Tuple{Int,Int}; memory=Array)
    # The droplet model runs on the host regardless of the selected backend.
    _ = memory
    width, height = dimensions
    unit = rain_unit(height)
    drops = Raindrop[]
    # Seed a pane that has already been rained on for a while, so the first
    # published frame shows drops instead of bare fog. Sizes stay below the
    # release threshold; the first runners appear as condensation and merges
    # push drops over it.
    seeds = max(6, round(Int, 120 * width * height / 1.0e6))
    for _ in 1:seeds
        radius = unit * (1.4f0 + 3.8f0 * rand(Float32)^2.2f0)
        push!(
            drops,
            rain_stuck_drop(rand(Float32) * width, rand(Float32) * height, radius, unit),
        )
    end
    return RainCase(
        dimensions,
        drops,
        zeros(Float32, width, height),
        rain_grain(width, height),
        Ref(0.0),
        (2pi * rand(), 2pi * rand()),
        unit,
    )
end

rain_max_drops(case::RainCase) =
    clamp(round(Int, 550 * case.dimensions[1] * case.dimensions[2] / 1.0e6), 60, 900)

simulation_time(case::RainCase) = case.clock[]

case_palette_name(::RainCase) = "glacier"

# The pane has no immersed body; the interface hooks that assume one are
# satisfied trivially so shared tooling can still call them.
body_distance(::RainCase, ::Real, ::Real, ::Real) = Inf

compute_vorticity!(scratch::RenderScratch, ::RainCase) = scratch

function sample_wetness(case::RainCase, x::Real, y::Real)
    width, height = case.dimensions
    return case.wetness[clamp(round(Int, x), 1, width), clamp(round(Int, y), 1, height)]
end

"""
Additively wet a disk of the pane, clamped to full wetness, with a quadratic
falloff toward the edge so wipes and trails feather into the mist instead of
punching hard-edged holes. Used by drop impacts, sliding trails, and pointer
wipes alike; the center always receives the full `amount`.
"""
function stamp_wetness!(case::RainCase, cx::Real, cy::Real, radius::Real, amount::Real)
    width, height = case.dimensions
    wetness = case.wetness
    r = Float32(radius)
    add = Float32(amount)
    x0 = clamp(floor(Int, cx - r), 1, width)
    x1 = clamp(ceil(Int, cx + r), 1, width)
    y0 = clamp(floor(Int, cy - r), 1, height)
    y1 = clamp(ceil(Int, cy + r), 1, height)
    r2 = r * r
    @inbounds for y in y0:y1, x in x0:x1
        dx = Float32(x) - Float32(cx)
        dy = Float32(y) - Float32(cy)
        d2 = dx * dx + dy * dy
        if d2 <= r2
            falloff = 1.0f0 - d2 / r2
            wetness[x, y] = min(1.0f0, wetness[x, y] + add * falloff)
        end
    end
    return nothing
end

function rain_spawn_count(expected::Float64)
    count = floor(Int, expected)
    rand() < expected - count && (count += 1)
    return count
end

function rain_gust(case::RainCase, t::Float64)
    a = sin(2pi * t / RAIN_GUST_PERIODS[1] + case.gust_phases[1])
    b = sin(2pi * t / RAIN_GUST_PERIODS[2] + case.gust_phases[2])
    return 0.6 + 0.4 * a * b
end

function rain_spawn!(case::RainCase, spawned::Vector{Raindrop}, dt::Float64)
    width, height = case.dimensions
    unit = case.unit
    rate = RAIN_SPAWN_PER_MEGAPIXEL * width * height / 1.0e6 *
           rain_gust(case, case.clock[])
    budget = rain_max_drops(case) - length(case.drops) - length(spawned)
    for _ in 1:min(rain_spawn_count(rate * dt), max(budget, 0))
        x = rand(Float32) * width
        y = rand(Float32) * height
        if rand() < RAIN_RUNNER_FRACTION
            # A heavy impact: born above the pinning threshold, it starts
            # running immediately, like the streak heads in real rain.
            drop = rain_stuck_drop(x, y, 0.0f0, unit)
            drop.radius = drop.release_radius * (1.05f0 + 0.25f0 * rand(Float32))
            drop.sliding = true
            drop.deposit_debt = (4.0f0 + 8.0f0 * rand(Float32)) * drop.radius
            push!(spawned, drop)
        else
            radius = unit * (1.4f0 + 3.8f0 * rand(Float32)^2.2f0)
            push!(spawned, rain_stuck_drop(x, y, radius, unit))
        end
        # The impact knocks a small halo of mist off the glass.
        radius = last(spawned).radius
        stamp_wetness!(case, x, y, 1.7f0 * radius, 0.5f0)
    end
    return nothing
end

function rain_slide!(case::RainCase, drop::Raindrop, spawned::Vector{Raindrop}, dt::Float64)
    unit = case.unit
    dtf = Float32(dt)
    # Wet paths drain faster: a drop that finds an existing rivulet zips
    # down it, which is what carves the long shared channels on real glass.
    wet_ahead = sample_wetness(case, drop.x, drop.y + 2.0f0 * drop.radius)
    surplus = max(drop.radius / drop.release_radius - 0.62f0, 0.0f0)
    target = Float32(RAIN_SLIDE_SPEED) * unit * (0.25f0 + 1.5f0 * surplus) *
             (1.0f0 + 0.8f0 * wet_ahead)
    drop.speed += (target - drop.speed) * min(1.0f0, 3.5f0 * dtf)

    drop.meander_phase += drop.meander_rate * dtf
    rand() < 0.7 * dt && (drop.meander_rate = rain_meander_rate())
    wet_left = sample_wetness(case, drop.x - 1.8f0 * drop.radius, drop.y + drop.radius)
    wet_right = sample_wetness(case, drop.x + 1.8f0 * drop.radius, drop.y + drop.radius)
    # Real rivulets run mostly straight with gentle bends; keep the meander
    # and the pull toward existing wet channels subtle or the pane turns
    # into branching zigzags.
    drift = 0.18f0 * sin(drop.meander_phase) + 0.35f0 * (wet_right - wet_left)

    old_x, old_y = drop.x, drop.y
    drop.y += drop.speed * dtf
    drop.x = clamp(drop.x + drift * drop.speed * dtf, -2.0f0 * drop.radius,
                   Float32(case.dimensions[1]) + 2.0f0 * drop.radius)

    # Stamp the wet trail densely enough that fast drops leave a continuous
    # channel instead of a dotted line.
    moved = hypot(drop.x - old_x, drop.y - old_y)
    steps = max(1, ceil(Int, moved))
    for step in 1:steps
        f = Float32(step) / steps
        stamp_wetness!(
            case,
            old_x + (drop.x - old_x) * f,
            old_y + (drop.y - old_y) * f,
            0.5f0 * drop.radius,
            1.0f0,
        )
    end

    # Mass sheds into the trail film continuously, and every so often into a
    # residual droplet left hanging behind the runner.
    drop.radius -= Float32(moved) * 0.0035f0 * unit
    drop.deposit_debt -= Float32(moved)
    if drop.deposit_debt <= 0.0f0 && drop.radius > 2.0f0 * unit
        residual = drop.radius * (0.26f0 + 0.14f0 * rand(Float32))
        jitter = (rand(Float32) - 0.5f0) * drop.radius
        push!(
            spawned,
            rain_stuck_drop(drop.x + jitter, drop.y - 1.4f0 * drop.radius, residual, unit),
        )
        drop.radius = cbrt(max(drop.radius^3 - residual^3, 0.0f0))
        drop.deposit_debt = (5.0f0 + 9.0f0 * rand(Float32)) * drop.radius
    end

    # Re-pin well below the release threshold: the hysteresis band is what
    # lets a drained runner stop and hang instead of flickering.
    if drop.radius <= 0.62f0 * drop.release_radius
        drop.sliding = false
        drop.speed = 0.0f0
    end
    return nothing
end

function rain_merge!(case::RainCase)
    drops = case.drops
    count = length(drops)
    @inbounds for i in 1:count
        a = drops[i]
        a.radius <= 0.0f0 && continue
        for j in (i + 1):count
            b = drops[j]
            b.radius <= 0.0f0 && continue
            reach = 0.82f0 * (a.radius + b.radius)
            dx = a.x - b.x
            abs(dx) > reach && continue
            dy = a.y - b.y
            dx * dx + dy * dy > reach * reach && continue

            into, from = a.radius >= b.radius ? (a, b) : (b, a)
            total = into.radius^3 + from.radius^3
            weight = from.radius^3 / total
            into.x += (from.x - into.x) * weight
            into.y += (from.y - into.y) * weight
            into.radius = min(cbrt(total), 10.5f0 * case.unit)
            into.speed = max(into.speed, from.speed)
            # A merge often jolts the pair loose even slightly below the
            # threshold — the sudden "two drops become one and run" moment.
            if into.radius >= 0.88f0 * min(a.release_radius, b.release_radius)
                into.sliding = true
                into.deposit_debt <= 0.0f0 &&
                    (into.deposit_debt = 6.0f0 * into.radius)
            end
            from.radius = 0.0f0
            a.radius <= 0.0f0 && break
        end
    end
    return nothing
end

function step_rain!(case::RainCase, dt::Float64)
    case.clock[] += dt
    width, height = case.dimensions
    unit = case.unit

    # The fog slowly re-forms everywhere; trails and wipes fade back to mist.
    decay = Float32(dt / RAIN_REGROW_SECONDS)
    wetness = case.wetness
    @. wetness = max(wetness - decay, 0.0f0)

    spawned = Raindrop[]
    rain_spawn!(case, spawned, dt)
    for drop in case.drops
        if drop.sliding
            rain_slide!(case, drop, spawned, dt)
        else
            # Condensation feeds the drop through its surface, so growth
            # accelerates with size: small drops effectively never release on
            # their own and merging stays the main path over the threshold.
            # A linear rate here turns every stuck drop into a runner within
            # a couple of minutes and shreds the pane into streaks.
            fraction = drop.radius / drop.release_radius
            drop.radius += Float32(RAIN_CONDENSE_RATE * dt) * unit * fraction * fraction
            if drop.radius >= drop.release_radius
                drop.sliding = true
                drop.speed = 0.0f0
                drop.deposit_debt = (4.0f0 + 8.0f0 * rand(Float32)) * drop.radius
            end
        end
    end
    rain_merge!(case)
    filter!(
        drop -> drop.radius > 0.55f0 * unit &&
                drop.y <= height + 2.0f0 * drop.radius,
        case.drops,
    )
    append!(case.drops, spawned)
    return case
end

function advance_budgeted!(case::RainCase, dimensionless_step::Real, deadline_ns::UInt64)
    remaining = Float64(dimensionless_step)
    achieved = 0.0
    while remaining > 0.0
        dt = min(remaining, RAIN_MAX_SUBSTEP)
        step_rain!(case, dt)
        achieved += dt
        remaining -= dt
        time_ns() >= deadline_ns && break
    end
    return achieved
end

function advance!(case::RainCase, dimensionless_step::Real)
    advance_budgeted!(case, dimensionless_step, typemax(UInt64))
    return case
end

"""
Wipe the mist with the pointer: a clear swath opens under the cursor and any
drops in it are swept away with the wipe. The fog then re-forms over it at
the usual pace.
"""
function handle_pointer!(case::RainCase, x::Real, y::Real)
    width, height = case.dimensions
    px = Float32(x) * width
    py = Float32(y) * height
    wipe = 0.055f0 * height
    stamp_wetness!(case, px, py, wipe, 1.0f0)
    filter!(drop -> hypot(drop.x - px, drop.y - py) > wipe + drop.radius, case.drops)
    return nothing
end

rain_smoothstep(edge0::Float32, edge1::Float32, x::Float32) =
    (t = clamp((x - edge0) / (edge1 - edge0), 0.0f0, 1.0f0); t * t * (3.0f0 - 2.0f0 * t))

"""
Colorize the pane. Unlike the fluid cases this canvas is authored directly in
the top-left display orientation, so rows map to output rows without the
vertical flip, and the vorticity scratch is ignored.

Mist pixels sit on the key white so the compositor swaps them for its
frosted blur. Everything wet uses the shader's water-lens contract: alpha
encodes optical thickness (255 dry, small values a thick drop core), and the
compositor refracts the sharp scene through the alpha gradient, tinting by
the producer color at the producer alpha. Drop domes therefore act as real
lenses over whatever is behind the canvas, and trails read as slightly
darkened clear streaks. `palette` only tints the water, so hot-swapping
palettes retints the pane subtly.
"""
function render_rgba!(
    scratch::RenderScratch,
    case::RainCase,
    τ::Real;
    palette::Tuple=case_palette(case),
    shimmer::Bool=false,
)
    _ = τ
    _ = shimmer
    width, height = case.dimensions
    rgba = scratch.rgba
    wetness = case.wetness
    grain = case.grain

    # Water tints derived from the active palette's cold stops, pulled toward
    # neutral slate so any palette reads as glass rather than dyed liquid.
    film = blend_color(palette[4], (UInt8(0x4a), UInt8(0x54), UInt8(0x60)), 0.7)
    rim = blend_color(palette[2], (UInt8(0x30), UInt8(0x36), UInt8(0x3e)), 0.55)
    deep = blend_color(palette[3], (UInt8(0x38), UInt8(0x40), UInt8(0x48)), 0.55)
    light = blend_color(palette[5], (UInt8(0xc8), UInt8(0xd2), UInt8(0xda)), 0.35)

    Threads.@threads :static for row in 1:height
        output = (row - 1) * width * 4 + 1
        @inbounds for x in 1:width
            g = grain[x, row]
            mist = (RAIN_KEY_WHITE[1] - g, RAIN_KEY_WHITE[2] - g, RAIN_KEY_WHITE[3] - (g >> 1))
            w = wetness[x, row]
            # Only well-wetted glass reads as clear; the sharp ramp keeps the
            # pane bimodal — frosted or clear — because the halfway colors are
            # neither keyed by the compositor nor transparent and would smear
            # every fading trail into an opaque milky streak.
            # Full clearness must sit well below a single feathered trail
            # stamp's peak: with the threshold above it, only the stamp
            # centerline ever cleared and every trail rendered as a hollow
            # outline with a bright transition band filling its body.
            clear = rain_smoothstep(0.45f0, 0.72f0, w)
            if clear > 0.004f0
                # A thin water film: mostly refraction-free (flat alpha), so
                # the streak is the sharp scene slightly darkened by the film
                # tint, with lens shimmer only along its feathered edges. The
                # squared ramp holds the film color dark through most of the
                # fade — the lens contract shows the producer color at the
                # producer alpha, so a color that drifted toward white early
                # would paint every aging trail as a milky streak.
                fraction = Float64(clear)
                color = blend_color(film, mist, (1.0 - fraction)^2)
                alpha = round(UInt8, 255 - 200 * fraction)
            else
                color = mist
                alpha = 0xff
            end
            rgba[output] = color[1]
            rgba[output + 1] = color[2]
            rgba[output + 2] = color[3]
            rgba[output + 3] = alpha
            output += 4
        end
    end

    # The drop pass is serial: the total lensed area is a few tens of
    # thousands of pixels, far below the threaded background pass.
    feather = 1.3f0
    for drop in case.drops
        r = drop.radius
        r < 0.3f0 && continue
        # Runners stretch into a falling-teardrop ellipse with the speed.
        stretch = drop.sliding ?
                  1.0f0 + min(0.9f0, drop.speed / (140.0f0 * case.unit)) : 1.0f0
        rx = r / sqrt(stretch)
        ry = r * stretch
        rmin = min(rx, ry)
        x0 = max(1, floor(Int, drop.x - rx - feather))
        x1 = min(width, ceil(Int, drop.x + rx + feather))
        y0 = max(1, floor(Int, drop.y - ry - feather))
        y1 = min(height, ceil(Int, drop.y + ry + feather))
        @inbounds for py in y0:y1
            ny = (Float32(py) - 0.5f0 - drop.y) / ry
            vert = clamp(ny, -1.0f0, 1.0f0)
            # The interior works as a lens: brighter toward the bottom where
            # a real drop gathers the sky, darker toward the top.
            interior = blend_color(deep, light, 0.5 + 0.4 * Float64(vert))
            output_row = (py - 1) * width * 4
            for px in x0:x1
                nx = (Float32(px) - 0.5f0 - drop.x) / rx
                q = sqrt(nx * nx + ny * ny)
                d = (q - 1.0f0) * rmin
                d >= feather && continue
                coverage = clamp(0.5f0 - d / feather, 0.0f0, 1.0f0)

                # Alpha carries the spherical-cap thickness: near-clear at
                # the core, dry at the edge. Its gradient is what the
                # compositor shader turns into the lens distortion, steepest
                # right at the rim like a real drop.
                dome = sqrt(max(1.0f0 - q * q, 0.0f0))
                alpha = round(UInt8, 255.0f0 - 217.0f0 * dome)
                rim_mix = rain_smoothstep(0.8f0, 1.0f0, q)
                color = blend_color(interior, rim, 0.9 * Float64(rim_mix))

                # Off-center specular dot toward the upper light; a high
                # alpha makes the tint opaque and locally flattens the lens.
                # The glint fades as a runner stretches — an elongated head
                # with a full-strength highlight reads as a glowing blob.
                hx = (nx + 0.34f0) / 0.26f0
                hy = (ny + 0.38f0) / 0.30f0
                h2 = hx * hx + hy * hy
                if h2 < 1.0f0
                    s = (1.0f0 - h2) * (1.0f0 - h2) / (stretch * stretch)
                    color = blend_color(color, RAIN_HIGHLIGHT, 0.9 * Float64(s))
                    alpha = blend_channel(alpha, UInt8(0xfa), Float64(s))
                end

                index = output_row + (px - 1) * 4 + 1
                background = (rgba[index], rgba[index + 1], rgba[index + 2])
                # The color hugs the drop farther out than the alpha ramp:
                # feather pixels whose color drifted halfway to the mist
                # while their alpha was already high rendered as a bright
                # ring around every drop on dark scenes. Keeping the edge
                # dark reads as the drop's contact shadow instead.
                blended = blend_color(background, color, sqrt(Float64(coverage)))
                rgba[index] = blended[1]
                rgba[index + 1] = blended[2]
                rgba[index + 2] = blended[3]
                rgba[index + 3] = blend_channel(rgba[index + 3], alpha, Float64(coverage))
            end
        end
    end
    return rgba
end
