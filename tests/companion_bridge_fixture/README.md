# Deterministic bridge fixture

Run `cargo test --manifest-path tests/companion_bridge_fixture/Cargo.toml` from JWM.
This small independent workspace compiles the exact production voice observer,
desktop observer, scene worker and bitmap font sources. Playback history,
notifier and external-command dependencies are explicitly stubbed; tests never
open microphone/speaker devices, connect a live coordinator, call a model or
render a native display. Passing tests are state/raster evidence only, not
socket, hardware, GPU, lock-screen or retirement-fence validation.
