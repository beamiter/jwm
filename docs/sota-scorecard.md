# Reproducible cross-product event-latency scorecard

This adds a **local, offline evidence validator and descriptive comparison**,
not a benchmark driver or a claim that JWM leads another product. It complements
`jwm-tool perf` and `docs/performance.md`: CPU render-pass durations are useful
for diagnosis but cannot substitute for physical input-to-photon latency.
No default compositor, remote protocol or runtime behavior changes.

## Run

```sh
python3 scripts/test-sota-scorecard.py
python3 scripts/sota-scorecard.py baseline.json > baseline-summary.json
python3 scripts/sota-scorecard.py baseline.json candidate.json > comparison.json
```

Python standard library only. No network, subprocess, desktop access or automatic
hardware collection. Inputs must be regular files, at most 16 MiB. Output goes to
stdout; the caller chooses whether to save/share it. Failures exit 2. Success
means evidence structure was accepted, **not performance acceptance**.

## Input v1

Each file describes one product/build and at least three repetitions. Example
shape below is a template, not measured data. Replace all placeholders and supply
three or more distinct repetitions. SHA256 fields must be actual lowercase hashes.

```json
{
  "schema_version": 1,
  "product": "jwm",
  "revision": "exact-public-commit",
  "binary_sha256": "<64 lowercase hex>",
  "config_sha256": "<64 lowercase hex>",
  "conditions": {
    "host_hardware": "GPU/CPU/RAM classes, no serial numbers",
    "client_hardware": "GPU/CPU/RAM classes or same-machine-local",
    "drivers": "host and client GPU driver versions",
    "kernel": "host and client kernel versions",
    "outputs": "resolution, refresh, scale, topology, HDR/VRR state",
    "network": "local-only or wired path, link rate, injected delay/loss",
    "workload": "versioned deterministic workload and equivalent settings",
    "measurement_method": "physical_input_to_photon",
    "clock_domain": "single-external-camera-frame-clock",
    "timestamp_uncertainty_us": 4200,
    "deadline_us": 50000
  },
  "runs": [{
    "repetition": "pair-01",
    "evidence_sha256": "<hash of retained original trace or recording>",
    "window_start_us": 0,
    "window_end_us": 2000000,
    "events": [
      {"id": "input-01", "input_us": 100000, "present_us": 130000},
      {"id": "input-02", "input_us": 200000, "present_us": null}
    ]
  }]
}
```

Use integer microseconds from a shared clock **within each repetition**, not
uncorrelated host/client wall clocks. Input and response must have a one-to-one
observable event ID. Record the first qualifying presentation; use null if none
was observed by the end. Do not omit missing events or silently substitute late
cleanup observations. Input schedules must leave a full deadline window for every
event. Add an observation tail after the final input. Warmup is outside the trace.
Software tests must use `software_input_to_present`, never the physical label.
The tool validates labels and hash syntax, not their truth: retain the executable,
config, trace/recording and event extraction procedure for independent review.

The same conditions, observation duration, repetition labels and relative input
schedule are required for comparison. Product/revision/binary/config hashes can
differ because products have different configurations. The operator must ensure
the configurations provide equivalent resolution, quality, effects and workload;
this is not established by equal labels. Keep a mapping of settings with results.
Do not compare JWM local compositing to a competitor's remote stream.

## Metrics and limitations

Each planned event is exactly one of on-time delivered, late delivered or missing.
`deadline_missed_fraction = (late_delivered + missing) / planned`. Thus delivering
an already-late event instead of losing it does not perversely increase failures.
Report latency p50/p95/p99/max **among delivered events**, always alongside missing
counts. All-missing latency is null, not zero. Quantiles use nearest rank. Do not
pool repeated runs, discard slow runs, or use a different control for each metric.
Every pair is retained with candidate-minus-baseline deltas. No tolerance or
non-inferiority margin is chosen after seeing results, and no automatic winner is
reported. Three repetitions are a schema minimum, not a statistical power claim.
Uncertainty is explicit and matched, not subtracted from measurements.

This schema is for discrete causal input/response events. It does not measure
continuous video frame freshness, power, bandwidth, image quality or static repair
convergence; keep existing remote replay/freshness diagnostics for those. Never
interpret an unloaded event test as evidence under sustained queue pressure.

## First hardware campaign

1. Begin with the RTX 5080 machine, then add an AMD or Intel GPU. Pin driver,
   kernel, output mode and power settings; record actual versions. No automatic
   changes to system settings are made by this tool.
2. Local track: JWM, Hyprland and niri on the same machine. Remote track: JWM,
   Sunshine/Moonlight and RustDesk using identical host/client/display/network.
   Each track has separate manifests and comparable workload/quality settings.
3. Use a camera or external instrument that observes both input trigger and
   screen response on the same clock. Document scanout location, event marker,
   frame rate and quantization uncertainty. Software timestamps remain a separate
   diagnostic track. Do not infer photon time from GL submission or swap completion.
4. Predeclare event schedules, warmup, test order, repeats and acceptance margins.
   Alternate product order to reduce thermal/time bias; retain failed runs. Cover
   idle clicks, editor scrolling, browser/video mixed load, mixed-refresh outputs
   and controlled network impairments. Never run competing builds during trials.
5. Publish descriptive paired results first. Also collect frame age, static text
   quality, payload bytes and resource use through their existing tools. A lower
   latency number obtained by degrading image quality or dropping more events is
   not an automatic improvement. Complete `hardware-validation.md` separately.

No physical measurements or competitor results are supplied by this patch.
