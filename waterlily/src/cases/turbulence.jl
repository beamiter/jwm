"""
Two-dimensional turbulence in a real WaterLily solve, reviving the gesture
"tuanliu" style of the pre-WaterLily Rust postprocess. The flow starts from a
field of random Gaussian vortices and evolves freely — merging, pairing and
coarsening the way 2D turbulence does — while the pointer stirs new vortex
dipoles in along its strokes and an ambient reseed keeps the canvas alive
when nobody touches it. There is no immersed body: the vorticity field itself
is the picture, rendered through the standard diverging palettes exactly like
the old curl-based shading.
"""
mutable struct TurbulenceCase{S} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    # Previous pointer position in grid coordinates (y up); NaN until the
    # first event so a fresh stream cannot inject a screen-wide jet.
    pointer::NTuple{2,Float32}
    # Dimensionless time of the next ambient vortex reseed.
    reseed_time::Float64
end

const TURBULENCE_SEED_VORTICES = 24
const TURBULENCE_REYNOLDS = 3000.0
# Stir strength as a multiple of the velocity scale U; dipoles this strong
# stay comfortably inside the adaptive-CFL budget the stylus case proved out.
const TURBULENCE_STIR_SPEED = 0.9
const TURBULENCE_RESEED_SECONDS = (4.0, 9.0)

"""
Initial-condition closure over a tuple of `(cx, cy, 1/2σ², Γ/σ)` scalars: a
sum of Gaussian vortices, divergence-free by construction. Plain-number
captures keep the closure isbits so `apply!` can run it inside GPU kernels,
the same contract the moving-body cases rely on.
"""
function turbulence_seed(vortices)
    return function (i, x)
        total = zero(eltype(x))
        for vortex in vortices
            dx = x[1] - vortex[1]
            dy = x[2] - vortex[2]
            swirl = vortex[4] * exp(-(dx * dx + dy * dy) * vortex[3])
            total += i == 1 ? -dy * swirl : dx * swirl
        end
        return total
    end
end

function build_turbulence_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=TURBULENCE_REYNOLDS,
)
    width, height = dimensions
    T = Float32
    # No body sets the length scale here, so pick one that gives the eddies
    # a graceful dimensionless clock: raw time per published frame is L/fps.
    scale = max(T(8), T(height) * T(0.08))
    # Many small, mild vortices give the fine-grained turbulent texture; a
    # few large strong ones just paint the whole canvas in saturated blobs.
    vortices = ntuple(TURBULENCE_SEED_VORTICES) do _
        σ = (T(0.028) + T(0.05) * rand(T)) * T(height)
        strength = (T(0.55) + T(0.5) * rand(T)) * (rand(Bool) ? T(1) : T(-1))
        (
            T(width) * rand(T),
            T(height) * rand(T),
            T(1) / (T(2) * σ * σ),
            strength / σ,
        )
    end
    simulation = WaterLily.Simulation(
        dimensions,
        (T(0), T(0)),
        scale;
        U=T(1),
        ν=scale / T(reynolds),
        u0=turbulence_seed(vortices),
        T,
        mem=memory,
    )
    return TurbulenceCase(
        simulation,
        dimensions,
        (NaN32, NaN32),
        next_reseed_time(0.0),
    )
end

next_reseed_time(now::Real) =
    Float64(now) + TURBULENCE_RESEED_SECONDS[1] +
    rand() * (TURBULENCE_RESEED_SECONDS[2] - TURBULENCE_RESEED_SECONDS[1])

case_palette_name(::TurbulenceCase) = "fluent"

# No immersed body: the default renderer probes the distance only when the
# bounds say it might matter, and `nothing` bounds with an infinite distance
# keep the body pass a no-op.
body_distance(::TurbulenceCase, ::Real, ::Real, ::Real) = Inf

remeasure_on_step(::TurbulenceCase) = false

