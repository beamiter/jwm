# Local draft: #143 native manual session acceptance
No repository updates or workflow dispatch. This probe targets one real X11RB output on a dedicated Ubuntu 24.04 GitHub Actions runner, rather than representing Xvfb synthetic RandR monitor aliases as two real outputs. It does not test output migration, managed restart, Wayland, physical focus or key delivery. The focus positive control compares native X input focus to the owned window XID after explicitly exposing its tag, not during restore.

* Exact baseline: `9b3974b31566a2ae1bb6fe33cfb9642762434bf6`.
* Candidate: source tree `3e3a3ccd47b62de19702dcc5774cfca2fe1391ea` from `git merge-tree --write-tree 9b3974b3 4df4d36b`. The workflow expects the isolated acceptance base and detached candidate at commit `c2fec065c4a891712e7a4f34eb5dfbb8a6893cb6` with those exact parents, and requires both this commit and its tree. The workflow is limited to same-repository draft head `dot/manual-session-x11-validation` against the exact combined-product base `dot/manual-session-x11-product-base`; it checks event base, both parents and that the head adds only fixture files. Do not merge this isolated acceptance draft into master. If master advances, re-audit before updating either pin.
* Actual unmodified IPC `save_session` writes a private session JSON; new client processes are opened in reverse order. Actual unmodified IPC `restore_session` then reads that file. Tags are disjoint so default placements cannot fake the assertions.
* Each role does duplicate+unique, one duplicate survivor+unique, and distinct-instance positive control. Positive restores are required even in the ambiguity scenes; baseline acceptance concretely requires the old unintended placement, not generic failure.
* Three separate owned Xvfb instances per role; Xauthority negative and positive control, `SO_PEERCRED` verification, no TCP or inherited display/bus/home, XRes binds each real xmessage XID to its owned live process. Logical IDs and native XIDs are not interchanged. No root/system changes at probe runtime.
* Do not execute as root or on a user's host. Dedicated disposable runner only. Probe status and raw commands/IPC snapshots/logs are evidentiary; cleanup must also succeed.

## Same-job runner contract
Use separate exact detached checkouts and pinned checkout/Rust/artifact Actions consistent with CI. Install the X11RB build dependencies with the reviewed exact baseline `scripts/ci-install-deps.sh` (official Ubuntu), plus `xvfb xdotool x11-apps x11-utils xauth libxres1`. Set up an initial always-upload evidence directory; record both checkout SHA/tree, fixture SHA256 and installed package versions. Shell steps use `set -euo pipefail`. In separate matrix jobs (one baseline and one candidate), run:

```bash
fixture="$PWD/fixture/scripts/manual-session-x11"
source="$PWD/source"
evidence="$PWD/evidence"
mkdir -m 700 "$evidence"
toolbin="$(dirname "$(rustup which cargo)")"
git -C "$source" rev-parse HEAD 'HEAD^{tree}' > "$evidence/source.txt"
sha256sum "$fixture"/{probe.py,test_probe.py,build.sh} > "$evidence/fixture.sha256"
mkdir -m 700 "$PWD"/{build-home,build-cargo,build-config}
# ROLE=baseline|candidate; SOURCE_SHA must be the literal reviewed commit.
timeout -k 10 2400 env -i PATH="$toolbin:/usr/bin:/bin" HOME="$PWD/build-home" \
  CARGO_HOME="$PWD/build-cargo" XDG_CONFIG_HOME="$PWD/build-config" \
  RUSTC="$toolbin/rustc" CARGO_BUILD_JOBS=2 LANG=C.UTF-8 LC_ALL=C.UTF-8 \
  GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
  /bin/bash "$fixture/build.sh" "$source" "$evidence/build" "$ROLE" "$SOURCE_SHA" \
  2>&1 | tee "$evidence/build.log"
timeout -k 35 145 env -i PATH=/usr/bin:/bin LANG=C.UTF-8 LC_ALL=C.UTF-8 \
  /usr/bin/python3 -I "$fixture/probe.py" --role "$ROLE" \
  --binary "$source/target/release/jwm" --receipt "$evidence/build/build.json" \
  --out "$evidence/probe" 2>&1 | tee "$evidence/probe.log"
```

Always upload evidence on failure; check both matrix jobs, both binary receipts and all six scene results. Real setup failure is not baseline reproduction. Use an outer job limit >=65 minutes and an always artifact step; failed steps cannot be ignored. Do not reuse cached/supplied binaries. Before publishing, independently audit the fixture and actual YAML, including exact pins and isolated trigger.

Local runner lacks Xvfb and xdotool; the result presently establishes only offline verifier checks and code-path inspection, not runtime behavior.

Process cleanup retains unreaped leader PIDs until their owned groups are empty; Linux offline regression forks a child that outlives its leader. The global interruption gate prevents starting another scene and records the stop in the aggregate evidence. Raw focus uses xdotool getwindowfocus -f.
