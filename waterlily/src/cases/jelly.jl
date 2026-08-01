"""
Plain-data description of one animated bell.  The WaterLily `AutoBody`
closures below and the volume materializer both consume these same values,
so the visible membrane cannot drift away from the obstacle that generated
the flow.
"""
struct JellySpec
    radius::Float32
    x::Float32
    depth::Float32
    height::Float32
    phase::Float32
    angular_velocity::Float32
    sway_x::Float32
    sway_depth::Float32
    sway_height::Float32
    sway_phase_x::Float32
    sway_phase_depth::Float32
    sway_phase_height::Float32
    sway_rate_x::Float32
    sway_rate_depth::Float32
    sway_rate_height::Float32
end

"""
A smack of pulsing jellyfish in a true three-dimensional WaterLily solve,
adapted from the upstream `ThreeD_Jelly` example: each bell is a thin
spherical shell with its mouth cut off by a plane, breathing through the
`A`/`B`/`C` motion maps while a uniform downstream current balances its
swimming while smooth, independently seeded three-axis paths carry the smack
through the tank. Several jellies share the water at different positions,
sizes and pulse phases.

The display pipeline is natively three-dimensional: the case picks a 3D
domain from the canvas aspect ratio, materializes the analytic bell
membranes and curved trailing filaments together with the simulated
vorticity wake voxel-by-voxel, and
publishes the RGBA volume; the compositor ray-marches it through an orbiting
perspective camera. The magnitude feeds the positive half of the diverging
palettes: quiescent water stays transparent, while coherent translucent
bells and their shed rings receive reconstructed 3D normals and
view-dependent lighting. A planar fallback (`JWM_WATERLILY_PLANAR`) keeps
the historical line-integral projection for consumers without volumetric
support.
"""
struct JellyCase{S} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    domain::NTuple{3,Int}
    jellies::Vector{JellySpec}
    # Host-side copy of the solver's scalar field; on GPU backends the
    # projection runs after one bulk download instead of scalar reads.
    sigma_host::Array{Float32,3}
    # Maximum-intensity projection of vorticity magnitude along depth,
    # (nx, nz). Only used by the planar fallback path.
    projected::Matrix{Float32}
    # Version-2 volume scratch: nx * nz * ny RGBA voxels, slices front to
    # back, reused every frame to keep the publish loop garbage-free.
    volume_rgba::Vector{UInt8}
end

const JELLY_REYNOLDS = 500.0
const JELLY_COUNT = 5
# Bell radius as a fraction of the tank height. Smaller than the upstream
# example's ratio so five independently moving bells remain separated and
# their pulse wakes have room to decay before the floor.
const JELLY_RADIUS_FRACTION = 0.105

"""
Smooth quasi-random roaming shared by the solver body and display material.

Two incommensurate harmonics on each of the three tank axes avoid synchronized
pendulum motion, while bounded amplitudes keep every bell, crown and trailing
filament inside the tank. The generic time argument intentionally supports
ForwardDiff dual numbers used by `AutoBody` to derive body velocity.
"""
function jelly_center(spec::JellySpec, t::Real)
    x_primary = sin(spec.sway_rate_x * t + spec.sway_phase_x)
    x_secondary = sin(
        1.71f0 * spec.sway_rate_x * t + spec.sway_phase_depth + 1.13f0,
    )
    depth_primary = sin(spec.sway_rate_depth * t + spec.sway_phase_depth)
    depth_secondary = cos(
        1.43f0 * spec.sway_rate_depth * t + spec.sway_phase_x + 0.67f0,
    )
    height_primary = sin(spec.sway_rate_height * t + spec.sway_phase_height)
    height_secondary = cos(
        1.57f0 * spec.sway_rate_height * t +
        spec.sway_phase_x -
        spec.sway_phase_depth +
        0.41f0,
    )
    center_x = spec.x + spec.sway_x * (0.72f0 * x_primary + 0.28f0 * x_secondary)
    center_depth = spec.depth +
                   spec.sway_depth *
                   (0.68f0 * depth_primary + 0.32f0 * depth_secondary)
    center_height = spec.height +
                    spec.sway_height *
                    (0.71f0 * height_primary + 0.29f0 * height_secondary)
    return (center_x, center_depth, center_height)
end

# Retain the two-axis helper for callers that only need the horizontal tank
# coordinates; all body and material transforms use `jelly_center` directly.
function jelly_lateral_center(spec::JellySpec, t::Real)
    center_x, center_depth, _ = jelly_center(spec, t)
    return (center_x, center_depth)