"""
Add a host-built velocity patch into the simulation field. The patch is
staged through `similar(u, …)` so the same code feeds CPU arrays and GPU
backends without scalar indexing; injections are rare (pointer strokes and
the ambient reseed), so the upload cost is irrelevant.
"""
function add_velocity_patch!(u, x0::Int, y0::Int, patch::Array{Float32,3})
    staged = similar(u, size(patch))
    copyto!(staged, patch)
    width, height = size(patch, 1), size(patch, 2)
    @views u[x0:(x0 + width - 1), y0:(y0 + height - 1), :] .+= staged
    return nothing
end

"""
Build a Gaussian vortex dipole around `(cx, cy)` (grid coordinates, y up)
that jets along `(dirx, diry)` and add it to the flow. The two counter-signed
cores sit perpendicular to the jet, the same coherent-pair injection the old
Rust turbulence used for gesture strokes.
"""
function inject_dipole!(case::TurbulenceCase, cx, cy, dirx, diry, strength)
    simulation = case.simulation
    T = Float32
    σ = max(T(3), T(0.03) * case.dimensions[2])
    reach = ceil(Int, 3 * σ)
    span = 2 * reach + 1
    nx, ny = size(simulation.flow.p, 1), size(simulation.flow.p, 2)
    (span > nx - 3 || span > ny - 3) && return nothing
    x0 = clamp(round(Int, cx) - reach, 2, nx - span - 1)
    y0 = clamp(round(Int, cy) - reach, 2, ny - span - 1)

    norm = hypot(dirx, diry)
    norm < eps(T) && return nothing
    jet_x, jet_y = T(dirx / norm), T(diry / norm)
    # Counter-rotating cores offset perpendicular to the jet direction.
    offset_x, offset_y = -jet_y * σ, jet_x * σ
    peak = T(strength) * T(simulation.U)

    patch = zeros(Float32, span, span, 2)
    inv2σ2 = 1 / (2 * σ * σ)
    for j in 1:span, i in 1:span
        px = T(x0 + i - 1) - T(cx)
        py = T(y0 + j - 1) - T(cy)
        for (sign, ox, oy) in ((T(1), offset_x, offset_y), (T(-1), -offset_x, -offset_y))
            dx = px - ox
            dy = py - oy
            swirl = sign * peak * exp(-(dx * dx + dy * dy) * inv2σ2) / σ
            patch[i, j, 1] -= dy * swirl
            patch[i, j, 2] += dx * swirl
        end
    end
    add_velocity_patch!(simulation.flow.u, x0, y0, patch)
    return nothing
end

"""
Pointer strokes stir the fluid: each event injects a dipole jetting along the
motion since the previous event, once the stroke has covered enough distance
to define a direction.
"""
function handle_pointer!(case::TurbulenceCase, x::Real, y::Real)
    width, height = case.dimensions
    gx = clamp(Float32(x) * width, 1.0f0, Float32(width))
    gy = clamp((1.0f0 - Float32(y)) * height, 1.0f0, Float32(height))
    px, py = case.pointer
    if isnan(px)
        case.pointer = (gx, gy)
        return nothing
    end
    dx = gx - px
    dy = gy - py
    distance = hypot(dx, dy)
    # The spacing sits well above the dipole core size, so a fast flick lays
    # down a few distinct vortex pairs instead of a dense overlapping ladder.
    distance < 0.08f0 * height && return nothing
    case.pointer = (gx, gy)
    strength = TURBULENCE_STIR_SPEED * min(1.0, distance / (0.15 * height))
    inject_dipole!(case, (px + gx) / 2, (py + gy) / 2, dx, dy, strength)
    return nothing
end

"""
Ambient reseed: without input, 2D turbulence coarsens into a few lazy
vortices and slowly dies. Dropping one fresh random vortex pair every few
seconds keeps the canvas alive indefinitely.
"""
function frame_tick!(case::TurbulenceCase)
    now = simulation_time(case)
    now < case.reseed_time && return nothing
    case.reseed_time = next_reseed_time(now)
    width, height = case.dimensions
    angle = 2pi * rand()
    inject_dipole!(
        case,
        (0.15 + 0.7 * rand()) * width,
        (0.15 + 0.7 * rand()) * height,
        cos(angle),
        sin(angle),
        0.5 + 0.3 * rand(),
    )
    return nothing
end
