"""
True three-dimensional free turbulence, reviving the gesture-driven
"tuanliu" style of the pre-WaterLily Rust postprocess as a native volume.
The flow starts from sparse, randomly oriented Gaussian vortex blobs and
evolves in a periodic tank, while pointer strokes stir localized 3D dipoles
through the water and an ambient reseed keeps the cascade alive.

The display path matches the jelly case's version-2 contract: signed
depth-axis vorticity supplies the cold/warm halves of the selected palette,
vorticity magnitude supplies a sparse low-alpha participating medium, and
the compositor ray-marches the result inside its perspective glass tank.
Every turbulence voxel stays below the compositor's tissue band, so the
effect receives hue-preserving wake scattering without pretending that a
vortex filament is a refracting jelly membrane. `JWM_WATERLILY_PLANAR`
retains a signed depth projection for older consumers and debugging.
"""
mutable struct TurbulenceCase{S} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    # Solver axes are (screen x, front-to-back depth, screen vertical).
    domain::NTuple{3,Int}
    # Previous pointer position in solver x/vertical coordinates; NaN until
    # the first event so a fresh stream cannot inject a screen-wide jet.
    pointer::NTuple{2,Float32}
    # Dimensionless time of the next ambient 3D dipole injection.
    reseed_time::Float64
    # Host display fields. The flow scratch is downloaded twice per frame:
    # magnitude controls opacity and the signed depth-axis component controls
    # the diverging palette hue.
    magnitude_host::Array{Float32,3}
    signed_host::Array{Float32,3}
    # Signed max-absolute projection used only by the planar fallback.
    projected::Matrix{Float32}
    # Version-2 scratch: nx * nz rows * ny front-to-back slices.
    volume_rgba::Vector{UInt8}
end

const TURBULENCE_SEED_VORTICES = 24
const TURBULENCE_REYNOLDS = 1200.0
# Pointer dipoles remain below the velocity scale used by the adaptive CFL
# stepper while still producing coherent filaments through several slices.
const TURBULENCE_STIR_SPEED = 0.9
const TURBULENCE_RESEED_SECONDS = (3.0, 6.0)
# The transfer stays sparse enough for the compositor occupancy texture to
# skip empty water, and alpha 28/255 remains below the shader's 0.115 wake
# ceiling (the tissue transition does not start until 0.12).
const TURBULENCE_WAKE_FLOOR = 0.18f0
const TURBULENCE_WAKE_KNEE = 2.4f0
const TURBULENCE_PLANAR_KNEE = 3.0f0
const TURBULENCE_MIN_HUE = 0.28f0
const TURBULENCE_MAX_ALPHA = UInt8(0x1c)

"""
Choose the 3D tank from the display size. The budget deliberately matches
the jelly tank: 64 vertical cells on CPU, 80 on CUDA/ROCm, a display-shaped
x extent, and a shallower but nontrivial depth. Every extent is a multiple of
16 for WaterLily's multilevel pressure solve.
"""
function turbulence_domain(
    dimensions::Tuple{Int,Int};
    accelerated::Bool=false,
)
    width, height = dimensions
    layer_cap = accelerated ? 5 : 4
    nz = 16 * clamp(round(Int, height / 160), 2, layer_cap)
    aspect = width / height
    nx = 16 * clamp(round(Int, nz * aspect / 16), 2, 12)
    ny = 16 * max(1, round(Int, nz * 0.6 / 16))
    return (nx, ny, nz)
end

"""
One localized divergence-free vortex blob. The tuple stores
`(cx, cy, cz, 1/2sigma^2, strength/sigma, axis_x, axis_y, axis_z)`.
For displacement `r` and unit axis `a`, velocity is
`strength/sigma * exp(-|r|^2/2sigma^2) * (a cross r)`. Its divergence is
zero because the radial Gaussian gradient is orthogonal to `a cross r`.
"""
@inline function turbulence_blob_component(i::Integer, x, blob)
    dx = x[1] - blob[1]
    dy = x[2] - blob[2]
    dz = x[3] - blob[3]
    envelope = blob[5] * exp(-(dx * dx + dy * dy + dz * dz) * blob[4])
    cross_x = blob[7] * dz - blob[8] * dy
    cross_y = blob[8] * dx - blob[6] * dz
    cross_z = blob[6] * dy - blob[7] * dx
    component = i == 1 ? cross_x : (i == 2 ? cross_y : cross_z)
    return envelope * component