end

"""
Pick the 3D tank from the canvas: the vertical resolution sets fidelity and
cost (32³·aspect cells at the test sizes, ~half a million at a full display),
the width follows the display aspect so jellies stay round after the
compositor stretch, and every extent is a multiple of 16 for the multigrid
pressure solver.
"""
function jelly_domain(dimensions::Tuple{Int,Int})
    width, height = dimensions
    # Capped at 64 layers: the 80-layer tank measured ~190 ms per CPU frame,
    # deep slow motion, while 64 keeps the smack near the pace of the other
    # fluid cases. GPU backends can afford more via --sim-size upscaling.
    nz = 16 * clamp(round(Int, height / 160), 2, 4)
    aspect = width / height
    nx = 16 * clamp(round(Int, nz * aspect / 16), 2, 12)
    ny = 16 * max(1, round(Int, nz * 0.6 / 16))
    return (nx, ny, nz)
end

function build_jelly_case(
    dimensions::Tuple{Int,Int};
    memory=Array,
    reynolds::Real=JELLY_REYNOLDS,
)
    T = Float32
    nx, ny, nz = domain = jelly_domain(dimensions)
    U = T(1)
    radius = T(JELLY_RADIUS_FRACTION * nz)
    count = JELLY_COUNT
    lane_width = T(nx) / T(count)

    specs = map(1:count) do index
        R = radius * (T(0.82) + T(0.25) * rand(T))
        sway_x = min(T(0.72) * R, T(0.24) * lane_width) *
                 (T(0.68) + T(0.32) * rand(T))
        sway_depth = min(T(0.62) * R, T(0.18) * ny) *
                     (T(0.65) + T(0.35) * rand(T))
        sway_height = min(T(0.78) * R, T(0.10) * nz) *
                      (T(0.65) + T(0.35) * rand(T))

        # One stratified lane per jelly keeps the smack spread across the
        # canvas. Depth and height use different low-discrepancy strides, so
        # neighbouring screen lanes do not also form a flat parade rank. The
        # The full one-cell shell radius grows by at most 1/0.9 under the
        # pulse map; reserve that exact envelope as well as the roaming span.
        expanded_shell_radius = (R + T(1)) / T(0.9)
        lane_center = (T(index) - T(0.5)) * lane_width
        x_margin = min(expanded_shell_radius + sway_x, T(0.48) * nx)
        px = clamp(
            lane_center + (rand(T) - T(0.5)) * T(0.20) * lane_width,
            x_margin,
            T(nx) - x_margin,
        )
        depth_fraction = mod(T(0.14) + T(index - 1) * T(0.37), one(T))
        depth_margin = min(expanded_shell_radius + sway_depth, T(0.48) * ny)
        py = depth_margin + depth_fraction * (T(ny) - 2depth_margin)

        # The pulse can lift the crown 1.604R above the roaming center, while
        # the lowest tentacle tip reaches 2.60R below it. Reserve those full
        # envelopes plus the roaming amplitude, then distribute base heights
        # across the complete safe interval rather than clustering at the top.
        height_fraction = mod(T(0.12) + T(index - 1) * T(0.618_034), one(T))
        lowest_center = T(2.62) * R + sway_height + T(1.25)
        # The compositor draws the water surface near 0.94nz. Keeping the
        # complete crown below 0.92nz leaves a visible layer of water above
        # every swimmer instead of letting bells break through the surface.
        highest_center =
            T(0.92) * T(nz) - T(1.62) * R - sway_height - T(1.25)
        h = lowest_center + height_fraction * (highest_center - lowest_center)

        phase = T(2pi) * rand(T)
        ω = 2U / R
        sway_phase_x = T(2pi) * rand(T)
        sway_phase_depth = T(2pi) * rand(T)
        sway_phase_height = T(2pi) * rand(T)
        sway_rate_x = ω * (T(0.20) + T(0.18) * rand(T))
        sway_rate_depth = ω * (T(0.17) + T(0.22) * rand(T))
        sway_rate_height = ω * (T(0.13) + T(0.18) * rand(T))
        JellySpec(
            R,
            px,
            py,
            h,
            phase,
            ω,
            sway_x,
            sway_depth,
            sway_height,
            sway_phase_x,
            sway_phase_depth,
            sway_phase_height,
            sway_rate_x,
            sway_rate_depth,
            sway_rate_height,
        )
    end

    jellies = map(specs) do spec
        R = spec.radius
        phase = spec.phase
        ω = spec.angular_velocity
        # The bell: a thin spherical shell breathing through the upstream
        # example's maps — radial squeeze `A`, recoil `B`, and heave `C` —
        # with the mouth plane sharing `C` so the cut rides the pulse. The
        # maps stay generic in `t`: WaterLily measures body velocity by
        # forward-mode differentiation, so a `T(...)` around anything
        # time-dependent would reject the ForwardDiff duals.
        bell = WaterLily.AutoBody(
            (x, t) -> abs(√sum(abs2, x) - R) - 1.0f0,
            let spec = spec, phase = phase, ω = ω, R = R
                function (x, t)
                    θ = ω * t + phase
                    center_x, center_depth, center_height = jelly_center(spec, t)
                    squeeze = 1 .- SA[1, 1, 0] .* (cos(θ) / 10)
                    recoil =
                        SA[0, 0, 1] .* ((cos(θ) - 1) * R / 4 - center_height)
                    heave = SA[0, 0, 1] .* (sin(θ) * R / 4)
                    return squeeze .* (x - SA[center_x, center_depth, 0.0f0]) +
                           recoil + heave
                end
            end,
        )
        mouth = WaterLily.AutoBody(
            (x, t) -> x[3],
            let spec = spec, phase = phase, ω = ω, R = R
                function (x, t)
                    center_x, center_depth, center_height = jelly_center(spec, t)
                    heave = sin(ω * t + phase) * R / 4
                    # X/depth do not change this plane's SDF, but retaining
                    # the complete inverse translation lets AutoBody derive
                    # the same lateral rim velocity as the moving bell.
                    return x .- SA[center_x, center_depth, center_height] +
                           SA[0, 0, heave]
                end
            end,
        )
        bell - mouth
    end

    simulation = WaterLily.Simulation(
        domain,
        (T(0), T(0), -U),
        radius;
        U,
        ν=U * radius / T(reynolds),
        body=reduce(∪, jellies),
        T,
        mem=memory,
    )
    return JellyCase(
        simulation,
        dimensions,
        domain,
        specs,
        Array{Float32,3}(undef, nx + 2, ny + 2, nz + 2),
        Matrix{Float32}(undef, nx, nz),
        Vector{UInt8}(undef, 4 * nx * nz * ny),
    )
