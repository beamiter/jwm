# Local Silero distribution

Eva embeds the unmodified `silero_vad.onnx` from the upstream **v5.1.2** tag.
The model's MIT license is included alongside the bytes. No model, audio or
transcript is fetched or uploaded by inference, construction or reset.

- Model: https://github.com/snakers4/silero-vad/blob/v5.1.2/src/silero_vad/data/silero_vad.onnx
- Streaming contract: https://github.com/snakers4/silero-vad/blob/v5.1.2/src/silero_vad/utils_vad.py
- License: https://github.com/snakers4/silero-vad/blob/v5.1.2/LICENSE
- Model SHA-256: `2623a2953f6ff3d2c1e61740c6cdb7168133479b267dfef114a4a3cc5bdd788f`

Only regular model files are accepted; opening a FIFO/device cannot block initialization.
The exact size and SHA-256 are checked before loading. An optional absolute
`audio.silero_model_path` must contain this same model, not an arbitrary ONNX
graph. Replacing a model requires reviewing the streaming contract, license,
checksum and acoustic tests. Pinning this established version does not imply
that it is the newest upstream release.

## Native runtime

Default builds enable the `silero` Cargo feature. They use `ort` **2.0.0-rc.10**
(MIT/Apache-2.0) and the local CPU ONNX Runtime C API **22** (1.22 or compatible
later). Cargo does not download or link a native runtime. No GPU provider is
enabled. Inference uses one intra-op and one inter-op thread; ORT telemetry is
disabled. The native runtime remains loaded until process exit.

On Linux x86_64/aarch64, the optional helper downloads official Microsoft
ONNX Runtime **1.22.0**, verifies its pinned archive and library hashes, and
installs it into a user-selected directory with the upstream license/notices:

```sh
bash eva/tools/install-silero-runtime.sh --prefix "$HOME/.local/share/jwm/eva/onnxruntime/1.22.0"
```

For an offline install, supply `--archive /path/to/onnxruntime-linux-ARCH-1.22.0.tgz`.
There is no daemon startup download and no automatic service restart. Set the
printed absolute `ORT_DYLIB_PATH` in Eva's launch environment (for a user
service, a systemd `Environment=ORT_DYLIB_PATH=/absolute/path/...` override),
then restart Eva when convenient. Alternatively install the runtime via your
distribution's normal package manager; the loader searches `libonnxruntime.so`
and `libonnxruntime.so.1`. Changing a missing/incompatible runtime after the
first attempt requires a process restart.

The runtime source and licensing are at
https://github.com/microsoft/onnxruntime/tree/v1.22.0 . Other architectures can
use a compatible locally installed CPU runtime, or explicitly use RNNoise.

## Selection and failure behavior

```toml
[audio]
local_voice_detection = true
local_voice_backend = "silero"    # or "rnnoise"
local_voice_fallback = "rnnoise"  # or "none" to fail closed
silero_voice_threshold = 0.50    # Silero-specific probability calibration
local_voice_threshold = 0.65    # RNNoise/fallback calibration, preserves old tuning
# silero_model_path = "/absolute/path/to/the/pinned/silero_vad.onnx"
```

Silero and RNNoise scores are not calibrated interchangeably: thresholds are
selected by the actual active backend, including fallback. Confirmation remains
160 ms by default. A 600 ms public speech slice missed with the old common 0.65
threshold now opens at 0.50 without opening seven synthetic noise controls.
The native RMS floor is evaluated on the same 32 ms window as the cached
Silero decision, so trailing speech evidence is not erased by a later quiet
20 ms native frame. This fixes the 100/200 ms prelude regressions without
lowering the score or duration requirement.
A 240 ms mid-sentence slice after 400 ms of quiet is still missed: the short-slice
test without that leading quiet and with an 80 ms confirmation setting must not
be presented as a guarantee for all short commands or actual Chinese words.

`doctor` reports Silero readiness, a visible RNNoise fallback, or blocked
upload. Aggregate gate telemetry contains `voice_backend`,
`voice_backend_requested` and `voice_backend_degraded_reason`; it contains no
paths, audio or transcripts. A build with `--no-default-features` reports
`silero_feature_unavailable` if Silero is requested. Fallback happens only at
stream initialization. A model failure during a turn resets all local state
and stops that turn; it never admits unclassified PCM or switches models in
the middle of speech. Disabling `local_voice_detection` is an explicit choice
to upload continuously, not a failure recovery mechanism. A disabled gate does
not open model files or initialize the native runtime.

24 kHz mono S16LE is resampled with a stateful, anti-aliasing 2/3 FIR to 16 kHz,
then accumulated into exactly 512 new samples per inference. The preceding
64 samples and `[2,1,128]` recurrent state follow upstream. A decision is less than
one 32 ms model window old at a native 20 ms boundary; model input is never padded during
live capture. Original 24 kHz PCM and capture timestamps are kept for upload.
The bounded pre-roll includes the confirmation window, retaining short words
and initial consonants. Cloud VAD still commits turns; local trailing silence
does not create a second commit. All stream history resets on source gaps,
new input epochs or explicit reset. Echo rejection and native permissions
remain independent: VAD does not identify the user and TV speech may pass.

## Offline verification

```sh
cargo test -p jwm-eva -p jwm_eva_core --all-targets --locked
ORT_DYLIB_PATH=/absolute/path/libonnxruntime.so.1.22.0 \
  cargo test -p jwm-eva --test silero_offline --locked -- --ignored --nocapture
cargo test -p jwm-eva --no-default-features --all-targets --locked
```

The explicit real-model suite fails if Silero cannot initialize. It uses the
public licensed fixture documented in `eva/tests/fixtures/README.md`, plus
deterministic generated noise. Ordinary CPU tests also exercise unavailable
models, fallback, format errors, resampling and reset. None of these tests
opens a microphone, contacts a paid API or controls a user's desktop. Narrow
fixtures are not a general speech/noise accuracy or echo-cancellation claim.
