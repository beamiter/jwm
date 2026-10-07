const FRAME_MAGIC = UInt8[codeunits("JWMLILY\0")...]
const FRAME_VERSION = UInt32(1)
# Version 2 publishes a volume: `depth` two-dimensional slices per slot,
# stacked front (nearest the viewer) to back. Its header keeps the version-1
# prefix byte-for-byte and appends the depth plus reserved space.
const FRAME_VERSION_VOLUMETRIC = UInt32(2)
# Version 3 keeps the version-2 header layout and doubles each slot so a
# tightly packed RGBA8 material plane (octahedral normal + thickness) sits
# immediately behind the color volume. Older compositors that only speak
# version 2 continue to work with producers that omit the material plane.
const FRAME_VERSION_VOLUME_MATERIAL = UInt32(3)
# Version 4 is negotiated on the wake socket; legacy peers continue to receive
# version 3. Each slot appends <=5 fixed-size analytic poses behind both planes.
const FRAME_VERSION_JELLY_POSE = UInt32(4)
const MAX_JELLY_POSES = 5
const JELLY_POSE_FLOATS = 8
const JELLY_POSE_BYTES = 32
const JELLY_POSE_KIND = UInt32(1)
const MAX_WAKE_COMMAND_BYTES = 256
const FRAME_HEADER_BYTES = 64
const FRAME_VOLUME_HEADER_BYTES = 96
const PIXEL_FORMAT_RGBA8 = UInt32(1)
const COLOR_SPACE_SRGB = UInt32(1)
const ALPHA_MODE_OPAQUE = UInt32(1)
const ORIGIN_TOP_LEFT = UInt32(1)
const LOCK_EX = Cint(2)
const LOCK_UN = Cint(8)

mutable struct FramePublisher
    path::String
    io::Base.Filesystem.File
    width::Int
    height::Int
    depth::Int
    stride::Int
    header_bytes::Int
    # Bytes of one color volume (and of one material plane when present).
    color_bytes::Int
    # Bytes per slot: color, optional material, then optional jelly poses.
    slot_bytes::Int
    material_aux::Bool
    jelly_count::Int
    jelly_detail::Int
    slot::UInt32
    sequence::UInt64
    device::UInt64
    inode::UInt64
    closed::Bool
end

function frame_header(
    width::Integer,
    height::Integer,
    stride::Integer,
    slot::Integer,
    sequence::Integer,
    timestamp_ns::Integer;
    depth::Integer=1,
    material_aux::Bool=false,
    jelly_count::Integer=0,
    jelly_detail::Integer=0,
)
    validate_jelly_layout(depth, material_aux, jelly_count, jelly_detail)
    volumetric = depth > 1
    version = if !volumetric
        FRAME_VERSION
    elseif jelly_count > 0
        FRAME_VERSION_JELLY_POSE
    elseif material_aux
        FRAME_VERSION_VOLUME_MATERIAL
    else
        FRAME_VERSION_VOLUMETRIC
    end
    header_bytes = volumetric ? FRAME_VOLUME_HEADER_BYTES : FRAME_HEADER_BYTES
    buffer = IOBuffer(sizehint=header_bytes)
    write(buffer, FRAME_MAGIC)
    write(buffer, htol(version))
    write(buffer, htol(UInt32(header_bytes)))
    write(buffer, htol(UInt32(width)))
    write(buffer, htol(UInt32(height)))
    write(buffer, htol(UInt32(stride)))
    write(buffer, htol(PIXEL_FORMAT_RGBA8))
    write(buffer, htol(COLOR_SPACE_SRGB))
    write(buffer, htol(ALPHA_MODE_OPAQUE))
    write(buffer, htol(ORIGIN_TOP_LEFT))
    write(buffer, htol(UInt32(slot)))
    write(buffer, htol(UInt64(sequence)))
    write(buffer, htol(UInt64(timestamp_ns)))
    if volumetric
        write(buffer, htol(UInt32(depth)))
        write(buffer, htol(UInt32(material_aux ? 1 : 0)))
        write(buffer, htol(UInt32(jelly_count)))
        write(buffer, htol(UInt32(jelly_count > 0 ? JELLY_POSE_BYTES : 0)))
        write(buffer, htol(jelly_count > 0 ? JELLY_POSE_KIND : UInt32(0)))
        write(buffer, htol(UInt32(jelly_detail)))
        write(buffer, zeros(UInt8, 8))
    end
    header = take!(buffer)
    length(header) == header_bytes || error("internal frame header size mismatch")
    return header
end

