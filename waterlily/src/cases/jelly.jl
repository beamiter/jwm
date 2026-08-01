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
domain from the canvas aspect ratio, materializes the analytic anatomy —
apex-to-rim shaded bell membranes, the rose gonad crown visible through
each translucent bell, four thick curling oral arms, and five thin trailing
filaments — together with the simulated vorticity wake voxel-by-voxel, and
publishes the RGBA volume; the compositor ray-marches it through an orbiting
perspective camera. The magnitude feeds the positive half of the diverging
palettes: quiescent water stays transparent, while coherent translucent
tissue and shed vortex rings receive continuous volumetric illumination; the
compositor uses a stable shallow-interface normal only for its subtle
directional cue and scene refraction. A planar fallback
(`JWM_WATERLILY_PLANAR`) keeps
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
Pick the 3D tank from the canvas. Vertical resolution sets fidelity and cost,
width follows the display aspect so jellies stay round after compositor
projection, and every extent is a multiple of 16 for the multigrid pressure
solver. CPU publication is capped at 64 vertical cells; accelerated backends
use an 80-cell ceiling so a 1280x800 canvas rises from a 96x32x64 CPU domain
to 128x48x80 on CUDA or ROCm.
"""
function jelly_domain(
    dimensions::Tuple{Int,Int};
    accelerated::Bool=false,
)
    width, height = dimensions
    # CPU stays capped at 64 layers: the 80-layer tank measured ~190 ms per
    # frame there. CUDA/ROCm get an 80-layer ceiling, which raises a 1280x800
    # publication from 96x64x32 to 128x80x48; the finer curved coverage is a
    # direct spatial antialiasing improvement before compositor reconstruction.
    layer_cap = accelerated ? 5 : 4
    nz = 16 * clamp(round(Int, height / 160), 2, layer_cap)
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
    nx, ny, nz = domain = jelly_domain(
        dimensions;
        accelerated=memory !== Array,
    )
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

"""
Byte offset (one-based, pointing at R) of solver cell `(x, depth, vertical)`
inside the published RGBA volume. Slices advance front-to-back in solver
depth, while rows run top-to-bottom, hence the vertical-axis reversal. Keeping
this mapping explicit makes the non-square transport contract independently
testable instead of hiding it in a loop initializer.
"""
@inline function jelly_volume_offset(
    nx::Integer,
    nz::Integer,
    x::Integer,
    depth::Integer,
    vertical::Integer,
)
    top_row = nz - vertical
    return 4 * (((depth - 1) * nz + top_row) * nx + (x - 1)) + 1
end

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

# Display-material palette of one swimmer: the translucent membrane fades
# from a lifted rim lavender to a deeper apex violet, the four gonads sit in
# the bell cavity as the classic rose four-leaf crown, and the oral arms
# trail in a pale blush distinct from the thin lavender filaments.
const JELLY_APEX_VIOLET = (UInt8(0xb8), UInt8(0x9c), UInt8(0xf6))
const JELLY_RIM_LAVENDER = (UInt8(0xd8), UInt8(0xd4), UInt8(0xff))
const JELLY_FILAMENT_LAVENDER = (UInt8(0xc4), UInt8(0xbe), UInt8(0xff))
const JELLY_ARM_BLUSH = (UInt8(0xd9), UInt8(0xb8), UInt8(0xe8))
const JELLY_ORGAN_ROSE = (UInt8(0xe8), UInt8(0x84), UInt8(0xb0))

"""
Frame-constant pose of one bell: every trigonometric quantity the voxel
materializer needs, computed once per jelly per frame instead of once per
voxel.  `axis_shift` is the recoil-plus-heave displacement the pulse maps
apply along the tank height; `strand_dir`, `arm_dir`, and `organ_local` are
the frame-constant directions and squeezed-frame gonad centers of the
appendages; and the bounding data (`reach_sq`, `z_lo`, `z_hi`)
conservatively encloses the bell, crown, gonads, oral arms, and trailing
filaments so the materializer can reject far voxels with two
multiplications and two comparisons.
"""
struct JellyPose
    spec::JellySpec
    center_x::Float32
    center_y::Float32
    center_z::Float32
    theta::Float32
    squeeze::Float32
    heave::Float32
    axis_shift::Float32
    mouth_z::Float32
    reach_sq::Float32
    z_lo::Float32
    z_hi::Float32
    strand_dir::NTuple{5,NTuple{2,Float32}}
    arm_dir::NTuple{4,NTuple{2,Float32}}
    organ_local::NTuple{4,NTuple{3,Float32}}
    organ_radius::Float32
end

function jelly_pose(spec::JellySpec, t::Real)
    θ = Float32(spec.angular_velocity * t + spec.phase)
    squeeze = 1.0f0 - cos(θ) / 10.0f0
    center_x, center_y, center_z = Float32.(jelly_center(spec, t))
    R = spec.radius
    heave = sin(θ) * R / 4.0f0
    axis_shift = (cos(θ) - 1.0f0) * R / 4.0f0 + heave
    mouth_z = center_z - heave
    # The 2.0 margin covers the widened analytic feather (about 0.8 voxels
    # beyond the one-voxel shell) plus the strand radii on every side.
    lateral_reach = max((R + 2.0f0) / 0.9f0, 0.62f0 * R + 2.2f0)
    strand_dir = ntuple(5) do index
        angle = Float32(2pi * (index - 1) / 5) + 0.18f0 * sin(θ)
        (cos(angle), sin(angle))
    end
    arm_dir = ntuple(4) do index
        angle = Float32(index - 1) * 1.5707964f0 + 0.785f0 + 0.22f0 * sin(θ)
        (cos(angle), sin(angle))
    end
    organ_radius = max(0.15f0 * R, 0.8f0)
    organ_local = ntuple(4) do index
        angle = Float32(index - 1) * 1.5707964f0 + 0.4f0 + 0.15f0 * sin(θ)
        (0.40f0 * R * cos(angle), 0.40f0 * R * sin(angle), 0.30f0 * R)
    end
    return JellyPose(
        spec,
        center_x,
        center_y,
        center_z,
        θ,
        squeeze,
        heave,
        axis_shift,
        mouth_z,
        lateral_reach * lateral_reach,
        mouth_z - 2.35f0 * R - 2.0f0,
        center_z - axis_shift + R + 2.0f0,
        strand_dir,
        arm_dir,
        organ_local,
        organ_radius,
    )
end

function pose_tentacle_center(pose::JellyPose, strand::Integer, q::Real)
    R = pose.spec.radius
    fraction = Float32(q)
    direction = pose.strand_dir[strand + 1]
    anchor = 0.34f0 * R * (1.0f0 - 0.42f0 * fraction)
    wave = pose.theta + Float32(1.7pi) * fraction + Float32(1.9 * strand)
    sway = R * (0.035f0 + 0.12f0 * fraction)
    center_x = pose.center_x + anchor * direction[1] + sway * sin(wave)
    center_y = pose.center_y + anchor * direction[2] + sway * cos(0.83f0 * wave)
    center_z = pose.mouth_z - 2.35f0 * R * fraction
    strand_radius = clamp(
        0.12f0 * R * (1.0f0 - 0.52f0 * fraction),
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
function pose_tentacle_coverage(pose::JellyPose, x::Real, y::Real, z::Real)
    q = (pose.mouth_z - z) / (2.35f0 * pose.spec.radius)
    (q < 0.0f0 || q > 1.0f0) && return 0.0f0
    # Taper both ends so the filaments join the mouth smoothly and fade
    # before hitting the lower tank boundary.
    end_fade = jelly_smoothstep(0.0f0, 0.08f0, q) *
               (1.0f0 - jelly_smoothstep(0.78f0, 1.0f0, q))
    end_fade <= 0.0f0 && return 0.0f0
    coverage = 0.0f0
    for strand in 0:4
        center_x, center_y, _, strand_radius =
            pose_tentacle_center(pose, strand, q)
        radial_distance = hypot(x - center_x, y - center_y) - strand_radius
        strand_coverage = clamp(0.5f0 - radial_distance, 0.0f0, 1.0f0)
        coverage = max(coverage, strand_coverage * end_fade)
    end
    return coverage
end

function pose_arm_center(pose::JellyPose, arm::Integer, q::Real)
    R = pose.spec.radius
    fraction = Float32(q)
    direction = pose.arm_dir[arm + 1]
    anchor = R * (0.30f0 - 0.10f0 * fraction)
    wave = pose.theta + 4.084f0 * fraction + 1.5707964f0 * Float32(arm)
    sway = R * (0.06f0 + 0.16f0 * fraction)
    center_x = pose.center_x + anchor * direction[1] + sway * sin(wave)
    center_y = pose.center_y + anchor * direction[2] + sway * cos(0.77f0 * wave)
    center_z = pose.mouth_z - 1.7f0 * R * fraction
    arm_radius = clamp(
        0.17f0 * R * (1.0f0 - 0.45f0 * fraction),
        0.55f0,
        1.35f0,
    )
    return (center_x, center_y, center_z, arm_radius)
end

"""
Coverage of the four frilled oral arms below the mouth.  They are shorter
and markedly thicker than the filaments, curl on their own wave, and give
the silhouette the fleshy center real moon jellies have; like the filaments
they are display material only.
"""
function pose_arm_coverage(pose::JellyPose, x::Real, y::Real, z::Real)
    q = (pose.mouth_z - z) / (1.7f0 * pose.spec.radius)
    (q < 0.0f0 || q > 1.0f0) && return 0.0f0
    end_fade = jelly_smoothstep(0.0f0, 0.06f0, q) *
               (1.0f0 - jelly_smoothstep(0.80f0, 1.0f0, q))
    end_fade <= 0.0f0 && return 0.0f0
    coverage = 0.0f0
    for arm in 0:3
        center_x, center_y, _, arm_radius = pose_arm_center(pose, arm, q)
        radial_distance = hypot(x - center_x, y - center_y) - arm_radius
        arm_coverage = clamp(0.5f0 - 0.8f0 * radial_distance, 0.0f0, 1.0f0)
        coverage = max(coverage, arm_coverage * end_fade)
    end
    return coverage
end

"""
Coverage of the four gonads: the rose four-leaf crown visible through the
translucent bell of a real moon jelly.  Their centers live in the squeezed
bell frame (`local_*` coordinates), so they breathe and heave with the
pulse maps exactly like the membrane around them.
"""
function pose_organ_coverage(
    pose::JellyPose,
    local_x::Real,
    local_y::Real,
    local_z::Real,
)
    coverage = 0.0f0
    for organ in pose.organ_local
        distance = sqrt(
            (local_x - organ[1])^2 +
            (local_y - organ[2])^2 +
            (local_z - organ[3])^2,
        ) - pose.organ_radius
        coverage = max(coverage, clamp(0.5f0 - 0.75f0 * distance, 0.0f0, 1.0f0))
    end
    return coverage
end

"""
Every display material of one swimmer at one voxel: bell membrane coverage
with its apex-to-rim polar coordinate, thin trailing filaments, the four
oral arms, and the gonad crown.  The bounding test rejects the vast
majority of voxel/jelly pairs before any per-voxel trigonometry runs.
"""
function pose_material(pose::JellyPose, x::Float32, y::Float32, z::Float32)
    dx = x - pose.center_x
    dy = y - pose.center_y
    if dx * dx + dy * dy > pose.reach_sq || z < pose.z_lo || z > pose.z_hi
        return (0.0f0, 0.0f0, 0.0f0, 0.0f0, 0.0f0)
    end
    R = pose.spec.radius
    local_x = pose.squeeze * dx
    local_y = pose.squeeze * dy
    local_z = z - pose.center_z + pose.axis_shift
    shell = abs(sqrt(local_x^2 + local_y^2 + local_z^2) - R) - 1.0f0
    mouth = pose.mouth_z - z
    # Round the shell∩mouth corner of the displayed membrane: the exact
    # set-difference edge is a sharp circular lip whose voxelized coverage
    # alternates cell by cell and drew a sawtooth fringe under the old
    # high-contrast shading. A ~one-voxel polynomial smooth-max recesses
    # and rounds the lip; the solver keeps the exact AutoBody cut.
    lip_blend = clamp(0.5f0 + 0.5f0 * (shell - mouth) / 1.2f0, 0.0f0, 1.0f0)
    lip = mouth + (shell - mouth) * lip_blend +
          1.2f0 * lip_blend * (1.0f0 - lip_blend)
    # The coverage feather spans about 1.6 voxels: a one-voxel ramp beat
    # against the coarse grid along the curved dome and drew concentric
    # moiré rings once the old high-contrast transfer raised them. The wider
    # analytic ramp reconstructs smoothly while retaining a crisp silhouette.
    surface = clamp(0.5f0 - 0.62f0 * lip, 0.0f0, 1.0f0)
    polar = clamp(local_z / max(R, 1.0f-4), -1.0f0, 1.0f0)
    organs = 0.0f0
    organ_reach = 0.55f0 * R + pose.organ_radius + 1.0f0
    if local_z > 0.0f0 &&
       local_x^2 + local_y^2 + local_z^2 < organ_reach^2
        organs = pose_organ_coverage(pose, local_x, local_y, local_z)
    end
    tentacles = pose_tentacle_coverage(pose, x, y, z)
    arms = pose_arm_coverage(pose, x, y, z)
    return (surface, tentacles, arms, organs, polar)
end

# Standalone forms used by the tests and kept in one place so they can never
# drift from the fused per-voxel evaluation above.
jelly_tentacle_center(spec::JellySpec, strand::Integer, q::Real, t::Real) =
    pose_tentacle_center(jelly_pose(spec, t), strand, q)

jelly_tentacle_coverage(spec::JellySpec, x::Real, y::Real, z::Real, t::Real) =
    pose_tentacle_coverage(jelly_pose(spec, t), x, y, z)

jelly_arm_center(spec::JellySpec, arm::Integer, q::Real, t::Real) =
    pose_arm_center(jelly_pose(spec, t), arm, q)

jelly_arm_coverage(spec::JellySpec, x::Real, y::Real, z::Real, t::Real) =
    pose_arm_coverage(jelly_pose(spec, t), x, y, z)

"""
World-space center of one gonad lobe, for tests and tuning: the inverse of
the squeezed-frame transform `pose_material` applies.
"""
function jelly_organ_center(spec::JellySpec, organ::Integer, t::Real)
    pose = jelly_pose(spec, t)
    local_center = pose.organ_local[organ + 1]
    return (
        pose.center_x + local_center[1] / pose.squeeze,
        pose.center_y + local_center[2] / pose.squeeze,
        pose.center_z - pose.axis_shift + local_center[3],
    )
end

function jelly_organ_coverage(spec::JellySpec, x::Real, y::Real, z::Real, t::Real)
    pose = jelly_pose(spec, t)
    return pose_organ_coverage(
        pose,
        pose.squeeze * (x - pose.center_x),
        pose.squeeze * (y - pose.center_y),
        z - pose.center_z + pose.axis_shift,
    )
end

function jelly_surface_coverage(case::JellyCase, x::Real, y::Real, z::Real, t::Real)
    coverage = 0.0f0
    for spec in case.jellies
        surface, _, _, _, _ = pose_material(
            jelly_pose(spec, t),
            Float32(x),
            Float32(y),
            Float32(z),
        )
        coverage = max(coverage, surface)
    end
    return coverage
end

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

@inline jelly_finite_vorticity(value::Real) =
    isfinite(value) ? max(Float32(value), 0.0f0) : 0.0f0

"""
Compact isotropic reconstruction filter for the displayed wake. WaterLily's
cell-centred vorticity is physically meaningful at solver resolution, but a
single hot cell magnified across dozens of desktop pixels reads as stipple.
The centre-heavy seven-point kernel attenuates that grid-scale mode while
retaining coherent vortex sheets; anatomy is composited afterwards and stays
analytic and crisp.
"""
@inline function jelly_filtered_vorticity(
    sigma::AbstractArray{<:Real,3},
    x::Integer,
    y::Integer,
    z::Integer,
)
    i = x + 1
    j = y + 1
    k = z + 1
    # Weight before summing. Summing six individually finite Float32 maxima
    # first overflows to Inf and makes the later rational wake knee evaluate
    # Inf/Inf. This convex accumulation stays finite across the complete
    # representable input range while preserving normalized DC gain to
    # Float32 precision.
    filtered = 0.52f0 * jelly_finite_vorticity(sigma[i, j, k])
    filtered += 0.08f0 * jelly_finite_vorticity(sigma[i - 1, j, k])
    filtered += 0.08f0 * jelly_finite_vorticity(sigma[i + 1, j, k])
    filtered += 0.08f0 * jelly_finite_vorticity(sigma[i, j - 1, k])
    filtered += 0.08f0 * jelly_finite_vorticity(sigma[i, j + 1, k])
    filtered += 0.08f0 * jelly_finite_vorticity(sigma[i, j, k - 1])
    filtered += 0.08f0 * jelly_finite_vorticity(sigma[i, j, k + 1])
    return min(filtered, floatmax(Float32))
end

"""
Materialize the animated analytic anatomy and colorize the filtered vorticity
magnitude voxel-by-voxel into the version-2 volume buffer. Emission runs
through the palette's positive half while the apex-to-rim shaded membranes,
rose gonad crowns, blush oral arms, and lavender filaments give the compositor
coherent, distinctly colored surfaces and a stable front-interface normal.
Quiescent water remains transparent; opacity follows a soft rational knee for
the wake and bounded translucent coverage for the body. Wake opacity is capped
below about 0.115, while anatomy feathers continuously from zero to its dense
interior so the compositor can blend medium and tissue lighting smoothly.
"""
function render_volume!(case::JellyCase; palette::Tuple=case_palette(case))
    sigma = download_vorticity_magnitude!(case)
    nx, ny, nz = case.domain
    rgba = case.volume_rgba
    τ = Float32(simulation_time(case))
    poses = [jelly_pose(spec, τ) for spec in case.jellies]
    Threads.@threads :static for y in 1:ny
        # `sigma[x + 1, ...]` is WaterLily's first interior cell, whose
        # physical centre is `loc(0, I) = I - 1.5`.  Sample the analytic
        # tissue at that exact point so the anatomy remains registered with
        # the simulated wake instead of being shifted by one voxel per axis.
        voxel_y = jelly_voxel_center(y)
        @inbounds for row in 1:nz
            z = nz - row + 1
            voxel_z = jelly_voxel_center(z)
            output = jelly_volume_offset(nx, nz, 1, y, z)
            for x in 1:nx
                value = jelly_filtered_vorticity(sigma, x, y, z)
                wake = max(value - JELLY_WAKE_FLOOR, 0.0f0)
                # Soft knee keeps the shell/wake dynamic range on the palette
                # without clipping; the deterministic mapping avoids per-frame
                # autoscale flicker in the marched image.
                wake_density = wake / (wake + 6.0f0)
                voxel_x = jelly_voxel_center(x)
                surface = 0.0f0
                polar = 0.0f0
                tentacles = 0.0f0
                arms = 0.0f0
                organs = 0.0f0
                for pose in poses
                    coverage = pose_material(pose, voxel_x, voxel_y, voxel_z)
                    if coverage[1] > surface
                        surface = coverage[1]
                        polar = coverage[5]
                    end
                    tentacles = max(tentacles, coverage[2])
                    arms = max(arms, coverage[3])
                    organs = max(organs, coverage[4])
                end
                # All materials remain genuinely translucent. A ray crosses
                # many voxels, so modest per-cell absorption is enough to form
                # a legible bell. Bounding the turbulent wake below the dense
                # anatomy range prevents individual high-vorticity cells from
                # becoming opaque pepper-like particles; the analytic anatomy
                # feather itself remains continuous down to transparent.
                density = max(
                    0.15f0 * wake_density,
                    max(
                        0.44f0 * surface,
                        max(
                            0.40f0 * tentacles,
                            max(0.42f0 * arms, 0.46f0 * organs),
                        ),
                    ),
                )
                # Reserve the palette's darkest positive endpoint for the
                # planar plot.  Volumetric color is integrated repeatedly and
                # therefore uses the luminous middle of the green ramp.
                color = palette_color(palette, 0.60f0 * wake_density, 1.0)
                if tentacles > 0.0f0
                    color = blend_color(
                        color,
                        JELLY_FILAMENT_LAVENDER,
                        Float64(0.92f0 * tentacles),
                    )
                end
                if arms > 0.0f0
                    color = blend_color(
                        color,
                        JELLY_ARM_BLUSH,
                        Float64(0.94f0 * arms),
                    )
                end
                if surface > 0.0f0
                    membrane = blend_color(
                        JELLY_RIM_LAVENDER,
                        JELLY_APEX_VIOLET,
                        Float64(jelly_smoothstep(-0.35f0, 0.85f0, polar)),
                    )
                    color = blend_color(color, membrane, Float64(0.95f0 * surface))
                end
                if organs > 0.0f0
                    color = blend_color(
                        color,
                        JELLY_ORGAN_ROSE,
                        Float64(0.95f0 * organs),
                    )
                end
                rgba[output] = color[1]
                rgba[output + 1] = color[2]
                rgba[output + 2] = color[3]
                # Per-voxel opacity for one straight-through voxel crossing;
                # the shader renormalizes it by its actual step length.
                rgba[output + 3] = round(UInt8, 190 * density)
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
