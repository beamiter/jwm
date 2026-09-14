# Status bars

JWM can spawn a status bar named in `config_*.toml` (`status_bar.name`). All bar
crates live under this directory and share protocol helpers from `xbar_core`.

## Official bar

**`tao_glow_bar`** is the supported daily-drive bar. `scripts/install_jwm_scripts.sh`
installs it by default. Release packaging and the maintainer checklist exercise
this crate.

```bash
scripts/install_jwm_scripts.sh                  # installs tao_glow_bar
scripts/install_jwm_scripts.sh -b tao_glow_bar  # explicit
```

## Example bars

Every other crate in this directory is an **example** or experiment (toolkit
coverage, web frontends, alternate presenters). They remain in-tree so
contributors can compare implementations, but they are not the product default
and are not required for a SOTA daily-drive install.

Use `-b <name>` only when you intentionally want an example bar.

## Shell Hub

Whatever bar you run, the Shell Hub entry should open the same built-in control
center documented in [docs/control-center.md](../docs/control-center.md).
