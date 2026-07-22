module JwmWaterLily

using Sockets
using StaticArrays
using WaterLily

include("protocol.jl")
include("cases/common.jl")
include("cases/hover.jl")
include("cases/cylinder.jl")
include("cases/dance.jl")
include("cases/flap.jl")
include("cases/tandem.jl")
include("cases/diamond.jl")
include("cases/orbit.jl")
include("cases/wander.jl")
include("cases/registry.jl")

const DEFAULT_FPS = 30.0
const DEFAULT_SIZE = (1980, 1080)
const MIN_SIM_DIMENSION = 64
const MAX_SIM_DIMENSION = 4096
const MULTIGRID_QUANTUM = 16

struct RunnerOptions
    case_name::String
    device::Symbol
    fps::Float64
    socket_path::String
    frame_path::String
    requested_size::Tuple{Int,Int}
    simulation_size::Tuple{Int,Int}
end

struct SelectedBackend
    name::Symbol
    memory::Any
end

function runtime_directory()
    configured = get(ENV, "XDG_RUNTIME_DIR", "")
    return isempty(configured) ? "/tmp/jwm-$(Base.Libc.getuid())" : configured
end

default_socket_path() = get(
    ENV,
    "JWM_WATERLILY_SOCKET",
    joinpath(runtime_directory(), "jwm-waterlily.sock"),
)
default_frame_path() = get(
    ENV,
    "JWM_WATERLILY_FRAME_FILE",
    joinpath(runtime_directory(), "jwm-waterlily.frame"),
)

function usage(io::IO=stdout)
    cases = join(available_cases(), ", ")
    print(
        io,
        """
        usage: julia --project=waterlily waterlily/runner.jl [options]

          --case NAME          registered case ($cases; default: wander)
          --device DEVICE      auto, cpu, cuda, or rocm (default: auto)
          --fps FPS            publication rate, 1..240 (default: 30)
          --socket PATH        compositor wakeup Unix socket
          --frame-file PATH    private double-buffer frame file
          --sim-size SIZE      N or WxH; normalized to multiples of 16
          --help               show this help
        """,
    )
end

function option_pairs(args::AbstractVector{<:AbstractString})
    pairs = Pair{String,String}[]
    index = 1
    while index <= length(args)
        argument = String(args[index])
        argument in ("-h", "--help") && return nothing
        startswith(argument, "--") ||
            throw(ArgumentError("unexpected positional argument: $argument"))

        if occursin('=', argument)
            key, value = split(argument, '='; limit=2)
            isempty(value) && throw(ArgumentError("$key requires a value"))
            push!(pairs, key => value)
        else
            index == length(args) && throw(ArgumentError("$argument requires a value"))
            value = String(args[index + 1])
            startswith(value, "--") &&
                throw(ArgumentError("$argument requires a value"))
            push!(pairs, argument => value)
            index += 1
        end
        index += 1
    end
    return pairs
end

function parse_size(value::AbstractString)
    text = lowercase(strip(value))
    parts = split(text, 'x')
    if length(parts) == 1
        side = tryparse(Int, parts[1])
        side === nothing && throw(ArgumentError("invalid --sim-size: $value"))
        requested = (side, side)
    elseif length(parts) == 2
        width, height = tryparse.(Int, parts)
        (width === nothing || height === nothing) &&
            throw(ArgumentError("invalid --sim-size: $value"))
        requested = (width, height)
    else
        throw(ArgumentError("invalid --sim-size: $value"))
    end

    all(dimension -> MIN_SIM_DIMENSION <= dimension <= MAX_SIM_DIMENSION, requested) ||
        throw(
            ArgumentError(
                "--sim-size dimensions must be between $MIN_SIM_DIMENSION and $MAX_SIM_DIMENSION",
            ),
        )
    return requested
end

