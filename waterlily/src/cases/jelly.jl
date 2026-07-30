"""
A smack of pulsing jellyfish in a true three-dimensional WaterLily solve,
adapted from the upstream `ThreeD_Jelly` example: each bell is a thin
spherical shell with its mouth cut off by a plane, breathing through the
`A`/`B`/`C` motion maps while a uniform downstream current balances its
swimming so the smack holds station. Several jellies share the tank at
different positions, sizes and pulse phases.

The display pipeline is natively three-dimensional: the case picks a 3D
domain from the canvas aspect ratio, colorizes the vorticity magnitude
voxel-by-voxel, and publishes the RGBA volume; the compositor ray-marches
it through an orbiting perspective camera. The magnitude feeds the positive
half of the diverging palettes: quiescent water stays on the keyed white
midpoint and frosts out, while the bells and their shed rings deepen into
the palette's warm side. A planar fallback (`JWM_WATERLILY_PLANAR`) keeps
the historical line-integral projection for consumers without volumetric
support.
"""
struct JellyCase{S} <: AbstractWaterLilyCase
    simulation::S
    dimensions::Tuple{Int,Int}
    domain::NTuple{3,Int}
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
# Bell radius as a fraction of the tank height. Smaller than the upstream
# example's ratio so the pulse wakes have room to decay before the floor —
# at 0.175 the trailing columns filled the entire canvas.
const JELLY_RADIUS_FRACTION = 0.13

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
    count = clamp(floor(Int, nx / (3.6 * radius)), 1, 4)

    jellies = map(1:count) do index
        R = radius * (T(0.78) + T(0.35) * rand(T))
        # Station-keeping height near the top of the tank, with per-jelly
        # spread so the smack is staggered instead of a parade rank.
        h = T(nz) - 2R - T(0.6) * radius * rand(T)
        px = T(nx) * (T(index) - T(0.5)) / T(count) +
             (rand(T) - T(0.5)) * T(0.3) * radius
        py = T(ny) / 2 + (rand(T) - T(0.5)) * T(0.25) * ny
        phase = T(2pi) * rand(T)
        ω = 2U / R

        # The bell: a thin spherical shell breathing through the upstream
        # example's maps — radial squeeze `A`, recoil `B`, and heave `C` —
        # with the mouth plane sharing `C` so the cut rides the pulse. The
        # maps stay generic in `t`: WaterLily measures body velocity by
        # forward-mode differentiation, so a `T(...)` around anything
        # time-dependent would reject the ForwardDiff duals.
        bell = WaterLily.AutoBody(
            (x, t) -> abs(√sum(abs2, x) - R) - 1.0f0,
            let px = px, py = py, phase = phase, ω = ω, R = R, h = h
                function (x, t)
                    θ = ω * t + phase
                    squeeze = 1 .- SA[1, 1, 0] .* (cos(θ) / 10)
                    recoil = SA[0, 0, 1] .* ((cos(θ) - 1) * R / 4 - h)
                    heave = SA[0, 0, 1] .* (sin(θ) * R / 4)
                    return squeeze .* (x - SA[px, py, 0.0f0]) + recoil + heave
                end
            end,
        )
        mouth = WaterLily.AutoBody(
            let h = h
                (x, t) -> x[3] - h
            end,
            let phase = phase, ω = ω, R = R
                (x, t) -> x .+ SA[0, 0, 1] .* (sin(ω * t + phase) * R / 4)
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
        Array{Float32,3}(undef, nx + 2, ny + 2, nz + 2),
        Matrix{Float32}(undef, nx, nz),
        Vector{UInt8}(undef, 4 * nx * nz * ny),
    )
end

case_palette_name(::JellyCase) = "violet"

# The bells render through the vorticity they shed, not as a painted body.
body_distance(::JellyCase, ::Real, ::Real, ::Real) = Inf

# The tank publishes as a native volume: frame width spans the tank, frame
# height is the vertical extent (top-left rows, like every planar frame),
# and the ny tank layers become front-to-back depth slices.
frame_geometry(case::JellyCase) = (case.domain[1], case.domain[3], case.domain[2])

# Ambient-wake floor shared by the volume transfer function and the planar
# projection; only shells and coherent shed rings contribute.
const JELLY_WAKE_FLOOR = 0.6f0

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
Colorize the vorticity magnitude voxel-by-voxel into the version-2 volume
buffer. Emission runs through the palette's positive half — quiescent water
stays on the keyed near-white midpoint — while opacity follows a soft
rational knee of the wake strength, so bell shells read as translucent
membranes whose tangent chords brighten into rims, exactly the depth cue the
old line-integral projection faked in 2D and the compositor's ray-marcher
now produces for real.
"""
function render_volume!(case::JellyCase; palette::Tuple=case_palette(case))
    sigma = download_vorticity_magnitude!(case)
    nx, ny, nz = case.domain
    rgba = case.volume_rgba
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
                density = wake / (wake + 6.0f0)
                color = palette_color(palette, density, 1.0)
                rgba[output] = color[1]
                rgba[output + 1] = color[2]
                rgba[output + 2] = color[3]
                # Per-voxel opacity for one straight-through voxel crossing;
                # the shader renormalizes it by its actual step length.
                rgba[output + 3] = round(UInt8, 217 * density)
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