end

case_palette_name(::JellyCase) = "violet"

# Planar fallback still renders the projected vorticity rather than trying
# to flatten the 3D membrane into the generic 2D body overlay.
body_distance(::JellyCase, ::Real, ::Real, ::Real) = Inf

# The tank publishes as a native volume: frame width spans the tank, frame
# height is the vertical extent (top-left rows, like every planar frame),
# and the ny tank layers become front-to-back depth slices.
frame_geometry(case::JellyCase) = (case.domain[1], case.domain[3], case.domain[2])

# Ambient-wake floor shared by the volume transfer function and the planar
# projection; only shells and coherent shed rings contribute.
const JELLY_WAKE_FLOOR = 0.6f0

"""
Signed distance to one animated bell membrane at solver time `t`.

This is the explicit form of the `AutoBody` construction in
[`build_jelly_case`](@ref): an approximately one-cell-thick breathing sphere
with its lower half removed at the moving mouth plane.  Keeping this
calculation next to the volume transfer function gives the compositor real
surface voxels to shade instead of asking it to infer a jelly body from the
surrounding vorticity cloud.
"""
function jelly_signed_distance(
    spec::JellySpec,
    x::Real,
    y::Real,
    z::Real,
    t::Real,
)
    θ = spec.angular_velocity * t + spec.phase
    squeeze = 1.0f0 - cos(θ) / 10.0f0
    center_x, center_depth, center_height = jelly_center(spec, t)
    local_x = squeeze * (x - center_x)
    local_y = squeeze * (y - center_depth)
    heave = sin(θ) * spec.radius / 4.0f0
    local_z =
        z - center_height + (cos(θ) - 1.0f0) * spec.radius / 4.0f0 + heave
    shell = abs(sqrt(local_x^2 + local_y^2 + local_z^2) - spec.radius) - 1.0f0
    mouth = z + heave - center_height
    # Set difference `bell - mouth`: retain the shell only above the mouth
    # plane, exactly as WaterLily's body-composition operator does.
    return max(shell, -mouth)
end

jelly_smoothstep(edge0::Float32, edge1::Float32, value::Real) = let
    q = clamp((Float32(value) - edge0) / (edge1 - edge0), 0.0f0, 1.0f0)
    q * q * (3.0f0 - 2.0f0 * q)
