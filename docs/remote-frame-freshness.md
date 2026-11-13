# Extended combined-frame workload diagnostics

These are opt-in synthetic TCP/software-GL experiments, not deployment latency
or acceptance claims. The original fixed workload and summary decision remain
unchanged. No production scheduler is enabled by this instrumentation.

Run the ignored `actual_static_lossless_region_coalesced_workload_matrix` test
with `JWM_REMOTE_WORKLOAD_CASE` and an absolute `JWM_REMOTE_WORKLOAD_REPORT` path.
Cases are `marker0`, `blocks50`, `noise100`, `cap_noise150`,
`overcap_noise250`, `noise0`, and `noise250`. Each runs production/coalesced/
coalesced/production arms, 80 scheduled generations and an eight-second active
window. Cases vary the dynamic rectangle and deterministic noise content;
feedback holds are 0, 50, 100, 150 or 250 ms. They model presentation-credit
pressure, not network bandwidth or WAN latency. Noise retains a fixed eight-bit
Gray marker inside the dynamic rectangle. All remaining pixels are checked for
static exactness. Matching input hashes are required across arms.

The matrix retains failure evidence, including endpoint cleanup failures. A
session can complete correctly without achieving static convergence; that
negative result is recorded rather than changing correctness assertions.

`python3 scripts/remote-frame-freshness.py REPORT --output DIAGNOSTICS` adds a
separate diagnostic view; `--self-test` runs adversarial cases and 100 seeded
independent age-integral checks.

- Every planned generation is exactly one of on-time, late within the active
  window, or missing within the active window. Cleanup-only arrivals stay missing
  in the active window. Unsubmitted generations remain in the denominator.
- Display age integrates presentation time minus the source commit timestamp of
  the currently displayed identity. Repeated frames do not reset source age.
  Backward identities are retained and counted rather than made monotonic.
- No identity is attributed if its source commit follows GL readback start.
  Unidentified frames and startup before the first known frame produce unknown
  coverage, not zero age. Mean age is conditional on known coverage and must be
  reported with that coverage. This is still software presentation evidence.
- Refresh intervals, startup and tail waits, unknown identities and cleanup-only
  observations are separate. The oldest/newest timing averages are not a proof
  of statistical non-regression.

A fixed 100 ms deadline is expected to saturate under a 150 ms feedback hold;
raw late-frame counts can also penalize a policy that successfully delivers more
frames. Keep these facts visible without replacing or relaxing the old verdict.
No benchmark numbers from a previous lost workspace are claimed as validation of
this reconstructed source. Rebuild and rerun the intended matrix and checks.

The optimized Wayland CI job explicitly runs all seven cases and the Python
self-tests. It retains failed session records locally while failing the job;
no raw traces are uploaded. Existing fixed-workload verdicts remain separate.