end

function turbulence_seed(blobs)
    return function (i, x)
        total = zero(eltype(x))
        for blob in blobs
            total += turbulence_blob_component(i, x, blob)
        end
        return total
    end
end

function turbulence_random_blob(domain::NTuple{3,Int})
    T = Float32
    nx, ny, nz = domain
    sigma = max(T(1.25), (T(0.025) + T(0.045) * rand(T)) * T(nz))
    margin = min(T(3) * sigma, T(0.42) * T(min(nx, ny, nz)))
    center(extent) = margin + rand(T) * (T(extent) - T(2) * margin)

    axis_y = T(2) * rand(T) - one(T)
    angle = T(2pi) * rand(T)
    radial = sqrt(max(zero(T), one(T) - axis_y * axis_y))
    axis_x = radial * cos(angle)
    axis_z = radial * sin(angle)
    strength = (T(0.48) + T(0.52) * rand(T)) * (rand(Bool) ? one(T) : -one(T))
    return (
        center(nx),
        center(ny),
        center(nz),
        one(T) / (T(2) * sigma * sigma),
        strength / sigma,
        axis_x,
        axis_y,
        axis_z,
    )
end

function build_turbulence_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=TURBULENCE_REYNOLDS,
)
    T = Float32
    nx, ny, nz = domain = turbulence_domain(
        dimensions;
        accelerated=memory !== Array,
    )
    scale = max(T(8), T(nz) * T(0.08))
    blobs = ntuple(_ -> turbulence_random_blob(domain), TURBULENCE_SEED_VORTICES)
    simulation = WaterLily.Simulation(
        domain,
        (T(0), T(0), T(0)),
        scale;
        U=T(1),
        ν=scale / T(reynolds),
        u0=turbulence_seed(blobs),
        perdir=(1, 2, 3),
        T,
        mem=memory,
    )
    return TurbulenceCase(
        simulation,
        dimensions,
        domain,
        (NaN32, NaN32),
        next_reseed_time(0.0),
        Array{Float32,3}(undef, nx + 2, ny + 2, nz + 2),
        Array{Float32,3}(undef, nx + 2, ny + 2, nz + 2),
        Matrix{Float32}(undef, nx, nz),
        Vector{UInt8}(undef, 4 * nx * nz * ny),
    )
end

next_reseed_time(now::Real) =
    Float64(now) + TURBULENCE_RESEED_SECONDS[1] +
    rand() * (TURBULENCE_RESEED_SECONDS[2] - TURBULENCE_RESEED_SECONDS[1])

case_palette_name(::TurbulenceCase) = "fluent"

body_distance(::TurbulenceCase, ::Real, ::Real, ::Real) = Inf

remeasure_on_step(::TurbulenceCase) = false

# Publish width x vertical rows x front-to-back depth slices.
frame_geometry(case::TurbulenceCase) =
    (case.domain[1], case.domain[3], case.domain[2])

"""
Byte offset (one-based, pointing at R) of solver cell
`(x, depth, vertical)` in the published RGBA volume. Rows are reversed from
the solver's vertical-up convention into the protocol's top-left origin.
"""
@inline function turbulence_volume_offset(
    nx::Integer,
    nz::Integer,
    x::Integer,
    depth::Integer,
    vertical::Integer,
)
    top_row = nz - vertical
    return 4 * (((depth - 1) * nz + top_row) * nx + (x - 1)) + 1
end

"""
Add a host-built localized velocity patch to either CPU or accelerator
storage. Injections are rare compared with solver steps, so one staged upload
keeps the implementation backend-neutral without affecting frame cadence.
"""
function add_velocity_patch!(
    u,
    x0::Int,
    y0::Int,
    z0::Int,
    patch::Array{Float32,4},
)
    staged = similar(u, size(patch))
    copyto!(staged, patch)
    width, depth, height = size(patch, 1), size(patch, 2), size(patch, 3)
    @views u[
        x0:(x0 + width - 1),
        y0:(y0 + depth - 1),
        z0:(z0 + height - 1),
        :,
    ] .+= staged
    return nothing