end

function jelly_tentacle_center(spec::JellySpec, strand::Integer, q::Real, t::Real)
    theta = spec.angular_velocity * t + spec.phase
    jelly_x, jelly_depth, jelly_height = jelly_center(spec, t)
    heave = sin(theta) * spec.radius / 4.0f0
    fraction = Float32(q)
    angle = Float32(2pi * strand / 5) + 0.18f0 * sin(theta)
    anchor = 0.34f0 * spec.radius * (1.0f0 - 0.42f0 * fraction)
    wave = theta + Float32(1.7pi) * fraction + Float32(1.9 * strand)
    sway = spec.radius * (0.035f0 + 0.12f0 * fraction)
    center_x = jelly_x + anchor * cos(angle) + sway * sin(wave)
    center_y = jelly_depth + anchor * sin(angle) + sway * cos(0.83f0 * wave)
    center_z = jelly_height - heave - 2.35f0 * spec.radius * fraction
    strand_radius = clamp(
        0.12f0 * spec.radius * (1.0f0 - 0.52f0 * fraction),
        0.30f0,
        0.78f0,
    )
    return (center_x, center_y, center_z, strand_radius)
end

"""
Coverage of the animated trailing filaments below one bell.  These strands
are display material rather than solid solver obstacles: keeping them out of
the `AutoBody` preserves the upstream propulsion model while giving the
volumetric renderer the unmistakable silhouette and depth crossings of a
jellyfish instead of a set of isolated spherical caps.
"""
function jelly_tentacle_coverage(
    spec::JellySpec,
    x::Real,
    y::Real,
    z::Real,
    t::Real,
)
    theta = spec.angular_velocity * t + spec.phase
    _, _, jelly_height = jelly_center(spec, t)
    mouth_z = jelly_height - sin(theta) * spec.radius / 4.0f0
    length = 2.35f0 * spec.radius
    q = (mouth_z - z) / length
    (q < 0.0f0 || q > 1.0f0) && return 0.0f0

    coverage = 0.0f0
    for strand in 0:4
        center_x, center_y, _, strand_radius =
            jelly_tentacle_center(spec, strand, q, t)
        radial_distance = hypot(x - center_x, y - center_y) - strand_radius
        strand_coverage = clamp(0.5f0 - radial_distance, 0.0f0, 1.0f0)
        # Taper both ends so the filaments join the mouth smoothly and fade
        # before hitting the lower tank boundary.
        end_fade = jelly_smoothstep(0.0f0, 0.08f0, q) *
                   (1.0f0 - jelly_smoothstep(0.78f0, 1.0f0, q))
        coverage = max(coverage, strand_coverage * end_fade)
    end
    return coverage
end

function jelly_material_coverage(case::JellyCase, x::Real, y::Real, z::Real, t::Real)
    surface = 0.0f0
    tentacles = 0.0f0
    @inbounds for jelly in case.jellies
        surface = max(
            surface,
            clamp(
                0.5f0 - Float32(jelly_signed_distance(jelly, x, y, z, t)),
                0.0f0,
                1.0f0,
            ),
        )
        tentacles = max(tentacles, jelly_tentacle_coverage(jelly, x, y, z, t))
    end
    return (surface, tentacles)
end

jelly_surface_coverage(case::JellyCase, x::Real, y::Real, z::Real, t::Real) =
    first(jelly_material_coverage(case, x, y, z, t))

# A published voxel numbered `index` reads solver storage at `index + 1`;
# WaterLily's staggered-grid convention places that cell centre at I - 1.5.
@inline jelly_voxel_center(index::Integer) = Float32(index) - 0.5f0

"""
Evaluate the vorticity magnitude on the device and download it once into the
host scratch. Both display paths (native volume and planar projection) start
from this field.
"""
function download_vorticity_magnitude!(case::JellyCase)
    simulation = case.simulation
    u = simulation.flow.u
    σ = simulation.flow.σ
    scale = eltype(σ)(simulation.L / simulation.U)
    WaterLily.@inside σ[I] = WaterLily.ω_mag(I, u) * scale
    copyto!(case.sigma_host, σ)
    return case.sigma_host
end