function normalize_dimension(value::Int)
    lower = fld(value, MULTIGRID_QUANTUM) * MULTIGRID_QUANTUM
    upper = cld(value, MULTIGRID_QUANTUM) * MULTIGRID_QUANTUM
    lower = max(lower, MIN_SIM_DIMENSION)
    upper = min(upper, MAX_SIM_DIMENSION)
    return value - lower < upper - value ? lower : upper
end

normalize_size(size::Tuple{Int,Int}) = normalize_dimension.(size)

function parse_cli(args::AbstractVector{<:AbstractString})
    parsed = option_pairs(args)
    parsed === nothing && return nothing

    values = Dict(
        "--case" => "wander",
        "--device" => "auto",
        "--fps" => string(DEFAULT_FPS),
        "--socket" => default_socket_path(),
        "--frame-file" => default_frame_path(),
        "--sim-size" => "$(DEFAULT_SIZE[1])x$(DEFAULT_SIZE[2])",
    )
    for (key, value) in parsed
        haskey(values, key) || throw(ArgumentError("unknown option: $key"))
        values[key] = value
    end

    case_name = values["--case"]
    case_name in available_cases() ||
        throw(
            ArgumentError(
                "unknown case '$case_name'; available cases: $(join(available_cases(), ", "))",
            ),
        )

    device_text = lowercase(values["--device"])
    device_text in ("auto", "cpu", "cuda", "rocm") ||
        throw(ArgumentError("--device must be auto, cpu, cuda, or rocm"))
    device = Symbol(device_text)

    fps = tryparse(Float64, values["--fps"])
    (fps === nothing || !isfinite(fps) || !(1.0 <= fps <= 240.0)) &&
        throw(ArgumentError("--fps must be a finite number between 1 and 240"))

    socket_path = values["--socket"]
    frame_path = values["--frame-file"]
    isempty(socket_path) && throw(ArgumentError("--socket must not be empty"))
    isempty(frame_path) && throw(ArgumentError("--frame-file must not be empty"))
    socket_path == frame_path &&
        throw(ArgumentError("--socket and --frame-file must be different paths"))

    requested_size = parse_size(values["--sim-size"])
    simulation_size = normalize_size(requested_size)
    return RunnerOptions(
        case_name,
        device,
        fps,
        socket_path,
        frame_path,
        requested_size,
        simulation_size,
    )
end

const CUDA_ID = Base.PkgId(Base.UUID("052768ef-5323-5732-b1bb-66c8b64840ba"), "CUDA")
const AMDGPU_ID =
    Base.PkgId(Base.UUID("21141c5a-9bdb-4563-92ae-f87d6854732e"), "AMDGPU")

function probe_source(device::Symbol)
    if device == :cuda
        return "import CUDA; exit(CUDA.functional() ? 0 : 1)"
    elseif device == :rocm
        return "import AMDGPU; exit(AMDGPU.functional() ? 0 : 1)"
    end
    throw(ArgumentError("cannot probe device $device"))
end

"""
Probe a GPU in a disposable Julia process. In particular, `auto` never imports
an optional GPU runtime into the long-lived worker until a probe has succeeded.
"""
function probe_backend(device::Symbol; timeout_seconds::Float64=30.0)
    active_project = Base.active_project()
    active_project === nothing && error("the WaterLily worker requires an active Julia project")
    project = dirname(active_project)
    command = `$(Base.julia_cmd()) --startup-file=no --history-file=no --compiled-modules=existing --pkgimages=existing --project=$project -e $(probe_source(device))`
    process = try
        run(pipeline(command; stdout=devnull, stderr=devnull); wait=false)
    catch error
        @debug "could not launch GPU probe" device exception = (error, catch_backtrace())
        return false
    end

    try
        deadline = time() + timeout_seconds
        while process_running(process) && time() < deadline
            sleep(0.05)
        end
        if process_running(process)
            # A package import may be in native compilation and can defer
            # SIGTERM indefinitely. The probe is disposable, so enforce the
            # advertised timeout with SIGKILL.
            kill(process, Base.SIGKILL)
            wait(process)
            return false
        end
        return success(process)
    catch error
        process_running(process) && kill(process, Base.SIGKILL)
        @debug "GPU probe failed" device exception = (error, catch_backtrace())
        return false
    end
