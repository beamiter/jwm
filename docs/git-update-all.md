# Updating a directory of repositories

`scripts/git-update-all.sh` scans the directory itself and its immediate child
repositories. The `projects/git-update-all.sh` symlink points to this script.
It fetches one selected source per repository, rebases by default, and builds
known projects only when they changed or their artifacts are missing.

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
