# Release process

JWM has no stable published release. This defines the automation and maintainer
checklist for a future release; version `0.2.0` is not a release announcement.

Only `jwm-v<semver>` tags start the workflow, and the tag version must exactly
equal the root `jwm` version in `Cargo.toml`. Components elsewhere in the
monorepo retain independent SemVer versions.

The job runs on x86_64 Ubuntu 22.04, the binary bundle's Linux ABI baseline. It
checks formatting and shell syntax, compiles all root targets and eight feature
profiles, runs Clippy and tests with required headless EGL coverage,
tests the shared protocol, provider adapters, bundled bar, and install
lifecycle, then runs:

```bash
bash scripts/package-release.sh --output-dir dist
```

It creates `jwm-<version>-source.tar.gz` with `git archive`, writes and verifies
`SHA256SUMS` over source and binary archives, passes that manifest to
`actions/attest@v4`, then uses `gh release create` on the existing tag.

## Maintainer checklist

1. Confirm CI and supply-chain checks are green. Complete direct DRM/KMS,
   driver, upgrade, rollback, and uninstall tests that hosted CI cannot run.
2. Move relevant changelog entries from `Unreleased` to the new JWM version and
   verify all bundled component versions and known limitations.
3. Update the root version and lockfile in a reviewed commit; do not align
   independent component versions unless they actually changed.
4. From a clean checkout, run:

   ```bash
   cargo fmt --all -- --check
   cargo check --locked --all-targets
   cargo clippy --locked --lib --bins --tests --no-deps -- -D warnings
   EGL_PLATFORM=surfaceless JWM_REQUIRE_HEADLESS_GL=1 \
     cargo test --locked --lib --bins --tests
   cargo test --locked -p shared_structures --all-targets
   cargo test --locked -p xbar_core --all-features --all-targets
   cargo clippy --locked -p xbar_linux_actions -p xbar_dbus_providers \
     --all-targets --no-deps -- -D warnings
   cargo test --locked -p xbar_linux_actions -p xbar_dbus_providers --all-targets
   cargo test --locked --manifest-path bars/tao_glow_bar/Cargo.toml --all-targets
   cargo test --locked --manifest-path bars/tao_pixels_bar/Cargo.toml --all-targets
   bash scripts/test-install-lifecycle.sh
   bash scripts/package-release.sh --output-dir dist
   ```

5. Create and push the reviewed version tag, for example:

   ```bash
   git tag -s jwm-v0.2.0 -m "JWM 0.2.0"
   git push origin jwm-v0.2.0
   ```

6. Download and inspect every asset. Verify checksums, provenance, contents,
   version output, and a clean install before announcing it.

## Immutability and correction

Repository administration must enable **Settings → General → Releases → Enable
release immutability** before publishing. A workflow cannot enable this policy.
Without it, tag checks, checksums, and provenance do not prevent an administrator
from later replacing assets or moving/deleting a tag.

Never reuse a published version/tag or replace its assets. Publish corrections
as a new patch release. Provenance identifies the producing repository, commit,
and workflow; it does not prove that an artifact is vulnerability-free.

Consumers can verify downloaded assets with:

```bash
sha256sum --check SHA256SUMS
gh attestation verify jwm-0.2.0-linux-x86_64.tar.gz --repo beamiter/jwm
```
