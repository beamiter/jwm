# Install, upgrade, rollback, and uninstall

JWM has no stable binary release yet. These commands describe the tested bundle
lifecycle once an asset is published. The bundle targets x86_64 Linux on the
Ubuntu 22.04 ABI baseline; other systems should build tagged source.

## Verify and install

Download both archives and `SHA256SUMS` from one release, then substitute the
actual version:

```bash
sha256sum --check SHA256SUMS
gh attestation verify jwm-0.2.0-linux-x86_64.tar.gz --repo beamiter/jwm
tar -xzf jwm-0.2.0-linux-x86_64.tar.gz
cd jwm-0.2.0-linux-x86_64
sudo bash install-release.sh install
/usr/local/bin/jwm --version
/usr/local/bin/jwm --backend x11rb --doctor
```

The installer stages files under `/usr/local/lib/jwm/versions/<version>` and
atomically switches relative links in `/usr/local/bin`. It preserves user data.
If a first managed install collides with legacy paths, inspect them and explicitly
authorize one-time takeover with `install --replace`; backups are retained for
final uninstall. Do not use `--replace` for normal upgrades. Packaging can add
`--destdir DIR` to every command; it is a staging root, not a runtime prefix.

## Upgrade

Save a session and private backup first:

```bash
jwm-tool msg save_session
backup="$HOME/jwm-backup-$(date +%Y%m%d-%H%M%S)"
mkdir -m 700 "$backup"
cp -a "$HOME/.config/jwm" "$backup/config" 2>/dev/null || true
cp -a "${XDG_STATE_HOME:-$HOME/.local/state}/jwm" "$backup/state" 2>/dev/null || true
```

Read the changelog, stop the graphical session, unpack the new bundle, then:

```bash
sudo bash install-release.sh install
/usr/local/bin/jwm --version
/usr/local/bin/jwm --backend x11rb --check-config
/usr/local/bin/jwm --backend x11rb --doctor
```

Installing an existing version is refused. A successful upgrade retains the
previous tree and switches stable links only after the new tree is complete.

## Roll back

Stop the session, then from either retained bundle run:

```bash
sudo bash install-release.sh rollback
/usr/local/bin/jwm --version
/usr/local/bin/jwm --backend x11rb --check-config
```

Rollback activates the previous install-history entry without deleting either
version. Restore the matching config/state backup if the older binary cannot
read newer user data.

## Uninstall

Remove one version with `sudo bash install-release.sh uninstall --version
0.2.0`. If active, the newest remaining history entry becomes active. Remove
all managed versions with either command:

```bash
sudo bash install-release.sh uninstall
# equivalent:
sudo bash install-release.sh uninstall --all
```

Final uninstall removes managed version trees, links, and installer state, then
restores paths backed up by the first `--replace`. User data is retained. After
reviewing it, optionally remove it separately:

```bash
rm -r -- "$HOME/.config/jwm"
rm -r -- "${XDG_STATE_HOME:-$HOME/.local/state}/jwm"
```

Those commands are destructive without a backup. The separately installed
portal is not owned by this core bundle lifecycle.
