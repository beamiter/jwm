# Updating a directory of repositories

`scripts/git-update-all.sh` scans the directory itself and its immediate child
repositories. The `projects/git-update-all.sh` symlink points to this script.
It fetches one selected source per repository, rebases by default, and builds
known projects only when they changed or their artifacts are missing. Apps use
their own installers by default, so successful builds are installed too.
This includes jsh; `--force` rebuilds source changes even when its version
number has not changed. Libraries, cplus and LifeAI keep their build-only paths.

```sh
# GitHub origin only; other configured remotes are never contacted.
./git-update-all.sh --github ~/projects

# Pull from the matching SSH remote, e.g. box78's ubuntu@192.168.0.78:projects/jwm.
./git-update-all.sh --host 192.168.0.78 ~/projects

# Without a matching remote, use user@host:projects/<repository-directory>.
./git-update-all.sh --host ubuntu@192.168.0.78 ~/projects

# Override that fallback directory; configured matching remotes retain their paths.
./git-update-all.sh --host ubuntu@192.168.0.78 --host-root /srv/git ~/projects

# Select an existing remote by name, or the current branch's upstream remote.
./git-update-all.sh --remote box78 ~/projects
./git-update-all.sh --upstream ~/projects

# Fetch and report only, or update without building/installing.
./git-update-all.sh --host 192.168.0.78 -n ~/projects
./git-update-all.sh --github -N -T jagent,jwm ~/projects
```

GitHub is the default source (`origin`). GitHub, named-remote and upstream modes
push remaining local commits back to their selected source unless `--no-push`
is supplied. Host mode defaults to pull only; add `-u` to enable pushing to that
host. This is useful when the host repository is a working checkout that does
not accept pushes to its checked-out branch.

Source selection leaves remote URLs and branch upstream configuration unchanged.
The source branch is the current branch's configured `refs/heads/...` merge
branch, or the current local branch when no merge branch is configured. A missing
source branch fails the update. A host matching multiple configured remotes also
fails; use `--remote` to select one explicitly. Hosts accept DNS/IPv4 names and
optional SSH usernames. SSH aliases and authentication use the normal SSH config.

`-s` saves dirty changes in a stash during updates; otherwise dirty worktrees are
skipped. `-f` requires fast-forward history and `-m` permits a merge commit.
`-n` still fetches objects/tags, but does not change the working tree or push.
Run `--help` for build targets, installer arguments and all options.

The offline contract gate is `bash scripts/test-git-update-all.sh`. It uses local
bare repositories and Git URL rewrites to exercise SSH source selection without
network access, including unavailable unrelated remotes, dry runs, source-specific
pushes and unchanged upstream configuration.

`-B` forces rebuilding and installing the selected projects. `-C` compiles
without installing, and `-N` only updates repositories. `-I` explicitly selects
the default build-and-install mode. Installation failures are included in the
summary and produce a nonzero exit status.

`-X LIST` / `--skip-build LIST` skips the named projects' compilation and
installation while their Git updates and selected-source push behavior continue.
It takes priority over `-B` and missing artifacts, and skips those projects'
toolchain and sudo preflight checks. The build summary reports each exclusion.
`-T` continues to select which repositories are updated at all.

Names are comma-separated, accept surrounding whitespace and the `LifeAI` alias,
and must belong to the known build targets. Repeated `-X` options accumulate;
duplicates are ignored. Empty or unknown names fail before repository updates.
The exclusion applies to a project's own entry point; another project's build
tool may still compile shared dependencies it needs.

```sh
# Update every repository, but skip JWM's build and installation.
./git-update-all.sh -X jwm ~/projects

# Force other projects to rebuild, while skipping JWM and jsh.
./git-update-all.sh -B --skip-build=jwm,jsh ~/projects

# Update both terminals, but build/install only frost.
./git-update-all.sh -B -T ember,frost -X ember ~/projects
```

```sh
# Build and install the current published terminal dependency revisions.
./git-update-all.sh -B -T anvil,ember,frost,forge ~/projects

# Before publishing a coordinated core change, use sibling source checkouts.
./git-update-all.sh -B -L -T anvil,ember,frost,forge ~/projects

# Compile only; keep the installed apps unchanged.
./git-update-all.sh -B -C -T anvil,ember,frost,forge ~/projects
```

`-L` uses `jterm_core/scripts/family-cargo.py`, builds against sibling
`jterm_core` and `jagent` sources with temporary manifests and lockfiles, then
passes the exact compiled binary to each terminal's `scripts/install.sh`.
It does not rewrite release dependency pins. All four terminal installers
default to `~/.local/bin`; explicit installer arguments retain their target
directory settings. Existing source-selection, dirty-tree and dry-run rules
still apply.

The install-routing regression suite is
`python3 scripts/test-git-update-all-install.py`; it uses local repositories
and stub builders/installers and does not install anything on the host.
