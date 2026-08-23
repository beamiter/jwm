# Water and glass properties in SI units. Every rule below is a force or a
# flux written in these units and converted to canvas pixels exactly once,
# through `case.resolution`, so the look is resolution independent and the
# constants can be checked against the literature instead of retuned by eye.
const RAIN_WATER_DENSITY = 998.0f0          # kg m^-3 at 20 degrees C
const RAIN_SURFACE_TENSION = 0.0728f0       # N m^-1
const RAIN_VISCOSITY = 1.002f-3             # Pa s
const RAIN_GRAVITY = 9.81f0                 # m s^-2
# Contact-angle hysteresis of water on soda-lime glass that already carries a
# mist of condensate. The gap between the advancing and receding angles is
# the entire reason a drop can hang on a vertical pane at all.
const RAIN_ANGLE_ADVANCING = Float32(deg2rad(80.0))
const RAIN_ANGLE_RECEDING = Float32(deg2rad(55.0))
const RAIN_ANGLE_MEAN = 0.5f0 * (RAIN_ANGLE_ADVANCING + RAIN_ANGLE_RECEDING)
const RAIN_HYSTERESIS = cos(RAIN_ANGLE_RECEDING) - cos(RAIN_ANGLE_ADVANCING)
# Molecular slip length that regularizes the contact-line stress singularity
# in the Cox-Voinov wedge. It only ever enters through a logarithm.
const RAIN_SLIP_LENGTH = 1.0f-8             # m

# Spherical-cap geometry: a drop with contact radius `a` and contact angle
# `θ` has volume `π/3 · f(θ) · a³`. Carrying the shape factor explicitly is
# what lets merges, pearls and film losses conserve volume instead of radius.
const RAIN_CAP_FACTOR =
    (1 - cos(RAIN_ANGLE_MEAN))^2 * (2 + cos(RAIN_ANGLE_MEAN)) / sin(RAIN_ANGLE_MEAN)^3
const RAIN_VOLUME_COEFFICIENT = Float32(pi / 3) * RAIN_CAP_FACTOR

# Lumped contact-line dissipation. The Cox-Voinov wedge alone contributes
# about 6 (a factor 3 for each of the advancing and receding lines, over a
# contact width of 2a); the recirculating bulk flow inside a millimetric drop
# dissipates at least as much again, and the wedge cutoff ratio is uncertain
# by an order of magnitude inside its logarithm. The value here is calibrated
# so 10-50 microlitre drops run at the 5-20 cm/s measured on vertical glass.
const RAIN_DISSIPATION = 40.0f0
# The pane's physical height. 260 mm of glass across the canvas puts the
# ~2.2 mm critical contact radius at ~7 px on an 800-tall frame.
const RAIN_PANE_HEIGHT = 0.26f0             # m
# Tiny canvases (tests, thumbnails) represent a smaller patch of the same
# glass instead of shrinking the physics into sub-pixel drops.
const RAIN_MIN_RESOLUTION = 1200.0f0        # px m^-1

# Marshall-Palmer raindrop size distribution: N(D) = N0 exp(-Λ D) with
# Λ = 4.1 R^-0.21 mm^-1 for a rain rate R in mm/h.
const RAIN_MP_N0 = 8.0f6                    # m^-3 m^-1 (8000 m^-3 mm^-1)
const RAIN_MP_LAMBDA_COEFFICIENT = 4100.0f0 # m^-1
const RAIN_RATE = 3.5f0                     # mm/h, a steady shower
const RAIN_WIND_SPEED = 2.5f0               # m/s normal component onto the pane
# Impacts below this diameter are the mist: they are folded into the
# condensation term rather than tracked as individual drops.
const RAIN_TRACKED_CUTOFF = 1.2f-3          # m
# Cossali-Mundo splash parameter threshold on a wetted surface.
const RAIN_SPLASH_PARAMETER = 57.7f0