end

"""
Inject two counter-signed, randomly depth-localized 3D vortex blobs. Their
centres straddle the screen-space stroke and their axes lean through depth,
so the pair jets along the pointer direction while also exciting the third
velocity component instead of stamping an extruded 2D ladder.
"""
function inject_dipole!(
    case::TurbulenceCase,
    cx,
    cy,
    cz,
    dirx,
    dirz,
    strength,
)
    simulation = case.simulation
    T = Float32
    nx, ny, nz = case.domain
    sigma = max(T(1.5), T(0.045) * T(nz))
    screen_reach = ceil(Int, T(4) * sigma)
    depth_reach = ceil(Int, T(3) * sigma)
    span_xz = 2 * screen_reach + 1
    span_y = 2 * depth_reach + 1
    (span_xz > nx || span_xz > nz || span_y > ny) && return nothing

    # Keep the complete counter-rotating pair inside the periodic tile. A
    # clamped patch window with an unchanged centre would cut off one core at
    # a screen edge, injecting divergence and a visible seam instead of the
    # intended compact dipole. Moving only the virtual 3D centre inward keeps
    # edge gestures active while preserving the whole Gaussian support.
    cx = clamp(
        T(cx),
        T(screen_reach) + T(0.5),
        T(nx - screen_reach) - T(0.5),
    )
    cy = clamp(T(cy), T(depth_reach) + T(0.5), T(ny - depth_reach) - T(0.5))
    cz = clamp(
        T(cz),
        T(screen_reach) + T(0.5),
        T(nz - screen_reach) - T(0.5),
    )

    norm = hypot(dirx, dirz)
    norm < eps(T) && return nothing
    stroke_x, stroke_z = T(dirx / norm), T(dirz / norm)
    axis_x, axis_y, axis_z = T(0.35) * stroke_z, one(T), -T(0.35) * stroke_x
    axis_norm = sqrt(axis_x * axis_x + axis_y * axis_y + axis_z * axis_z)
    axis_x, axis_y, axis_z =
        axis_x / axis_norm, axis_y / axis_norm, axis_z / axis_norm
    offset_x, offset_z = -stroke_z * sigma, stroke_x * sigma
    peak = T(strength) * T(simulation.U)
    inv2sigma2 = one(T) / (T(2) * sigma * sigma)
    blobs = (
        (
            T(cx) + offset_x,
            T(cy),
            T(cz) + offset_z,
            inv2sigma2,
            peak / sigma,
            axis_x,
            axis_y,
            axis_z,
        ),
        (
            T(cx) - offset_x,
            T(cy),
            T(cz) - offset_z,
            inv2sigma2,
            -peak / sigma,
            axis_x,
            axis_y,
            axis_z,
        ),
    )
    velocity = turbulence_seed(blobs)

    storage = size(simulation.flow.p)
    x0 = clamp(round(Int, cx + T(1.5)) - screen_reach, 2, storage[1] - span_xz)
    y0 = clamp(round(Int, cy + T(1.5)) - depth_reach, 2, storage[2] - span_y)
    z0 = clamp(round(Int, cz + T(1.5)) - screen_reach, 2, storage[3] - span_xz)
    patch = Array{Float32,4}(undef, span_xz, span_y, span_xz, 3)
    @inbounds for k in 1:span_xz, j in 1:span_y, i in 1:span_xz, component in 1:3
        storage_x = x0 + i - 1
        storage_y = y0 + j - 1
        storage_z = z0 + k - 1
        point = SA[
            T(storage_x) - T(1.5) - (component == 1 ? T(0.5) : T(0)),
            T(storage_y) - T(1.5) - (component == 2 ? T(0.5) : T(0)),
            T(storage_z) - T(1.5) - (component == 3 ? T(0.5) : T(0)),
        ]
        patch[i, j, k, component] = velocity(component, point)
    end
    flow = simulation.flow
    add_velocity_patch!(flow.u, x0, y0, z0, patch)
    # Pointer commands are handled before the current frame is rendered, so
    # the vorticity metric must see the injected velocity through periodic
    # ghosts immediately rather than waiting for the next solver step's BC.
    WaterLily.BC!(
        flow.u,
        flow.uBC,
        flow.exitBC,
        flow.perdir,
        simulation_time(case),
    )
    return nothing