end

function load_backend(device::Symbol)
    if device == :cuda
        cuda = Base.require(CUDA_ID)
        Base.invokelatest(cuda.functional) || error("CUDA loaded but is not functional")
        return SelectedBackend(:cuda, cuda.CuArray)
    elseif device == :rocm
        amdgpu = Base.require(AMDGPU_ID)
        Base.invokelatest(amdgpu.functional) || error("AMDGPU loaded but is not functional")
        return SelectedBackend(:rocm, amdgpu.ROCArray)
    elseif device == :cpu
        return SelectedBackend(:cpu, Array)
    end
    throw(ArgumentError("unsupported device $device"))
end

function select_backend(requested::Symbol)
    requested == :cpu && return load_backend(:cpu)

    if requested in (:cuda, :rocm)
        probe_backend(requested) ||
            error("requested $(requested) backend is unavailable or not functional")
        return load_backend(requested)
    end

    for candidate in (:cuda, :rocm)
        probe_backend(candidate) || continue
        try
            return load_backend(candidate)
        catch error
            @warn(
                "GPU backend passed its probe but failed to load; trying another backend",
                candidate,
                exception=(error, catch_backtrace()),
            )
        end
    end
    return load_backend(:cpu)
end

function prepare_runtime_parent(path::AbstractString)
    parent = dirname(abspath(path))
    if !isdir(parent)
        mkpath(parent; mode=0o700)
    end
    metadata = stat(parent)
    owned_by_user =
        metadata.uid == Base.Libc.getuid() && (metadata.mode & 0o022) == 0
    sticky_shared_directory = (metadata.mode & 0o1000) != 0
    (owned_by_user || sticky_shared_directory) ||
        throw(
            ArgumentError(
                "runtime directory must be private or a sticky shared directory: $parent",
            ),
        )
    return parent
end

"""
Resolve a compositor control command against the case registry. `case NAME`
selects a registered case, `case next` cycles through the sorted registry.
Unknown commands and unknown case names are logged and ignored so a newer
compositor can never crash an older worker.
"""
function resolve_case_command(command::AbstractString, current_case::AbstractString)
    parts = split(command)
    if length(parts) != 2 || parts[1] != "case"
        @warn "ignoring unknown WaterLily command" command
        return nothing
    end
    name = String(parts[2])
    cases = available_cases()
    if name == "next"
        index = findfirst(==(String(current_case)), cases)
        return index === nothing ? first(cases) : cases[mod1(index + 1, length(cases))]
    end
    if !(name in cases)
        @warn "ignoring unknown WaterLily case" requested = name available = cases
        return nothing
    end
    return name
end