"""
Materialize the animated analytic membranes and colorize the vorticity
magnitude voxel-by-voxel into the version-2 volume buffer. Emission runs
through the palette's positive half while lavender membranes and trailing
filaments give the compositor coherent surfaces from which to reconstruct 3D
normals. Quiescent water remains transparent; opacity follows a soft rational
knee for the wake and bounded translucent densities for the body material.
"""
function render_volume!(case::JellyCase; palette::Tuple=case_palette(case))
    sigma = download_vorticity_magnitude!(case)
    nx, ny, nz = case.domain
    rgba = case.volume_rgba
    τ = Float32(simulation_time(case))
    # A lifted lavender reads as translucent tissue after several depth
    # samples have accumulated.  The darker UI accent lavender is suitable
    # for an opaque 2D body, but in a volume it turned dense shell crossings
    # into charcoal dots over bright windows.
    membrane_color = (UInt8(0xc4), UInt8(0xbe), UInt8(0xff))
    Threads.@threads :static for y in 1:ny
        @inbounds for row in 1:nz
            z = nz - row + 1
            output = 4 * ((y - 1) * nz + (row - 1)) * nx + 1
            for x in 1:nx
                value = sigma[x + 1, y + 1, z + 1]
                wake = isfinite(value) ? max(value - JELLY_WAKE_FLOOR, 0.0f0) : 0.0f0
                # Soft knee keeps the shell/wake dynamic range on the palette
                # without clipping; the deterministic mapping avoids per-frame
                # autoscale flicker in the marched image.
                wake_density = wake / (wake + 6.0f0)
                surface, tentacles = jelly_material_coverage(
                    case,
                    # `sigma[x + 1, ...]` is WaterLily's first interior
                    # cell, whose physical centre is `loc(0, I) = I - 1.5`.
                    # Sample the analytic tissue at that exact point so its
                    # bell/tentacles remain registered with the simulated
                    # wake instead of being shifted by one voxel per axis.
                    jelly_voxel_center(x),
                    jelly_voxel_center(y),
                    jelly_voxel_center(z),
                    τ,
                )
                # All materials remain genuinely translucent.  A ray crosses
                # many voxels, so modest per-cell absorption is enough to form
                # a legible bell.  Keeping the turbulent wake below the tissue
                # density prevents individual high-vorticity cells from
                # becoming opaque pepper-like particles.
                density = max(
                    0.22f0 * wake_density,
                    0.30f0 * surface,
                    0.34f0 * tentacles,
                )
                # Reserve the palette's darkest positive endpoint for the
                # planar plot.  Volumetric color is integrated repeatedly and
                # therefore uses the luminous middle of the green ramp.
                flow_color = palette_color(palette, 0.48f0 * wake_density, 1.0)
                material = max(surface, 0.88f0 * tentacles)
                surface_mix = clamp(
                    material * (0.82f0 + 0.18f0 * (1.0f0 - wake_density)),
                    0.0f0,
                    1.0f0,
                )
                color = blend_color(
                    flow_color,
                    membrane_color,
                    Float64(surface_mix),
                )
                rgba[output] = color[1]
                rgba[output + 1] = color[2]
                rgba[output + 2] = color[3]
                # Per-voxel opacity for one straight-through voxel crossing;
                # the shader renormalizes it by its actual step length.
                rgba[output + 3] = round(UInt8, 160 * density)
                output += 4
            end
        end
    end
    return rgba
end

"""
Fill the 2D vorticity scratch from the 3D solve: evaluate the vorticity
magnitude on the device, download once, take the maximum along each depth
ray, and upsample it bilinearly to the display grid. The default renderer
then colors it exactly like a native 2D case, using the palette's positive
half. This is the planar fallback path (`JWM_WATERLILY_PLANAR`).
"""
function compute_vorticity!(scratch::RenderScratch, case::JellyCase)
    download_vorticity_magnitude!(case)
    nx, ny, nz = case.domain
    sigma = case.sigma_host
    projected = case.projected
    @inbounds for z in 1:nz, x in 1:nx
        total = 0.0f0
        for y in 1:ny
            value = sigma[x + 1, y + 1, z + 1]
            isfinite(value) || continue
            # The per-cell floor keeps weak ambient wake from integrating
            # into a broad halo across the depth of the tank; only shells
            # and coherent shed rings contribute.
            total += max(value - JELLY_WAKE_FLOOR, 0.0f0)
        end
        # X-ray line integral instead of a max projection: rays grazing a
        # bell tangentially run a long chord through its shell while rays
        # through the middle only cross it twice, so bells get bright rims
        # over dimmer translucent interiors instead of one flat silhouette.
        # The soft knee compresses the shell/wake dynamic range, scaled
        # under the renderer's 0.35 autoscale floor so the mapping stays
        # deterministic and never clips.
        projected[x, z] = 0.34f0 * total / (total + 40.0f0)
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