end

"""
Pointer strokes map to the x/vertical face of the tank. A deterministic,
slowly wandering virtual depth gives consecutive strokes real parallax while
remaining controllable from a two-dimensional desktop pointer.
"""
function handle_pointer!(case::TurbulenceCase, x::Real, y::Real)
    nx, ny, nz = case.domain
    gx = clamp(Float32(x) * nx, 0.5f0, Float32(nx) - 0.5f0)
    gz = clamp((1.0f0 - Float32(y)) * nz, 0.5f0, Float32(nz) - 0.5f0)
    px, pz = case.pointer
    if isnan(px)
        case.pointer = (gx, gz)
        return nothing
    end
    dx = gx - px
    dz = gz - pz
    distance = hypot(dx, dz)
    distance < 0.08f0 * nz && return nothing
    case.pointer = (gx, gz)
    strength = TURBULENCE_STIR_SPEED * min(1.0, distance / (0.15 * nz))
    phase = 0.73f0 * Float32(simulation_time(case)) + 0.9f0 * gx / nx
    depth = (0.5f0 + 0.28f0 * sin(phase)) * ny
    inject_dipole!(case, (px + gx) / 2, depth, (pz + gz) / 2, dx, dz, strength)
    return nothing
end

"""
Inject one fresh 3D dipole every few dimensionless seconds. Three-dimensional
turbulence cascades toward small scales and dissipates, so this sparse forcing
keeps filaments entering the display without turning the complete tank into
uniform occupied fog.
"""
function frame_tick!(case::TurbulenceCase)
    now = simulation_time(case)
    now < case.reseed_time && return nothing
    case.reseed_time = next_reseed_time(now)
    nx, ny, nz = case.domain
    angle = 2pi * rand()
    inject_dipole!(
        case,
        (0.15 + 0.7 * rand()) * nx,
        (0.15 + 0.7 * rand()) * ny,
        (0.15 + 0.7 * rand()) * nz,
        cos(angle),
        sin(angle),
        0.5 + 0.3 * rand(),
    )
    return nothing
end

"""
Evaluate vorticity magnitude and the signed depth-axis component on the
simulation device, downloading each scalar field once. Both are normalized
by the solver's L/U scale so transfer thresholds are resolution-independent.
"""
function download_turbulence_vorticity!(case::TurbulenceCase)
    simulation = case.simulation
    flow = simulation.flow
    u = flow.u
    σ = flow.σ
    scale = eltype(σ)(simulation.L / simulation.U)
    WaterLily.@inside σ[I] = WaterLily.ω_mag(I, u) * scale
    WaterLily.perBC!(σ, flow.perdir)
    copyto!(case.magnitude_host, σ)
    WaterLily.@inside σ[I] = WaterLily.ω(I, u)[2] * scale
    WaterLily.perBC!(σ, flow.perdir)
    copyto!(case.signed_host, σ)
    return (case.magnitude_host, case.signed_host)
end

@inline turbulence_finite_scalar(value::Real) =
    isfinite(value) ? Float64(value) : 0.0

"""
Centre-heavy isotropic reconstruction shared by magnitude, signed hue, and
the planar projection. Float64 accumulation keeps a malicious or corrupted
Float32 field finite even when all seven inputs sit at the numeric limit.
"""
@inline function turbulence_filtered_scalar(
    field::AbstractArray{<:Real,3},
    x::Integer,
    y::Integer,
    z::Integer,
)
    i, j, k = x + 1, y + 1, z + 1
    filtered = 0.52 * turbulence_finite_scalar(field[i, j, k])
    filtered += 0.08 * turbulence_finite_scalar(field[i - 1, j, k])
    filtered += 0.08 * turbulence_finite_scalar(field[i + 1, j, k])
    filtered += 0.08 * turbulence_finite_scalar(field[i, j - 1, k])
    filtered += 0.08 * turbulence_finite_scalar(field[i, j + 1, k])
    filtered += 0.08 * turbulence_finite_scalar(field[i, j, k - 1])
    filtered += 0.08 * turbulence_finite_scalar(field[i, j, k + 1])
    limit = Float64(floatmax(Float32))
    return Float32(clamp(filtered, -limit, limit))
