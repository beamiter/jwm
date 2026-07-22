const FRAME_MAGIC = UInt8[codeunits("JWMLILY\0")...]
const FRAME_VERSION = UInt32(1)
const FRAME_HEADER_BYTES = 64
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
    stride::Int
    slot_bytes::Int
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
    timestamp_ns::Integer,
)
    buffer = IOBuffer(sizehint=FRAME_HEADER_BYTES)
    write(buffer, FRAME_MAGIC)
    write(buffer, htol(FRAME_VERSION))
    write(buffer, htol(UInt32(FRAME_HEADER_BYTES)))
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
    header = take!(buffer)
    length(header) == FRAME_HEADER_BYTES || error("internal frame header size mismatch")
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

function FramePublisher(path::AbstractString, width::Integer, height::Integer)
    width > 0 || throw(ArgumentError("frame width must be positive"))
    height > 0 || throw(ArgumentError("frame height must be positive"))
    width <= 16_384 || throw(ArgumentError("frame width exceeds protocol limit"))
    height <= 16_384 || throw(ArgumentError("frame height exceeds protocol limit"))
    stride = Base.checked_mul(Int(width), 4)
    slot_bytes = Base.checked_mul(stride, Int(height))
    slot_bytes <= 512 * 1024 * 1024 ||
        throw(ArgumentError("frame exceeds protocol size limit"))
    total_bytes = Base.checked_add(FRAME_HEADER_BYTES, Base.checked_mul(slot_bytes, 2))

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
            # Sequence zero is intentionally unpublished. The compositor
            # connects only after the first complete frame and never consumes
            # this header.
            write(io, frame_header(width, height, stride, 1, 0, 0))
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
        stride,
        slot_bytes,
        UInt32(1),
        UInt64(0),
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
    publisher.closed && error("cannot publish through a closed frame file")
    length(rgba) == publisher.slot_bytes ||
        throw(
            DimensionMismatch(
                "expected $(publisher.slot_bytes) RGBA bytes, received $(length(rgba))",
            ),
        )

    slot = publisher.slot == 0 ? UInt32(1) : UInt32(0)
    sequence = Base.checked_add(publisher.sequence, UInt64(1))
    offset = FRAME_HEADER_BYTES + Int(slot) * publisher.slot_bytes

    lock_file(publisher.io)
    try
        seek(publisher.io, offset)
        write(publisher.io, rgba)
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
                timestamp_ns,
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
