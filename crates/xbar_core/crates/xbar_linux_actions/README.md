# xbar_linux_actions

Small companion adapter for the process-backed `xbar_core` effects used by
JWM bars on Linux. It maps screenshot and audio-control effects to configurable
commands, launches them without blocking the UI loop, and owns child reaping.

Window placement, provider effects, and WM commands deliberately remain with
their respective host adapters.
