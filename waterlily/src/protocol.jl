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
    # Bytes written per double-buffer slot (color, or color+material).
    slot_bytes::Int
    material_aux::Bool
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
)
    volumetric = depth > 1
    version = if !volumetric
        FRAME_VERSION
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
        # Reserved: material_aux flag then padding. Version alone is enough
        # for parsers; the flag documents the doubled slot for tools.
        write(buffer, htol(UInt32(material_aux ? 1 : 0)))
        write(buffer, zeros(UInt8, FRAME_VOLUME_HEADER_BYTES - FRAME_HEADER_BYTES - 8))
    end
    header = take!(buffer)
    length(header) == header_bytes || error("internal frame header size mismatch")
    return header
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
volume. `start_sequence` seeds the publication counter so a worker
replacing its frame file mid-session (for example on a case switch that
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
    stride = Base.checked_mul(Int(width), 4)
    header_bytes = depth > 1 ? FRAME_VOLUME_HEADER_BYTES : FRAME_HEADER_BYTES
    color_bytes =
        Base.checked_mul(Base.checked_mul(stride, Int(height)), Int(depth))
    color_bytes <= 512 * 1024 * 1024 ||
        throw(ArgumentError("frame exceeds protocol size limit"))
    slot_bytes = material_aux ? Base.checked_mul(color_bytes, 2) : color_bytes
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
    return _publish_slot!(publisher, rgba, nothing, timestamp_ns)
end

function publish!(
    publisher::FramePublisher,
    rgba::AbstractVector{UInt8},
    material::AbstractVector{UInt8},
    timestamp_ns::Integer=time_ns(),
)
    publisher.material_aux ||
        throw(ArgumentError("material plane requires a version-3 publisher"))
    return _publish_slot!(publisher, rgba, material, timestamp_ns)
end

function _publish_slot!(
    publisher::FramePublisher,
    rgba::AbstractVector{UInt8},
    material::Union{Nothing,AbstractVector{UInt8}},
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
                timestamp_ns;
                depth=publisher.depth,
                material_aux=publisher.material_aux,
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
end

WakeClient(path::AbstractString) = WakeClient(String(path), nothing, Channel{String}(16))

function disconnect!(client::WakeClient)
    if client.stream !== nothing
        try
            close(client.stream)
        catch
        end
        client.stream = nothing
    end
end

"""
The wake socket is bidirectional: the worker writes one-byte frame wakeups
while the compositor writes newline-terminated control commands (for example
`case dance`). A background task drains the read side into `commands` so the
publish loop can poll without blocking.
"""
function start_command_reader!(client::WakeClient)
    stream = client.stream
    stream === nothing && return nothing
    @async try
        while true
            line = readline(stream)
            command = strip(line)
            if isempty(command)
                eof(stream) && break
                continue
            end
            put!(client.commands, String(command))
        end
    catch
        # A dropped consumer stream simply stops command delivery until the
        # next reconnect creates a fresh reader.
    end
    return nothing
end

function take_command!(client::WakeClient)
    isready(client.commands) || return nothing
    return take!(client.commands)
end

function notify!(client::WakeClient)
    for _attempt in 1:2
        if client.stream === nothing
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