# Diffusion-limited condensation: the flux onto a cap goes like its radius,
# so `da/dt = K/a` and `a(t) = sqrt(a0² + 2Kt)`. Small drops grow quickly and
# large ones stall, which is why coalescence — not condensation — is what
# carries most drops over the release threshold, exactly as in a real breath
# figure.
const RAIN_CONDENSATION = 2.5f-8            # m² s^-1
# A cleared path has no mist left to feed on until the fog re-forms over it.
const RAIN_VAPOUR_DEPLETION = 0.6f0
# A pre-wetted path has markedly less contact-angle hysteresis, which is what
# makes runners fall into and follow existing channels.
const RAIN_WET_RELIEF = 0.45f0
const RAIN_REGROW_SECONDS = 40.0f0
# A half-wetted film is spinodally unstable: once it thins past a few tens of
# nanometres it dewets in a rush rather than fading. That matters visually as
# much as physically — the compositor keys mist only at alpha >= 0.97 and
# refracts water at low alpha, so a pixel caught between the two states is
# painted as opaque producer color. Crossing that band quickly is what keeps
# an aging trail from reading as a milky slab.
const RAIN_DEWET_LOW = 0.42f0
const RAIN_DEWET_HIGH = 0.58f0
const RAIN_DEWET_RATE = 8.0f0
# Rayleigh-Plateau breakup of the deposited rivulet, in trail widths.
const RAIN_PEARL_SPACING = 3.5f0
const RAIN_PEARL_YIELD = 0.55f0
const RAIN_MAX_SUBSTEP = 1.0 / 48.0
const RAIN_GUST_PERIODS = (47.0, 13.0)
const RAIN_STOP_SPEED = 0.5f0               # px/s below which a drop re-pins
# Motion blur: the frame integrates the drop over roughly this exposure, so
# fast runners read as streaks the way they do to the eye and to a camera.
const RAIN_EXPOSURE = 1.0f0 / 50.0f0        # s
const RAIN_TEARDROP = 60.0f0                # capillary elongation per unit Ca

# Warm ivory specular dot: bright but chroma-rich, so the compositor's
# bright/low-chroma key never frosts the highlight out.
const RAIN_HIGHLIGHT = (UInt8(0xf7), UInt8(0xee), UInt8(0xc6))
const RAIN_KEY_WHITE = (UInt8(0xfa), UInt8(0xfa), UInt8(0xfd))

"""
One drop of water sitting on (or running down) the pane. Positions and
velocities are in canvas pixels with a top-left origin, so gravity is simply
+y; radii are contact-line radii, also in pixels. Forces are computed in SI
and converted through `case.resolution` at the one place it matters.
"""
mutable struct Raindrop
    x::Float32
    y::Float32
    # Where the drop was when the current substep began, so coalescence tests
    # sweep the whole travelled segment instead of sampling its endpoints. A
    # fast runner would otherwise tunnel straight through a pinned drop.
    previous_x::Float32
    previous_y::Float32
    radius::Float32
    speed::Float32              # downslope velocity, px/s, never negative
    lateral::Float32            # cross-slope velocity, px/s, signed
    sliding::Bool               # cached from the force balance for rendering
    # Per-drop multiplier on the contact-angle hysteresis: two drops of the
    # same size pinned on the same glass still release at slightly different
    # radii because their contact lines sit on different defects.
    pinning::Float32
    # Pixels of travel left before the trailing rivulet beads up, and the
    # volume deposited into it since the last pearl.
    deposit_debt::Float32
    rivulet::Float32            # m^3
end