function validate_jelly_layout(depth, material_aux, jelly_count, jelly_detail)
    0 <= jelly_count <= MAX_JELLY_POSES ||
        throw(ArgumentError("jelly pose count must be between 0 and $MAX_JELLY_POSES"))
    if jelly_count > 0
        depth > 1 && material_aux ||
            throw(ArgumentError("jelly poses require a volumetric material frame"))
        jelly_detail in (1, 2) ||
            throw(ArgumentError("jelly pose detail must be 1 or 2"))
    else
        jelly_detail == 0 || throw(ArgumentError("jelly detail requires jelly poses"))
    end
    return nothing
end

function jelly_pose_bytes(poses::AbstractVector{Float32}, expected_count::Integer)
    length(poses) == expected_count * JELLY_POSE_FLOATS ||
        throw(DimensionMismatch("jelly pose count does not match the publisher"))
    for offset in 0:JELLY_POSE_FLOATS:(length(poses) - 1)
        values = @view poses[offset + 1:offset + JELLY_POSE_FLOATS]
        all(isfinite, values) &&
            all(value -> abs(value) <= 0.5f0, @view(values[1:3])) &&
            0.0f0 < values[4] <= 0.25f0 &&
            0.8f0 <= values[5] <= 1.2f0 &&
            abs(values[6]) <= Float32(2pi) &&
            abs(values[7]) <= 0.25f0 &&
            abs(values[8]) <= 0.5f0 ||
            throw(ArgumentError("jelly pose is non-finite or outside the protocol bounds"))
    end
    # Explicit endian conversion keeps the wire independent of host byte order.
    return reinterpret(UInt8, htol.(reinterpret(UInt32, collect(poses))))
end

function lock_file(io::Base.Filesystem.File)
    while true
        result = ccall(:flock, Cint, (Cint, Cint), Base.fd(io), LOCK_EX)
        result == 0 && return
        Base.Libc.errno() == Base.Libc.EINTR || systemerror("flock", true)
    end
end

function unlock_file(io::Base.Filesystem.File)
    ccall(:flock, Cint, (Cint, Cint), Base.fd(io), LOCK_UN) == 0 ||
        systemerror("flock unlock", true)
end

function truncate_file(io::Base.Filesystem.File, size::Integer)
    size >= 0 || throw(ArgumentError("file size must not be negative"))
    ccall(:ftruncate, Cint, (Cint, Int64), Base.fd(io), Int64(size)) == 0 ||
        systemerror("ftruncate", true)
end

function flush_file(io::Base.Filesystem.File)
    flush(io)
end

function atomic_replace(source::AbstractString, destination::AbstractString)
    ccall(:rename, Cint, (Cstring, Cstring), source, destination) == 0 ||
        systemerror("rename", true)
end