end

"""
Materialize the sparse signed-vorticity medium into the version-2 RGBA
volume. Nonzero voxels are kept away from every palette's near-white midpoint
and alpha never exceeds 0x1c, preserving color in the compositor's dedicated
low-alpha wake-scattering branch.
"""
function render_volume!(
    case::TurbulenceCase;
    palette::Tuple=case_palette(case),
)
    magnitude_field, signed_field = download_turbulence_vorticity!(case)
    nx, ny, nz = case.domain
    rgba = case.volume_rgba
    Threads.@threads :static for depth in 1:ny
        @inbounds for row in 1:nz
            vertical = nz - row + 1
            output = turbulence_volume_offset(nx, nz, 1, depth, vertical)
            for x in 1:nx
                magnitude = max(
                    turbulence_filtered_scalar(magnitude_field, x, depth, vertical),
                    0.0f0,
                )
                signed = turbulence_filtered_scalar(signed_field, x, depth, vertical)
                activity = max(magnitude - TURBULENCE_WAKE_FLOOR, 0.0f0)
                density = activity / (activity + TURBULENCE_WAKE_KNEE)
                orientation = clamp(
                    abs(signed) / max(magnitude, eps(Float32)),
                    0.0f0,
                    1.0f0,
                )
                hue = (signbit(signed) ? -1.0f0 : 1.0f0) *
                      (TURBULENCE_MIN_HUE + (1.0f0 - TURBULENCE_MIN_HUE) * orientation)
                color = palette_color(palette, hue, 1.0)

                rgba[output] = color[1]
                rgba[output + 1] = color[2]
                rgba[output + 2] = color[3]
                rgba[output + 3] = round(UInt8, Float32(TURBULENCE_MAX_ALPHA) * density)
                output += 4
            end
        end
    end
    return rgba
end

"""
Planar fallback: along every front-to-back ray retain the signed vorticity
sample with the greatest magnitude, apply a bounded soft knee, then bilinearly
upsample the compact tank projection to the requested display canvas. The
generic planar renderer supplies the palette and opaque version-1 alpha.
"""
function compute_vorticity!(scratch::RenderScratch, case::TurbulenceCase)
    _, signed_field = download_turbulence_vorticity!(case)
    nx, ny, nz = case.domain
    projected = case.projected
    @inbounds for vertical in 1:nz, x in 1:nx
        strongest = 0.0f0
        for depth in 1:ny
            value = turbulence_filtered_scalar(signed_field, x, depth, vertical)
            abs(value) > abs(strongest) && (strongest = value)
        end
        projected[x, vertical] =
            0.34f0 * strongest / (abs(strongest) + TURBULENCE_PLANAR_KNEE)
    end

    width, height = case.dimensions
    padded = scratch.padded_vorticity
    scale_x = Float32(nx) / width
    scale_z = Float32(nz) / height
    Threads.@threads :static for j in 1:(height + 2)
        gz = clamp((Float32(j) - 1.5f0) * scale_z + 0.5f0, 1.0f0, Float32(nz))
        iz = min(floor(Int, gz), nz - 1)
        fz = gz - iz
        @inbounds for i in 1:(width + 2)
            gx = clamp((Float32(i) - 1.5f0) * scale_x + 0.5f0, 1.0f0, Float32(nx))
            ix = min(floor(Int, gx), nx - 1)
            fx = gx - ix
            padded[i, j] =
                projected[ix, iz] * (1 - fx) * (1 - fz) +
                projected[ix + 1, iz] * fx * (1 - fz) +
                projected[ix, iz + 1] * (1 - fx) * fz +
                projected[ix + 1, iz + 1] * fx * fz
        end
    end
    return scratch
end