"""
Rain on a fogged pane, simulated as a Lagrangian population of sessile drops
whose motion follows a real force balance rather than an authored animation
curve:

  * A drop stays pinned while gravity `ρVg` is below the Furmidge retention
    force `σ·2a·(cos θ_r − cos θ_a)`. That balance sets the critical contact
    radius — about 2.2 mm for water on glass — instead of a tuned threshold.
  * A moving drop integrates `m·dU/dt = ρVg − F_retention − k_v·U` with
    Cox-Voinov contact-line dissipation `k_v ∝ μ·a·ln(a/λ)/θ`, so it
    accelerates over its own inertial time (~0.1 s), reaches a terminal
    velocity set by its size, and re-pins on its own once draining drops it
    back under the retention force. The hysteresis loop is the physics, not
    a state flag.
  * The moving contact line deposits a Landau-Levich-Derjaguin film of
    thickness `1.34·a·Ca^{2/3}`, gated by the partial-wetting threshold
    `Ca_c ≈ θ_r³/9L`. The deposited rivulet is Rayleigh-Plateau unstable and
    beads up into the residual pearls a runner leaves behind, so the trail,
    the drop's drain rate and the pearls are one conserved volume budget.
  * Impacts are sampled from the Marshall-Palmer size distribution at the
    wind-driven flux onto a vertical pane, and the Cossali-Mundo splash
    parameter `K = We^{1/2}·Re^{1/4}` decides whether one throws satellites.
    The rate of drops that land already above the release radius — a couple
    per thousand — falls out of the distribution rather than being dialled in.
  * Condensation is diffusion limited (`da/dt = K/a`) and coalescence
    conserves volume and momentum, so the drop population coarsens the way a
    breath figure does.
  * A static defect field modulates the local hysteresis. Its cross-stream
    asymmetry across the contact line is the lateral force, so meanders and
    the way runners fall into existing channels come out of the same
    retention term rather than a sine wave.

Rendering leans on the compositor keying contract: the misted pane is the
near-white key color, so it frosts out to the blurred scene, while wet trails
and drop interiors are emitted dark and translucent so the *sharp* scene
shows through them — the rain literally wipes the frost clear. Pointer events
wipe the mist by hand; the fog then slowly re-forms.

The model is host-side and cheap (a few hundred drops plus two fields), so
the `memory` backend of the GPU cases is accepted and ignored.
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
    # Static nucleation-site density in [0, 255]. Mist re-forms on a wiped
    # patch by heterogeneous nucleation, and sites are not spread evenly, so
    # a cleared trail re-fogs in patches. Fading the whole trail at one rate
    # instead drags every pixel of it through the half-cleared band at the
    # same moment, and that band is neither keyed by the compositor nor
    # transparent: aging trails then read as opaque milky streaks.
    nucleation::Matrix{UInt8}
    # Coarse static defect field around 1.0 that scales the local
    # contact-angle hysteresis. Sampled bilinearly, so the pinning landscape
    # a drop feels is smooth and repeatable.
    pinning::Matrix{Float32}
    pinning_step::Int
    clock::Base.RefValue{Float64}
    # Random phases for the two incommensurate gust tones that modulate the
    # rain rate, so showers come in bursts instead of a constant drizzle.
    gust_phases::NTuple{2,Float64}
    # Canvas pixels per metre of real glass.
    resolution::Float32
end

rain_resolution(height::Int) =
    max(Float32(height) / RAIN_PANE_HEIGHT, RAIN_MIN_RESOLUTION)

rain_drop_volume(radius_m::Float32) = RAIN_VOLUME_COEFFICIENT * radius_m^3
rain_radius_from_volume(volume::Float32) =
    cbrt(max(volume, 0.0f0) / RAIN_VOLUME_COEFFICIENT)

"""
Furmidge retention: the pinned contact line resists with `σ·w·Δcos θ` over a
contact width `w = 2a`. This is the only thing holding a drop up.
"""
rain_retention_force(radius_m::Float32, pinning::Float32) =
    2.0f0 * RAIN_SURFACE_TENSION * radius_m * RAIN_HYSTERESIS * pinning

rain_gravity_force(radius_m::Float32) =
    RAIN_WATER_DENSITY * RAIN_GRAVITY * rain_drop_volume(radius_m)

"""
Contact radius at which gravity exactly balances retention. Solving
`ρ·(π/3)f·a³·g = 2σ·a·Δcos θ` gives a radius proportional to the capillary
length; for water on glass it lands near 2.2 mm.
"""
rain_critical_radius(pinning::Float32) = sqrt(
    2.0f0 * RAIN_SURFACE_TENSION * RAIN_HYSTERESIS * pinning /
    (RAIN_WATER_DENSITY * RAIN_GRAVITY * RAIN_VOLUME_COEFFICIENT),
)

const RAIN_NOMINAL_CRITICAL = rain_critical_radius(1.0f0)
# A drop much larger than this sheds its excess into the trail rather than
# growing without bound: past a couple of critical radii the cap flattens
# toward the capillary length and the head of a rivulet simply breaks off.
const RAIN_MAX_CONTACT = 2.0f0 * RAIN_NOMINAL_CRITICAL

"""
Contact-line drag `k_v` in `F = k_v·U`. Cox-Voinov wedge dissipation scales
as `μ·U·ln(a/λ)/θ` per unit contact line; `RAIN_DISSIPATION` lumps the two
contact lines, the contact width and the bulk recirculation into one
calibrated coefficient.
"""
function rain_drag_coefficient(radius_m::Float32)
    logarithm = log(max(radius_m / RAIN_SLIP_LENGTH, 10.0f0))
    return RAIN_DISSIPATION * RAIN_VISCOSITY * radius_m * logarithm / RAIN_ANGLE_MEAN
end

"""
A coarse random field, bilinearly smoothed. Pure white noise sparkles once
the compositor magnifies the frame, and a pinning landscape built from it
would jitter a runner pixel by pixel instead of steering it.
"""
function rain_coarse_field(width::Int, height::Int, step::Int)
    return rand(Float32, cld(width, step) + 2, cld(height, step) + 2)
end

function rain_sample_field(field::Matrix{Float32}, step::Int, x::Real, y::Real)
    columns, rows = size(field)
    gx = clamp(Float32(x) / step + 1.0f0, 1.0f0, Float32(columns) - 1.0f0)
    gy = clamp(Float32(y) / step + 1.0f0, 1.0f0, Float32(rows) - 1.0f0)
    ix = floor(Int, gx)
    iy = floor(Int, gy)
    fx = gx - ix
    fy = gy - iy
    @inbounds return field[ix, iy] * (1 - fx) * (1 - fy) +
                    field[ix + 1, iy] * fx * (1 - fy) +
                    field[ix, iy + 1] * (1 - fx) * fy +
                    field[ix + 1, iy + 1] * fx * fy
end

"""
Blend the coarse mist field into a static per-pixel mottle with a small
per-pixel speck on top.
"""
function rain_grain(width::Int, height::Int)
    coarse_step = 9
    coarse = rain_coarse_field(width, height, coarse_step)
    grain = Matrix{UInt8}(undef, width, height)
    for y in 1:height, x in 1:width
        smooth = rain_sample_field(coarse, coarse_step, x - 1, y - 1)
        grain[x, y] = round(UInt8, 3.0f0 * (0.85f0 * smooth + 0.15f0 * rand(Float32)))
    end
    return grain
end

"""
Nucleation-site density: patches of glass that re-fog early or late. It is
static, so a dissolving trail is a fixed spatial pattern whose parts cross
the visibility band at different moments, rather than a field that sparkles
from frame to frame.
"""
function rain_nucleation(width::Int, height::Int)
    # Patches a few dozen pixels across. Per-pixel noise here would dissolve
    # a trail into grit rather than into fog, and every grain of it would
    # spend its own moment in the half-cleared band.
    # Two octaves: one bilinear grid alone dissolves along its own straight
    # iso-lines and leaves the retreating fog edge visibly polygonal.
    step = max(12, round(Int, 0.03 * height))
    detail = max(5, round(Int, step / 2.3))
    coarse = rain_coarse_field(width, height, step)
    fine = rain_coarse_field(width, height, detail)
    field = Matrix{UInt8}(undef, width, height)
    for y in 1:height, x in 1:width
        smooth = 0.65f0 * rain_sample_field(coarse, step, x - 1, y - 1) +
                 0.35f0 * rain_sample_field(fine, detail, x - 1, y - 1)
        field[x, y] = round(UInt8, 255.0f0 * clamp(smooth, 0.0f0, 1.0f0))
    end
    return field
end

rain_regrow_rate(site::UInt8) = 0.4f0 + 1.2f0 * (Float32(site) / 255.0f0)

"""
Effective hysteresis multiplier where the drop's contact line sits: the
static defect landscape, relieved wherever the glass has already been wetted
and cleared.
"""
function rain_local_pinning(case::RainCase, x::Real, y::Real)
    defect = rain_sample_field(case.pinning, case.pinning_step, x, y)
    wet = sample_wetness(case, x, y)
    return defect * (1.0f0 - RAIN_WET_RELIEF * wet)
end

function rain_stuck_drop(case::RainCase, x::Real, y::Real, radius::Real)
    return Raindrop(
        Float32(x),
        Float32(y),
        Float32(x),
        Float32(y),
        min(Float32(radius), RAIN_MAX_CONTACT * case.resolution),
        0.0f0,
        0.0f0,
        false,
        0.85f0 + 0.3f0 * rand(Float32),
        0.0f0,
        0.0f0,
    )
end

function build_rain_case(dimensions::Tuple{Int,Int}; memory=Array)
    # The droplet model runs on the host regardless of the selected backend.
    _ = memory
    width, height = dimensions
    resolution = rain_resolution(height)
    pinning_step = max(6, round(Int, 0.004f0 * resolution))
    case = RainCase(
        dimensions,
        Raindrop[],
        zeros(Float32, width, height),
        rain_grain(width, height),
        rain_nucleation(width, height),
        # Defect strengths spread about ±25% around nominal.
        0.75f0 .+ 0.5f0 .* rain_coarse_field(width, height, pinning_step),
        pinning_step,
        Ref(0.0),
        (2pi * rand(), 2pi * rand()),
        resolution,
    )

    # Seed a pane that has already been rained on for a while: impacts drawn
    # from the same size distribution, then aged by the same diffusion-limited
    # condensation law. Anything that would already have released has, so the
    # seeded population sits below the local critical radius.
    seeds = max(6, round(Int, 120 * width * height / 1.0e6))
    _, lambda = rain_impact_flux(case, 0.0)
    for _ in 1:seeds
        x = rand(Float32) * width
        y = rand(Float32) * height
        diameter = RAIN_TRACKED_CUTOFF - log(rand(Float32)) / lambda
        radius = rain_impact_radius(diameter)
        age = 40.0f0 * rand(Float32)
        radius = sqrt(radius^2 + 2.0f0 * RAIN_CONDENSATION * age)
        limit = 0.95f0 * rain_critical_radius(rain_local_pinning(case, x, y))
        push!(case.drops, rain_stuck_drop(case, x, y, min(radius, limit) * resolution))
    end
    return case
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

"""
Impacts per second on the whole pane, and the Marshall-Palmer slope that
produced them. A gust raises the instantaneous rain rate, which both flattens
the size distribution and lifts the flux, so squalls arrive as more *and*
bigger drops instead of a faster drizzle.
"""
function rain_impact_flux(case::RainCase, t::Float64)
    width, height = case.dimensions
    rate = max(RAIN_RATE * Float32(rain_gust(case, t)), 0.1f0)
    lambda = RAIN_MP_LAMBDA_COEFFICIENT * rate^(-0.21f0)
    # Number concentration of all drop sizes, then the wind-driven flux onto
    # a vertical pane, then the resolvable tail above the tracked cutoff.
    concentration = RAIN_MP_N0 / lambda
    flux = concentration * RAIN_WIND_SPEED * exp(-lambda * RAIN_TRACKED_CUTOFF)
    area = (width / case.resolution) * (height / case.resolution)
    return flux * area, lambda
end

"""
Contact radius a falling drop of diameter `D` settles into once it has
spread and relaxed on the glass, from `(π/6)D³ = (π/3)f·a³`.
"""
rain_impact_radius(diameter::Float32) =
    min(diameter / cbrt(2.0f0 * RAIN_CAP_FACTOR), RAIN_MAX_CONTACT)

function rain_spawn!(case::RainCase, spawned::Vector{Raindrop}, dt::Float64)
    width, height = case.dimensions
    resolution = case.resolution
    rate, lambda = rain_impact_flux(case, case.clock[])
    budget = rain_max_drops(case) - length(case.drops) - length(spawned)
    for _ in 1:min(rain_spawn_count(rate * dt), max(budget, 0))
        x = rand(Float32) * width
        y = rand(Float32) * height
        # Inverse-transform sampling of the Marshall-Palmer tail above the
        # tracked cutoff. Impacts that land already past the local critical
        # radius start running immediately; nothing forces that fraction, it
        # is exp(-Λ·ΔD) of the distribution.
        diameter = RAIN_TRACKED_CUTOFF - log(rand(Float32)) / lambda
        radius = rain_impact_radius(diameter)
        drop = rain_stuck_drop(case, x, y, radius * resolution)
        push!(spawned, drop)

        # The lamella spreads well past the final contact radius before it
        # recoils; that reach is what wipes the mist off around an impact.
        weber = RAIN_WATER_DENSITY * RAIN_WIND_SPEED^2 * diameter / RAIN_SURFACE_TENSION
        reynolds = RAIN_WATER_DENSITY * RAIN_WIND_SPEED * diameter / RAIN_VISCOSITY
        spread = clamp(0.45f0 * sqrt(sqrt(weber)), 1.2f0, 3.5f0)
        stamp_wetness!(case, x, y, spread * drop.radius, 0.55f0)

        # Cossali-Mundo: above K ≈ 57.7 an impact on a wetted surface throws a
        # corona of satellites instead of merely depositing.
        splash = sqrt(weber) * sqrt(sqrt(reynolds))
        splash <= RAIN_SPLASH_PARAMETER && continue
        satellites = min(3, floor(Int, (splash / RAIN_SPLASH_PARAMETER - 1.0f0) * 4.0f0))
        for _ in 1:satellites
            # Satellites carry a few percent of the parent volume each.
            share = 0.02f0 + 0.04f0 * rand(Float32)
            angle = 2.0f0 * Float32(pi) * rand(Float32)
            reach = drop.radius * (1.6f0 + 2.0f0 * rand(Float32)) * spread
            push!(
                spawned,
                rain_stuck_drop(
                    case,
                    clamp(x + reach * cos(angle), 1.0f0, Float32(width)),
                    clamp(y + reach * sin(angle), 1.0f0, Float32(height)),
                    drop.radius * cbrt(share),
                ),
            )
        end
    end
    return nothing
end

"""
Grow a pinned drop by diffusion-limited condensation. A cleared path has no
mist left to give up, so a runner's trail stays clear until the fog re-forms.
"""
function rain_condense!(case::RainCase, drop::Raindrop, dt::Float32)
    radius = drop.radius / case.resolution
    supply = 1.0f0 - RAIN_VAPOUR_DEPLETION * sample_wetness(case, drop.x, drop.y)
    radius += dt * RAIN_CONDENSATION * supply / max(radius, 1.0f-5)
    drop.radius = min(radius, RAIN_MAX_CONTACT) * case.resolution
    return nothing
end

"""
Deposit the Landau-Levich-Derjaguin film a receding contact line leaves
behind, and bead the resulting rivulet up into a pearl every Rayleigh-Plateau
wavelength. Returns the volume (m³) the drop loses over `travel` pixels.
"""
function rain_deposit!(
    case::RainCase,
    drop::Raindrop,
    spawned::Vector{Raindrop},
    travel::Float32,
)
    resolution = case.resolution
    radius = drop.radius / resolution
    speed = hypot(drop.speed, drop.lateral) / resolution
    capillary = RAIN_VISCOSITY * speed / RAIN_SURFACE_TENSION
    # Partial wetting suppresses deposition until the contact line can no
    # longer keep up: de Gennes' threshold Ca_c ≈ θ_r³/(9 ln(a/λ)).
    logarithm = log(max(radius / RAIN_SLIP_LENGTH, 10.0f0))
    critical = RAIN_ANGLE_RECEDING^3 / (9.0f0 * logarithm)
    gate = capillary / (capillary + critical)
    thickness = 1.34f0 * radius * cbrt(capillary^2) * gate
    lost = 2.0f0 * radius * thickness * (travel / resolution)

    drop.rivulet += lost
    drop.deposit_debt -= travel
    if drop.deposit_debt <= 0.0f0
        pearl = rain_radius_from_volume(RAIN_PEARL_YIELD * drop.rivulet)
        drop.rivulet = 0.0f0
        drop.deposit_debt = RAIN_PEARL_SPACING * 2.0f0 * drop.radius *
                            (0.75f0 + 0.5f0 * rand(Float32))
        if pearl * resolution > 0.8f0
            jitter = (rand(Float32) - 0.5f0) * drop.radius
            push!(
                spawned,
                rain_stuck_drop(
                    case,
                    drop.x + jitter,
                    drop.y - 1.4f0 * drop.radius,
                    pearl * resolution,
                ),
            )
        end
    end
    return lost
end

"""
One substep of the force balance for a single drop. Gravity pulls, the
retention force resists along whatever direction the drop is actually moving,
and the contact-line drag is integrated implicitly so the step stays stable
even when a big drop's inertial time is shorter than the substep.
"""
function rain_dynamics!(
    case::RainCase,
    drop::Raindrop,
    spawned::Vector{Raindrop},
    dt::Float32,
)
    resolution = case.resolution
    radius = drop.radius / resolution
    pinning = drop.pinning * rain_local_pinning(case, drop.x, drop.y)
    gravity = rain_gravity_force(radius)
    retention = rain_retention_force(radius, pinning)
    mass = RAIN_WATER_DENSITY * rain_drop_volume(radius)
    drag = rain_drag_coefficient(radius)

    speed = hypot(drop.speed, drop.lateral)
    if speed > RAIN_STOP_SPEED
        ux = drop.lateral / speed
        uy = drop.speed / speed
    else
        # Incipient motion is straight downslope.
        ux = 0.0f0
        uy = 1.0f0
    end

    # A cross-stream imbalance in the retention force steers the drop: the
    # contact line is held harder on the side with the stronger defects, so
    # the drop drifts toward the weaker one and settles into wet channels.
    ahead = drop.y + drop.radius
    left = drop.pinning * rain_local_pinning(case, drop.x - drop.radius, ahead)
    right = drop.pinning * rain_local_pinning(case, drop.x + drop.radius, ahead)
    steering = 0.5f0 * rain_retention_force(radius, left - right)

    fy = gravity - retention * uy
    fx = steering - retention * ux
    implicit = 1.0f0 / (mass + dt * drag)
    uy_new = (mass * (drop.speed / resolution) + dt * fy) * implicit
    ux_new = (mass * (drop.lateral / resolution) + dt * fx) * implicit

    drop.speed = max(uy_new, 0.0f0) * resolution
    drop.lateral = ux_new * resolution
    if drop.speed <= RAIN_STOP_SPEED
        # Static friction catches the drained drop: it re-pins on its own,
        # below the radius that released it, which is the hysteresis loop.
        drop.speed = 0.0f0
        drop.lateral = 0.0f0
        drop.sliding = false
        return nothing
    end
    drop.sliding = true
    drop.deposit_debt <= 0.0f0 &&
        (drop.deposit_debt = RAIN_PEARL_SPACING * 2.0f0 * drop.radius)

    old_x, old_y = drop.x, drop.y
    drop.y += drop.speed * dt
    drop.x = clamp(
        drop.x + drop.lateral * dt,
        -2.0f0 * drop.radius,
        Float32(case.dimensions[1]) + 2.0f0 * drop.radius,
    )

    # The swept contact area is what wipes the mist, so the trail is stamped
    # at the contact radius along the whole travelled segment. Overlapping
    # stamps every third of a radius keep the channel continuous without
    # redrawing the same disk sixty times for one fast substep.
    moved = hypot(drop.x - old_x, drop.y - old_y)
    stride = max(1.0f0, 0.35f0 * drop.radius)
    steps = max(1, ceil(Int, moved / stride))
    for step in 1:steps
        f = Float32(step) / steps
        stamp_wetness!(
            case,
            old_x + (drop.x - old_x) * f,
            old_y + (drop.y - old_y) * f,
            drop.radius,
            1.0f0,
        )
    end

    lost = rain_deposit!(case, drop, spawned, moved)
    volume = max(rain_drop_volume(radius) - lost, 0.0f0)
    drop.radius = rain_radius_from_volume(volume) * resolution
    return nothing
end

"""
Squared distance from a point to a segment. Coalescence tests the segment a
drop swept this substep, so a runner absorbs everything in its path instead
of tunnelling past drops between frames.
"""
function rain_segment_distance2(
    px::Float32,
    py::Float32,
    ax::Float32,
    ay::Float32,
    bx::Float32,
    by::Float32,
)
    ex = bx - ax
    ey = by - ay
    length2 = ex * ex + ey * ey
    t = length2 <= 0.0f0 ? 0.0f0 : clamp(((px - ax) * ex + (py - ay) * ey) / length2, 0.0f0, 1.0f0)
    dx = px - (ax + t * ex)
    dy = py - (ay + t * ey)
    return dx * dx + dy * dy
end

"""
Coalescence. Volume and momentum are conserved; the merged drop's radius
follows from the summed cap volumes, so whether the pair now runs is decided
by the same force balance as everything else rather than by a merge flag.
"""
function rain_merge!(case::RainCase, spawned::Vector{Raindrop})
    drops = case.drops
    resolution = case.resolution
    count = length(drops)
    @inbounds for i in 1:count
        a = drops[i]
        a.radius <= 0.0f0 && continue
        for j in (i + 1):count
            b = drops[j]
            b.radius <= 0.0f0 && continue
            reach = 0.82f0 * (a.radius + b.radius)
            # Cheap reject on the swept bounding boxes before the two capsule
            # tests; at a few hundred drops this is the whole cost of the pass.
            (min(a.x, a.previous_x) - reach > max(b.x, b.previous_x) ||
             min(b.x, b.previous_x) - reach > max(a.x, a.previous_x) ||
             min(a.y, a.previous_y) - reach > max(b.y, b.previous_y) ||
             min(b.y, b.previous_y) - reach > max(a.y, a.previous_y)) && continue
            reach2 = reach * reach
            touching =
                rain_segment_distance2(b.x, b.y, a.previous_x, a.previous_y, a.x, a.y) <=
                reach2 ||
                rain_segment_distance2(a.x, a.y, b.previous_x, b.previous_y, b.x, b.y) <=
                reach2
            touching || continue

            into, from = a.radius >= b.radius ? (a, b) : (b, a)
            into_volume = rain_drop_volume(into.radius / resolution)
            from_volume = rain_drop_volume(from.radius / resolution)
            total = into_volume + from_volume
            weight = from_volume / total
            into.x += (from.x - into.x) * weight
            into.y += (from.y - into.y) * weight
            # Momentum, not the faster of the two: a big slow drop absorbing a
            # pearl barely changes speed, while a merge of equals splits the
            # difference.
            into.speed += (from.speed - into.speed) * weight
            into.lateral += (from.lateral - into.lateral) * weight
            merged = rain_radius_from_volume(total)
            if merged > RAIN_MAX_CONTACT
                # Past the cap the head of the rivulet simply breaks off; the
                # excess volume leaves as a pearl instead of vanishing.
                excess = total - rain_drop_volume(RAIN_MAX_CONTACT)
                shed = rain_radius_from_volume(excess) * resolution
                merged = RAIN_MAX_CONTACT
                shed > 0.8f0 && push!(
                    spawned,
                    rain_stuck_drop(case, into.x, into.y - 1.6f0 * into.radius, shed),
                )
            end
            into.radius = merged * resolution
            from.radius = 0.0f0
            a.radius <= 0.0f0 && break
        end
    end
    return nothing
end

function step_rain!(case::RainCase, dt::Float64)
    case.clock[] += dt
    dtf = Float32(dt)
    width, height = case.dimensions

    # The fog re-forms everywhere, but at each patch's own nucleation rate,
    # so a trail dissolves into mist instead of dimming as one sheet.
    decay = dtf / RAIN_REGROW_SECONDS
    wetness = case.wetness
    nucleation = case.nucleation
    Threads.@threads :static for y in 1:height
        @inbounds for x in 1:width
            w = wetness[x, y]
            w <= 0.0f0 && continue
            rate = rain_regrow_rate(nucleation[x, y])
            RAIN_DEWET_LOW < w < RAIN_DEWET_HIGH && (rate *= RAIN_DEWET_RATE)
            wetness[x, y] = max(w - decay * rate, 0.0f0)
        end
    end

    spawned = Raindrop[]
    rain_spawn!(case, spawned, dt)
    for drop in case.drops
        drop.previous_x = drop.x
        drop.previous_y = drop.y
        rain_condense!(case, drop, dtf)
        rain_dynamics!(case, drop, spawned, dtf)
    end
    rain_merge!(case, spawned)

    # Drops that drain below a pixel have joined the film; drops past the
    # bottom edge have left the pane.
    minimum_radius = max(0.6f0, 1.2f-4 * case.resolution)
    filter!(
        drop -> drop.radius > minimum_radius && drop.y <= height + 2.0f0 * drop.radius,
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
            # The pane is bimodal — frosted or clear — because the halfway
            # colors are neither keyed by the compositor nor transparent, and
            # would smear a trail into an opaque milky band. The ramp is only
            # wide enough to antialias the boundary: the wetness gradient at
            # a trail edge is steep, so this band is a sub-pixel rim there,
            # and the dewetting term above keeps a drying trail from parking
            # its whole area inside it.
            clear = rain_smoothstep(0.46f0, 0.52f0, w)
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
        # A running drop is drawn as the frame integrates it: the capillary
        # number stretches the cap into a teardrop, and the exposure smears
        # that shape along its path. Spreading the same water over a longer
        # streak thins its optical depth, so the alpha ramp scales with the
        # elongation instead of painting a solid slug.
        speed = hypot(drop.speed, drop.lateral)
        capillary = RAIN_VISCOSITY * (speed / case.resolution) / RAIN_SURFACE_TENSION
        elongation = 1.0f0 + RAIN_TEARDROP * capillary
        smear = 0.5f0 * speed * RAIN_EXPOSURE
        rx = r / sqrt(elongation)
        ry = r * elongation + smear
        thinning = clamp(r / ry, 0.35f0, 1.0f0)
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
                dome = sqrt(max(1.0f0 - q * q, 0.0f0)) * thinning
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
                    s = (1.0f0 - h2) * (1.0f0 - h2) * thinning * thinning
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