"""
Create a double-buffered frame file. `depth == 1` publishes the classic
planar version-1 contract; `depth > 1` publishes a volumetric slot. Pass
`material_aux=true` (the worker default for native volumes) to select
version 3, whose slots pack an RGBA8 material plane behind the color
volume. After negotiating `jelly-pose-v1`, pass `jelly_count=1:5` and
`jelly_detail=1:2` to append analytic poses using version 4. Defaults preserve
the existing version-1/2/3 wire format. `start_sequence` seeds the publication
counter so a worker replacing its frame file mid-session (for example on a case switch that
changes the frame geometry) keeps the consumer's monotonic-sequence view
intact.
"""
function FramePublisher(
    path::AbstractString,
    width::Integer,
    height::Integer;
    depth::Integer=1,
    start_sequence::Integer=0,
    material_aux::Bool=false,
    jelly_count::Integer=0,
    jelly_detail::Integer=0,
)
    width > 0 || throw(ArgumentError("frame width must be positive"))
    height > 0 || throw(ArgumentError("frame height must be positive"))
    depth > 0 || throw(ArgumentError("frame depth must be positive"))
    width <= 16_384 || throw(ArgumentError("frame width exceeds protocol limit"))
    height <= 16_384 || throw(ArgumentError("frame height exceeds protocol limit"))
    depth <= 16_384 || throw(ArgumentError("frame depth exceeds protocol limit"))
    start_sequence >= 0 ||
        throw(ArgumentError("start sequence must not be negative"))
    material_aux && depth == 1 &&
        throw(ArgumentError("material aux requires a volumetric frame"))
    validate_jelly_layout(depth, material_aux, jelly_count, jelly_detail)
    stride = Base.checked_mul(Int(width), 4)
    header_bytes = depth > 1 ? FRAME_VOLUME_HEADER_BYTES : FRAME_HEADER_BYTES
    color_bytes =
        Base.checked_mul(Base.checked_mul(stride, Int(height)), Int(depth))
    color_bytes <= 512 * 1024 * 1024 ||
        throw(ArgumentError("frame exceeds protocol size limit"))
    slot_bytes = material_aux ? Base.checked_mul(color_bytes, 2) : color_bytes
    slot_bytes = Base.checked_add(slot_bytes, Int(jelly_count) * JELLY_POSE_BYTES)
    if jelly_count > 0
        slot_bytes <= 512 * 1024 * 1024 ||
            throw(ArgumentError("frame slot exceeds protocol size limit"))
        color_bytes <= 64 * 1024 * 1024 ||
            throw(ArgumentError("volume exceeds compositor size limit"))
    end
    total_bytes = Base.checked_add(header_bytes, Base.checked_mul(slot_bytes, 2))

    final_path = abspath(String(path))
    temporary_path = tempname(dirname(final_path); cleanup=false)
    flags =
        Base.Filesystem.JL_O_RDWR |
        Base.Filesystem.JL_O_CREAT |
        Base.Filesystem.JL_O_EXCL |
        Base.Filesystem.JL_O_CLOEXEC |
        Base.Filesystem.JL_O_NOFOLLOW
    io = Base.Filesystem.open(temporary_path, flags, 0o600)
    try
        chmod(temporary_path, 0o600)
        truncate_file(io, total_bytes)
        lock_file(io)
        try
            seekstart(io)
            # A fresh worker seeds sequence zero, which is intentionally
            # unpublished: the compositor connects only after the first
            # complete frame. A replacement publisher seeds the previous
            # publisher's sequence, which an up-to-date consumer has already
            # taken and therefore also never consumes.
            write(
                io,
                frame_header(
                    width,
                    height,
                    stride,
                    1,
                    start_sequence,
                    0;
                    depth,
                    material_aux,
                    jelly_count,
                    jelly_detail,
                ),
            )
            flush_file(io)
            atomic_replace(temporary_path, final_path)
        finally
            unlock_file(io)
        end
    catch
        close(io)
        ispath(temporary_path) && rm(temporary_path; force=true)
        rethrow()
    end
    identity = stat(io)
    return FramePublisher(
        final_path,
        io,
        Int(width),
        Int(height),
        Int(depth),
        stride,
        header_bytes,
        color_bytes,
        slot_bytes,
        material_aux,
        Int(jelly_count),
        Int(jelly_detail),
        UInt32(1),
        UInt64(start_sequence),
        UInt64(identity.device),
        UInt64(identity.inode),
        false,
    )
end

function publish!(
    publisher::FramePublisher,
    rgba::AbstractVector{UInt8},
    timestamp_ns::Integer=time_ns(),
)
    if publisher.material_aux
        throw(
            ArgumentError(
                "version-3 volumes require publish!(publisher, rgba, material, timestamp)",
            ),
        )
    end
    return _publish_slot!(publisher, rgba, nothing, Float32[], timestamp_ns)
end

function publish!(
    publisher::FramePublisher,
    rgba::AbstractVector{UInt8},
    material::AbstractVector{UInt8},
    timestamp_ns::Integer=time_ns(),
)
    publisher.material_aux ||
        throw(ArgumentError("material plane requires a version-3 publisher"))
    publisher.jelly_count == 0 ||
        throw(ArgumentError("version-4 publishers require jelly poses"))
    return _publish_slot!(publisher, rgba, material, Float32[], timestamp_ns)
end

function publish!(
    publisher::FramePublisher,
    rgba::AbstractVector{UInt8},
    material::AbstractVector{UInt8},
    poses::AbstractVector{Float32},
    timestamp_ns::Integer=time_ns(),
)
    publisher.jelly_count > 0 ||
        throw(ArgumentError("jelly poses require a version-4 publisher"))
    return _publish_slot!(publisher, rgba, material, poses, timestamp_ns)
end

