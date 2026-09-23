# Developing in a Minimal session

[Minimal](https://minimal.dev) (`min`) runs a sandboxed development session
from the blueprint in `minimal.toml`: the Rust toolchain, the scripts' tools
(sqlite3, jq, curl, flock), ffmpeg, cmake for the TLS library, `gh` and
Claude Code, and nothing else from the host. Every contributor and every
agent gets the same environment, pinned to one commit of the package
registry.

## What a session can and cannot do

A session builds the binary, runs what CI runs (`cargo fmt`, `clippy`, the
tests, a syntax pass over the scripts), runs the marathon audit, and replays
footage through the timer paths, since ffmpeg is in the registry and the
glyph reader needs nothing else.

Two things stay on the host. The AWS CLI is not in the registry either, but
the repo packages it itself, so the site deploy is a task (below).

- **tesseract** (and leptonica): anything that reads a pane — `locate`,
  `pane`, the splits, counter and title reads of a replay — needs the host.
  A replay in a session records timers, not boards.
- **the live bot itself**: it runs under `scripts/run-live.sh` on the host,
  and `scripts/rollout.sh` restarts it there.

## The blueprint

`minimal.toml` at the repository root. `[upstream]` pins the registry
commit (`min update` moves it); `[stack] use = "rust"` brings gcc, rustc and
cargo, with cmake added for aws-lc; `[session]` lists the tools every
session gets and patches in `~/.gitconfig` so commits inside carry a name;
`[tasks.*]` declares `check`, `test`, `build` and `audit` for
`min task run <name>`, each in a sandbox of its own.

`gh` is in the session but its login is not: `~/.config/gh` holds a GitHub
token, and whether that goes into a sandbox that also runs an agent is the
owner's decision. To push and open PRs from inside, add
`{ source = "~/.config/gh", dest = "~/.config/gh" }` to the session's
`patches` and allow those files in `~/.config/minimal/user_policy.toml`;
without it, commit inside and push from the host.

## Bringing a session up

The session is composed from the `minimal.toml` it finds in the project it
syncs, so the project has to be synced. This tree cannot be: the VODs, the
build directory and the replay databases under it run to hundreds of
gigabytes, and the uploader skips only `target` (a `.gitignore`-aware upload
is on Minimal's list). Activate from a **git worktree** of the branch
instead, a few megabytes of tracked files that Minimal recognises as a
repository:

```sh
git worktree add --detach /tmp/grindlog-wt <branch>
cd /tmp/grindlog-wt
min session activate --name grindlog --no-prompt .
min session attach grindlog
```

The first activation asks, through the user policy, to allow `~/.gitconfig`
into sessions; `--no-prompt` prints the exact lines to add to
`~/.config/minimal/user_policy.toml`.

Inside, the workspace is a copy of the worktree. Commit there; bring the
work back by pushing to GitHub from inside (with the `gh` login patched in)
or, on the native provider, by fetching from the session's workspace over
the `min://` git remote helper:

```sh
git remote add min min://grindlog
git fetch min
```

## The microVM provider

The native provider (`local-minimald`) enters a session through
`/proc/<pid>/task/<pid>/children`, which a kernel built without
`CONFIG_PROC_CHILDREN` does not have; activation then succeeds but every
command fails with `failed to spawn process: reading /proc/.../children`.
The reference box's Gentoo kernel is built that way. Until it is rebuilt
with that option, use the microVM provider, which boots a small Linux VM
and hosts the session in it:

```sh
min --provider local-minvmd session activate --name grindlog --no-prompt .
min --provider local-minvmd session attach grindlog
min --provider local-minvmd session exec grindlog 'cargo test --release'
```

The `min://` git helper talks to the native provider only, so under the VM
provider bring work back through GitHub. A warning that "session hostnames
will not route" (port 7654 in use) is harmless here.

## Deploying the site as a task

`min task run deploy` (or `mip run deploy` on Linux) fills the run numbers,
builds the pages and the feed from the live database beside the repo,
uploads them and invalidates CloudFront, in a sandbox that holds exactly
what that needs: the Rust toolchain for the report binary, sqlite3, jq,
curl, flock, and the AWS CLI.

`base` is named in every task's packages: a task holds only what it lists
plus the stack's, and bash, coreutils, sed, grep and gawk come from `base`.

The AWS CLI is not in the public registry, so it is this repository's own
package: `packages/awscli/build.ncl` and `build.sh`, built the way the
registry builds httpie, with pip installing the sdist and its dependency
tree into the package's own site-packages. A build spec in the repo
references registry packages by name (`upstream "python"`) rather than by
the registry's relative paths. `mip check` validates it and
`mip package build awscli` builds and tests it in a clean room; the built
artifact is cached, so later tasks reuse it.

Credentials never appear in `minimal.toml`. The task maps the host's
`~/.aws` in read-only (`patches.dir."~/.aws" = "read-only"`), which the
user policy gates the first time. For a session, the same files come in
through a per-developer loadout, which the policy passes without
prompting: `~/.config/minimal/loadouts/aws.toml` copies `~/.aws/config` and
`~/.aws/credentials` into the session's home and sets `AWS_PROFILE`, and
`min session activate --loadout aws` applies it.