function run_worker_with_backend(options::RunnerOptions, backend::SelectedBackend)
    options.requested_size != options.simulation_size &&
        @info "normalized simulation size for WaterLily multigrid" requested =
            options.requested_size simulation = options.simulation_size
    @info "starting WaterLily worker" case = options.case_name device = backend.name size =
        options.simulation_size fps = options.fps

    simulation_case =
        build_case(options.case_name, options.simulation_size; memory=backend.memory)
    publisher = FramePublisher(options.frame_path, options.simulation_size...)
    wakeups = WakeClient(options.socket_path)
    cleaned = Ref(false)

    function cleanup()
        cleaned[] && return
        cleaned[] = true
        close(wakeups)
        close(publisher)
        remove_owned_frame_file!(publisher)
    end
    atexit(cleanup)

    frame_period = 1.0 / options.fps
    current_case = options.case_name
    scratch = RenderScratch(options.simulation_size)
    # Leave a slice of the budget for publish pacing; the solver checks the
    # deadline only between substeps, so it can overshoot by one substep and
    # the pacing sleep below absorbs that overshoot.
    advance_budget_ns = UInt64(round(0.85 * frame_period * 1.0e9))
    frame_period_ns = UInt64(round(frame_period * 1.0e9))
    dilation_window = 0.0
    dilation_frames = 0
    next_frame_ns = time_ns() + frame_period_ns
    try
        while true
            started = time_ns()
            while (command = take_command!(wakeups)) !== nothing
                requested = resolve_case_command(command, current_case)
                (requested === nothing || requested == current_case) && continue
                try
                    simulation_case = build_case(
                        requested,
                        options.simulation_size;
                        memory=backend.memory,
                    )
                    current_case = requested
                    @info "switched WaterLily case" case = requested
                catch error
                    @warn(
                        "could not switch WaterLily case; keeping the current one",
                        requested,
                        exception=(error, catch_backtrace()),
                    )
                end
            end

            # Publish first, then advance: the frame reaches the compositor a
            # few milliseconds into the period, and the solver spends the rest
            # of the budget stepping toward the next state. Overlapping the
            # solver in a separate task measured slower — the threaded
            # colorize loop starves the task issuing device kernels.
            pose_time = simulation_time(simulation_case)
            compute_vorticity!(scratch, simulation_case)
            rgba = render_rgba!(scratch, simulation_case, pose_time)
            publish!(publisher, rgba, time_ns())
            notify!(wakeups)
            achieved_step = advance_budgeted!(
                simulation_case,
                frame_period,
                started + advance_budget_ns,
            )

            # Report sustained slow motion about once every ten seconds
            # rather than spamming per frame.
            dilation_window += min(Float64(achieved_step) / frame_period, 1.0)
            dilation_frames += 1
            if dilation_frames >= round(Int, 10 * options.fps)
                speed = dilation_window / dilation_frames
                speed < 0.9 && @info(
                    "solver below real time; simulation running in slow motion",
                    speed = round(speed; digits=2),
                    hint = "reduce --sim-size for real-time playback",
                )
                dilation_window = 0.0
                dilation_frames = 0
            end

            # Guarantee a scheduler point per frame so the command reader task
            # runs even when the solver saturates the frame budget.
            yield()
            # Pace against an absolute schedule: sleep() habitually oversleeps
            # by a millisecond or two, and a per-frame relative sleep would
            # accumulate that jitter into a visibly lower publish rate.
            now = time_ns()
            if now < next_frame_ns
                sleep(Float64(next_frame_ns - now) / 1.0e9)
                next_frame_ns += frame_period_ns
            else
                # Behind schedule: restart the cadence rather than bursting.
                next_frame_ns = time_ns() + frame_period_ns
            end
        end
    catch error
        error isa InterruptException || rethrow()
    finally
        cleanup()
    end
    return nothing
end

function run_worker(options::RunnerOptions)
    prepare_runtime_parent(options.frame_path)
    backend = select_backend(options.device)

    # Optional GPU packages are loaded dynamically. Enter the newest world age
    # before constructing their arrays or invoking methods they just defined.
    return Base.invokelatest(run_worker_with_backend, options, backend)
end

function main(args::AbstractVector{<:AbstractString}=ARGS)
    try
        options = parse_cli(args)
        if options === nothing
            usage()
            return 0
        end
        Base.exit_on_sigint(false)
        run_worker(options)
        return 0
    catch error
        if error isa ArgumentError
            println(stderr, "waterlily: ", sprint(showerror, error))
            usage(stderr)
            return 2
        end
        showerror(stderr, error, catch_backtrace())
        println(stderr)
        return 1
    end
end

export FramePublisher,
    RunnerOptions,
    available_cases,
    build_case,
    main,
    normalize_size,
    notify!,
    parse_cli,
    publish!,
    render_rgba,
    select_backend

end
