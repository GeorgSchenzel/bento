# bento

Per-checkout and per-forest [microsandbox](https://microsandbox.dev) boxes
on a shared nix store, with claude and opencode inside.

One **builder** VM per host owns a shared nix store and runs `nix-daemon`.
Every checkout (or git worktree), and every
[forest](https://github.com/GeorgSchenzel/forest) workforest, gets its own
**box**: a plain bash sandbox with the checkouts under `/workspace`, the
store mounted read-only and nix talking to the builder's daemon. Anything you `nix shell` or `nix build` in
one box lands in the shared store and is reused by all the others.

Runs on macOS on Apple Silicon, and on Linux (x86_64, aarch64) with KVM:
your user needs read/write access to `/dev/kvm`, usually through the `kvm`
group.

## Install

### Home Manager / nix-darwin

```nix
# your flake
inputs.bento.url = "github:YOUR_NAME/bento";
```

```nix
# your Home Manager configuration
imports = [ inputs.bento.homeManagerModules.bento ];

programs.bento.enable = true;
```

Ad-hoc: `nix profile install github:YOUR_NAME/bento`.

### cargo

```sh
cargo install --locked --path .   # needs rustc >= 1.94
```

From a clone: `just install` / `just uninstall`.

Either way, then run `bento install` once. It installs the microsandbox
runtime (0.7.8, into `~/.microsandbox`), creates the store, and builds the
box tools. The first run downloads about 1–2 GB. Nix on the host is not
needed. On Linux the nix package brings its own runtime instead. With
`cargo install` on NixOS, enable `programs.nix-ld`: the released runtime
expects a standard dynamic loader.

## Usage

Run these from inside a checkout or a forest:

```
bento enter      # bash in this checkout's box (created/started on demand)
bento claude     # claude in the box (args are passed through)
bento opencode   # opencode in the box (args are passed through)
bento exec       # run a command in the box without a TTY (see below)
bento start      # (re)create the box, applying config changes
bento stop       # stop the box (the builder keeps running)
bento rm         # remove the box and its state, archiving its chats
bento ls         # list the boxes: status, disk use, checkout or forest (gone ones marked)
bento prune      # remove the boxes whose checkout or forest is gone
bento builder    # bash in the builder VM
```

A box holds a forest of checkouts; a single checkout is a forest of one.
Each checkout is at `/workspace/<project>`, named after the project's dir
in the host layout (`~/projects/<project>/<branch>` →
`/workspace/<project>`). Commands start in the guest dir matching the host
cwd: `/workspace/backend/src` from `backend/src`, `/workspace` from the
forest root (or the checkout itself, in a forest of one).

- **Checkout:** `bento_<project>` for a main checkout,
  `bento_<project>_<dir>` for a linked worktree.
- **Forest:** anywhere inside `<root>/forests/<name>/` (forest's layout,
  with `<root>/repos/` beside it), including its checkouts, targets the
  forest's box `bento-forest_<name>`, with every checkout of the forest in
  it. A checkout added to the forest later shows up after `bento start`.

Boxes are keyed by path: after `forest rm`, `forest clean` or `forest mv`
the old box and its state (agent history included) are left behind;
`bento prune` removes them, archiving the chats.

`git commit` works in every checkout and lands in the host repo.
`/workspace` itself is box-owned (`boxes/<box>/root` on the host), so files
written there stay out of the host forest dir. Bento writes a short guide to
the box for the agents there, as `CLAUDE.md` and `AGENTS.md`: what's live on
the host, the checkouts and their branches, devenv, `just`, nix and docker.

### exec

`bento exec [--] <cmd> [args...]` runs a command in the box without a TTY:
stdin, stdout and stderr are piped through byte-exact and the exit code is
passed through, with nothing of bento's own on stdout (its progress and
warnings go to stderr). It starts the box if needed and starts in the guest
dir matching the host cwd, like the other commands. Made for scripts and
machine-to-machine pipes, e.g.
`hc machine add --via 'bento exec -- hc connect'`, `printf 'a\0b' | bento exec -- cat`.
It does not enter a devenv shell (see below); wrap in
`bento exec -- devenv shell -- <cmd>` for a checkout's tools.

### The bridge (`bento serve`)

`bento serve` runs the daemon the boxes talk to: the in-box `forest`
command and GPU-job queueing are client calls to it (TCP, JSON lines,
`127.0.0.1` only — boxes reach it as `host.microsandbox.internal`).
Started by the user, e.g. `bento serve --port 17761 &`; nothing
auto-starts it.

- Port: `--port`, else `$BENTO_BRIDGE_PORT`, else 17761. The port becomes
  the process's `BENTO_BRIDGE_PORT`, so boxes created by a handoff record
  the port this daemon actually listens on; an explicit
  `$BENTO_BRIDGE_PORT` rewrites a differing stored port in a box's
  `bridge.json` (the token is kept).
- Every box carries its identity in the env: `BENTO_BRIDGE` (the daemon's
  `host:port`), `BENTO_BRIDGE_TOKEN` (a random 128-bit token, generated
  once per box, kept in `~/.local/share/bento/boxes/<box>/bridge.json`
  with the box's forest dir and hc machine name), `BENTO_BOX`, and
  `BENTO_FOREST` (workforests only). The daemon authenticates by the
  token alone; a connection must start with `hello`.
- Ops (protocol, request and reply shapes: `src/bridge.rs`): `hello`,
  `forest.ls`, `forest.add`, `forest.name`, `forest.new`, `gpu.submit`,
  `gpu.status`, `gpu.ls`. Forest work runs the host's `forest` binary —
  `$BENTO_FOREST_BIN`, else `forest` on PATH — as argv, never through a
  shell, always `--no-fetch` for anything that could fetch.

`forest.new` with a `brief` is a **handoff** (decisions §5): after the
forest itself, the daemon creates and starts the new forest's box
in-process (the same code path as `bento start`, which also registers the
hc machine when `BENTO_HC` is set) and starts the first agent on it:

```
forest new spike-x bento ois --brief - <<< "what to do"
# → box bento-forest_spike-x, machine spike-x, terminal spike-x/t1 (lead)
```

The lead agent is `hc run --bg --on <machine> --name lead --cwd /workspace
opencode "<brief>"`; its brief gets a header naming the calling forest and
the new one. Without `BENTO_HC` the reply keeps `box` and carries a
`warning` instead of `machine`/`terminal`. A failure past the forest
creation names the step and what exists — nothing is deleted; the user
decides. Measured end-to-end: ~5 s (box create ~2.5 s, machine connect
~1–2 s), well inside the client's 120 s reply timeout, so the op stays
synchronous (no background thread, no `.handoff.log`).

### GPU jobs (`gpu.*`)

One jailed job at a time on the host, FIFO across all forests: an agent in
a box runs `forest gpu run -- <cmd>` (the bridge ops `gpu.submit` /
`gpu.status` / `gpu.ls`), the queue in `src/gpu.rs` runs it as
`timeout -k 5 <t> gpujail.sh <forest> -- bash -lc <cmd>`, and the results
land in `<forest>/<checkout>/.gpu-jobs/<id>/` (`cmd`, `stdout`, `stderr`,
`exit` written last, `timed-out`, `files/` — the job's `$GPU_JOB_FILES`),
readable from the box at `/workspace/<checkout>/.gpu-jobs/<id>`.

- **Two result stores**: `.gpu-jobs/` is writable by the box and by the
  job, so the daemon keeps the authoritative results in its own
  `$BENTO_GPU_STATE/out/<id>/` (stdout/stderr pipes, `exit`, `timed-out`;
  `gpu.status` tails from there) and only *copies* them into the checkout
  with `openat` + `O_NOFOLLOW` at every hop and `O_CREAT|O_EXCL` files —
  a symlink planted anywhere on that walk skips the copy (never follows),
  recorded in the job and shown by `gpu.status` as
  `results_copied`/`results_error`. `files/` alone is written by the job
  inside the jail.
- The jail (bubblewrap, `assets/gpujail.sh`, embedded into the binary and
  written to the state dir on `bento serve`): no network, only the forest's
  files writable, `~/.ssh`/other forests/`~/projects/repos`/credentials
  invisible, `/var` `/srv` `/mnt` `/media` private tmpfs, every checkout's
  real `.git` dir overlayed by a tmpfs, no controlling tty
  (`--new-session`), stdin from `/dev/null`, GPU (`/dev/dri` + NVIDIA
  nodes) and a display available. Properties are re-verified by
  `assets/test-gpujail.sh` (headless and real-display mode).
- **One daemon per state dir** (`flock` on `$BENTO_GPU_STATE/lock`); jobs
  run in their own process group with `PR_SET_PDEATHSIG`, so a daemon
  crash kills the running job, and the next daemon's startup kills any
  recorded leftover group before marking the job exit 250. Job records
  are written atomically (temp + rename).
- **Display**: by default each job gets its own **headless compositor**
  (sway, `WLR_BACKENDS=headless`, on the real GPU) inside the jail — the
  user's real wayland/X11 sockets are not bound, so jobs can neither
  screenshot nor inject into the real desktop. Consequences: no X11 in jobs,
  and the headless seat has no input devices (tools with mesa-demos' eglut
  bug, e.g. `eglgears_wayland`, crash on it). `BENTO_GPU_REAL_DISPLAY=1` on
  the daemon restores R5's real-display binding for such cases.
- State: `$BENTO_GPU_STATE` (default `~/.local/share/bento/gpu/`) holds
  `{queued,running,done}/<id>.json` (claim by rename; a job found `running`
  at daemon startup is killed and marked exit 250), the authoritative
  `out/<id>/` results, the `lock`, and the extracted `gpujail.sh`.
- Safety (decisions.md §1): `gpu.*` needs a workforest box (a single
  checkout's box would jail the repo root, `.git` included); the submitted
  `cwd` must realpath inside a checkout of the caller's forest (no `..`,
  no absolute, no symlinked `.gpu-jobs`/job dirs); `gpu.status`/`gpu.ls`
  only show the caller's own jobs; the command reaches the jail as argv,
  never through a host shell.

### herdr (`hc`) machines

With `BENTO_HC` set (the host `hc` binary's path), bento registers every
workforest box as a machine on the host's hc core when it creates it
(`bento start`), so `hc ls` on the host lists and drives the agents of
every forest: `hc run --on <forest-name> --cwd /workspace opencode "<task>"`.
The machine name is the forest dir's name (`spike-x` → machine `spike-x`,
kept in the box's `bridge.json`), made unique across boxes with a `-2`,
`-3`, … suffix when another box's bridge.json already claims it, and its
via line is `<msb> exec --stream
<box> -- sh -c` — byte-exact stdio without a PTY, and never `bento exec`,
which would start or even recreate stopped or removed boxes from hc's
reconnect loop. Registration replaces any entry of the same name — never
one another box still claims — and only warns on failure; `bento rm` and
`bento prune` remove the machine entry.
Boxes of single checkouts are not registered.

Without `BENTO_HC` nothing touches any hc core (no PATH fallback). Test
with an isolated core, never the real one: a separate `hc` binary plus
`XDG_CONFIG_HOME`/`XDG_STATE_HOME`/`XDG_RUNTIME_DIR` of your own, `BENTO_HC`
pointing at that binary, `hc server start`, then `bento start`.

### Agents

The box sees the host's agent config read-only: `~/.claude/{settings.json,
CLAUDE.md,skills,plugins,agents,commands}` and everything in
`~/.config/opencode`. Its HOME has the same path as on the host, so absolute
paths in that config still resolve. Agent state (history, sessions, the
opencode db) is kept per box in `~/.local/share/bento/boxes/<box>/`.
Plugins that call host-only binaries won't work in the box.

`bento rm` and `bento prune` first move the box's chats to
`~/.local/share/bento/archive/<box>-<unix time>/`, and leave the box alone if
that fails. They keep claude's `history.jsonl` and session transcripts, and
opencode's db and its WAL. These hold tool output inline and the db holds
opencode's API keys in plaintext, so the archive is `0700`. Nothing cleans it
up.

- **claude** and **opencode** run permission-free: bento passes
  `--dangerously-skip-permissions` / `--auto`, skipped when you pass your own
  permission flag. opencode only accepts `--auto` after a subcommand
  (`bento opencode run msg` runs `opencode run --auto msg`); claude's flag
  goes in front as before. The box is the sandbox, and the box-wide `IS_SANDBOX=1`
  lets claude run permission-free as root. `CLAUDE_CODE_SANDBOXED=1` and a
  `skipDangerousModePermissionPrompt` setting skip claude's "trust this folder"
  and bypass-mode prompts, so a checkout's hooks and MCP servers run in the
  box unasked.
- **opencode** uses the host's `~/.local/share/opencode/auth.json`
  (read-only).
- **claude** doesn't get the host login (on macOS it lives in the Keychain).
  Create a long-lived token once and save it:

  ```sh
  claude setup-token   # on the host
  mkdir -p ~/.config/bento
  (umask 077; cat > ~/.config/bento/claude-token)  # paste the token, then Ctrl-D
  ```

  Or set `BENTO_CLAUDE_TOKEN` on the host, which takes precedence over the
  file. Switch accounts by setting it per run:
  `BENTO_CLAUDE_TOKEN=$WORK_TOKEN bento claude`.

Bento also ships an agent skill, [`skills/forest/SKILL.md`](skills/forest/SKILL.md),
teaching agents in a box the `forest` command: what a forest is, listing and
adding repos, naming the work, handing work off to a new forest, and queueing
GPU jobs on the host. Install it on the host into `~/.claude/skills/forest/`
and `~/.config/opencode/skills/forest/`; both are mounted read-only into every
box (see above), so claude and opencode find it in every forest.

### devenv

`enter`, `claude` and `opencode` run inside the `devenv shell` of the
nearest `devenv.nix` up from the cwd, and each checkout's `.devenv` is
overlaid like `target` (see below). The first activation builds the shell
through the builder and can take minutes; after that it's cached in the
shared store. `exec` skips the wrapper — it runs the command directly, for
byte-exact stdio — so ask for the shell yourself when you want it.

At the root of a forest with several checkouts there is no devenv shell;
`cd` into a checkout for its tools. A forest of one starts in its checkout.

### Docker

Every box runs its own `dockerd` (with compose and buildx), started on
every connect; `docker` just works in `enter`, `claude` and `opencode`.
Its data root `/var/lib/docker` is an ext4 disk image,
`boxes/<box>/docker.raw` (docker's overlayfs can't stack on the box's
overlay rootfs). It is sparse, sized by `quota_gib`, and kept when the box
is recreated, so pulled images and volumes survive `bento start`; `bento rm`
deletes it. Ports published with `-p` are reachable inside the box only.
The image never shrinks on the host; `docker system prune` frees space for
reuse inside it. Log: `/tmp/dockerd.log` in the box.

### bento.yaml

Optional, at the checkout root (per checkout in a forest):

```yaml
# Repo dirs the box shadows with its own (linux) copies, kept in the box's
# state dir, so host and box build artifacts never mix.
overlay:
  - target
# GiB the box may add to each writable mount, and the size of its docker
# disk (default 64).
quota_gib: 64
```

## How it works

```
~/.local/share/bento/
├── store.sparseimage   macOS: case-sensitive APFS, sparse, max 60 GB
├── store/nix           its mountpoint (Linux: a plain dir, no image);
│                       /nix for the builder and every box
└── boxes/<box>/        per-box state: root/ (/workspace), ov/ (overlays),
                        docker.raw (/var/lib/docker), agent state,
                        generated git config

bento-builder           /nix rw, nix-daemon ← socat TCP, 127.0.0.1:17760
bento_<project>…        /nix ro, /tmp/nix.sock → host.microsandbox.internal:17760
                        NIX_REMOTE=unix:///tmp/nix.sock
```

- The builder is the only process that writes to the store. Locks don't
  work across microsandbox VMs, so two VMs writing the same store corrupt it.
- The box tools (claude-code, opencode, devenv, docker, git, just, python3, uv, socat, ncurses,
  herdr-core as `hc` for running agents, and the `forest` client for the bridge) come from a
  flake that is embedded in the binary (`tools/`). The builder builds it
  into the GC root `/nix/var/nix/gcroots/bento/tools`, and boxes have its
  `bin/` on PATH. Changing the flake rebuilds the tools on the next command.
- Every command checks the builder's health with a nix-daemon handshake.
  When the check fails, or bento changed, the builder is recreated, never
  restarted: published ports and background processes don't survive a
  restart.
- There is no garbage collection yet; the store only grows (on macOS up to
  the image cap; on Linux nothing caps its total size). On Linux it needs a
  filesystem with user xattrs (ext4, btrfs and xfs have them): msb keeps the
  guest's ownership and modes there.

## Development

```sh
nix develop --no-pure-eval   # same environment as `devenv shell`
```

Bumping the `microsandbox` crate means updating the release hashes in
`nix/package.nix`. Design notes and their evidence: `docs/handoff-shared-store.md`
and the `experiments/shared-store` branch.