function _publish_slot!(
    publisher::FramePublisher,
    rgba::AbstractVector{UInt8},
    material::Union{Nothing,AbstractVector{UInt8}},
    poses::AbstractVector{Float32},
    timestamp_ns::Integer,
)
    publisher.closed && error("cannot publish through a closed frame file")
    length(rgba) == publisher.color_bytes ||
        throw(
            DimensionMismatch(
                "expected $(publisher.color_bytes) RGBA bytes, received $(length(rgba))",
            ),
        )
    if material !== nothing
        length(material) == publisher.color_bytes ||
            throw(
                DimensionMismatch(
                    "expected $(publisher.color_bytes) material bytes, received $(length(material))",
                ),
            )
    end

    pose_bytes = jelly_pose_bytes(poses, publisher.jelly_count)
    timestamp_ns >= 0 || throw(ArgumentError("timestamp must not be negative"))
    # Convert before any slot writes, so all input validation is transactional.
    timestamp = UInt64(timestamp_ns)
    slot = publisher.slot == 0 ? UInt32(1) : UInt32(0)
    sequence = Base.checked_add(publisher.sequence, UInt64(1))
    offset = publisher.header_bytes + Int(slot) * publisher.slot_bytes

    lock_file(publisher.io)
    try
        seek(publisher.io, offset)
        write(publisher.io, rgba)
        if material !== nothing
            write(publisher.io, material)
        end
        write(publisher.io, pose_bytes)
        flush_file(publisher.io)

        seekstart(publisher.io)
        write(
            publisher.io,
            frame_header(
                publisher.width,
                publisher.height,
                publisher.stride,
                slot,
                sequence,
                timestamp;
                depth=publisher.depth,
                material_aux=publisher.material_aux,
                jelly_count=publisher.jelly_count,
                jelly_detail=publisher.jelly_detail,
            ),
        )
        flush_file(publisher.io)
        publisher.slot = slot
        publisher.sequence = sequence
    finally
        unlock_file(publisher.io)
    end
    return sequence
end

function remove_owned_frame_file!(publisher::FramePublisher)
    identity = try
        lstat(publisher.path)
    catch
        return false
    end
    if UInt64(identity.device) == publisher.device && UInt64(identity.inode) == publisher.inode
        rm(publisher.path; force=true)
        return true
    end
    return false
end

function Base.close(publisher::FramePublisher)
    publisher.closed && return
    publisher.closed = true
    close(publisher.io)
end

mutable struct WakeClient
    path::String
    stream::Union{Nothing,Base.PipeEndpoint}
    commands::Channel{String}
    jelly_pose_supported::Bool
end

WakeClient(path::AbstractString) = WakeClient(String(path), nothing, Channel{String}(16), false)

function disconnect!(client::WakeClient)
    stream = client.stream
    client.stream = nothing
    client.jelly_pose_supported = false
    # A reconnect must not replay a capability or command from the old peer.
    # Replacing and closing the channel also wakes a reader blocked on put!.
    commands = client.commands
    client.commands = Channel{String}(16)
    close(commands)
    if stream !== nothing
        try
            close(stream)
        catch
        end
    end
    return nothing
end

"""Consume a capability advertisement without exposing it as a case command."""
function apply_capabilities!(client::WakeClient, command::AbstractString)
    parts = split(command)
    (isempty(parts) || parts[1] != "capabilities") && return false
    client.jelly_pose_supported = "jelly-pose-v1" in @view(parts[2:end])
    return true
end

"""Read one bounded command, discarding an incomplete final line at EOF."""
function read_wake_command(stream::IO)
    buffer = UInt8[]
    while !eof(stream)
        byte = read(stream, UInt8)
        byte == UInt8('\n') && return String(strip(String(buffer)))
        length(buffer) < MAX_WAKE_COMMAND_BYTES ||
            throw(ArgumentError("WaterLily command exceeds size limit"))
        push!(buffer, byte)
    end
    return nothing
end

"""
The wake socket is bidirectional: one-byte worker wakeups and newline-terminated
compositor commands. Capability state belongs to this connection only. Bound
both a command and the channel before accepting externally supplied text.
"""
function start_command_reader!(client::WakeClient)
    stream = client.stream
    stream === nothing && return nothing
    commands = client.commands
    @async try
        while (command = read_wake_command(stream)) !== nothing
            client.stream === stream || break
            isempty(command) && continue
            apply_capabilities!(client, command) && continue
            put!(commands, command)
        end
    catch
        # A dropped consumer or overlong command restarts negotiation.
    finally
        client.stream === stream && disconnect!(client)
    end
    return nothing
end

function take_command!(client::WakeClient)
    isready(client.commands) || return nothing
    return take!(client.commands)
end

function notify!(client::WakeClient; allow_reconnect::Bool=true)
    for _attempt in 1:2
        if client.stream === nothing
            # A v4 frame was prepared for the previous peer. Never notify a
            # newly connected legacy consumer until a fallback frame exists.
            allow_reconnect || return false
            client.jelly_pose_supported = false
            try
                client.stream = Sockets.connect(client.path)
            catch
                return false
            end
            start_command_reader!(client)
        end

        try
            write(client.stream, UInt8(1))
            flush(client.stream)
            return true
        catch
            disconnect!(client)
        end
    end
    return false
end

Base.close(client::WakeClient) = disconnect!(client)
