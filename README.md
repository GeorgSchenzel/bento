# bento

Per-checkout and per-forest [microsandbox](https://microsandbox.dev) boxes
on a shared nix store, with claude and opencode inside.

One **builder** VM per host owns a shared nix store and runs `nix-daemon`.
Every checkout (or git worktree), and every
[forest](https://github.com/GeorgSchenzel/forest) workforest, gets its own
**box**: a plain bash sandbox with the checkouts under `/workspace`, the
store mounted read-only and nix talking to the builder's daemon. Anything you `nix shell` or `nix build` in
one box lands in the shared store and is reused by all the others.

macOS on Apple Silicon only, for now.

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
needed.

## Usage

Run these from inside a checkout or a forest:

```
bento enter      # bash in this checkout's box (created/started on demand)
bento claude     # claude in the box (args are passed through)
bento opencode   # opencode in the box (args are passed through)
bento start      # (re)create the box, applying config changes
bento stop       # stop the box (the builder keeps running)
bento rm         # remove the box and its state
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
`bento prune` removes them.

`git commit` works in every checkout and lands in the host repo.
`/workspace` itself is box-owned (`boxes/<box>/root` on the host), so files
written there stay out of the host forest dir. Bento writes a short guide to
the box for the agents there, as `CLAUDE.md` and `AGENTS.md`: what's live on
the host, the checkouts and their branches, devenv, `just`, nix and docker.

### Agents

The box sees the host's agent config read-only: `~/.claude/{settings.json,
CLAUDE.md,skills,plugins,agents,commands}` and everything in
`~/.config/opencode`. Its HOME has the same path as on the host, so absolute
paths in that config still resolve. Agent state (history, sessions, the
opencode db) is kept per box in `~/.local/share/bento/boxes/<box>/`.
Plugins that call host-only binaries won't work in the box.

- **claude** and **opencode** run permission-free: bento passes
  `--dangerously-skip-permissions` / `--auto`, skipped when you pass your own
  permission flag. The box is the sandbox, and the box-wide `IS_SANDBOX=1`
  lets claude run permission-free as root.
- **opencode** uses the host's `~/.local/share/opencode/auth.json`
  (read-only).
- **claude** can't use the host login (it lives in the macOS Keychain).
  Create a long-lived token once and save it:

  ```sh
  claude setup-token   # on the host
  mkdir -p ~/.config/bento && pbpaste > ~/.config/bento/claude-token  # after copying the token
  chmod 600 ~/.config/bento/claude-token
  ```

### devenv

`enter`, `claude` and `opencode` run inside the `devenv shell` of the
nearest `devenv.nix` up from the cwd, and each checkout's `.devenv` is
overlaid like `target` (see below). The first activation builds the shell
through the builder and can take minutes; after that it's cached in the
shared store.

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
├── store.sparseimage   case-sensitive APFS, sparse, max 60 GB
├── store/nix           its mountpoint; /nix for the builder and every box
└── boxes/<box>/        per-box state: root/ (/workspace), ov/ (overlays),
                        docker.raw (/var/lib/docker), agent state,
                        generated git config

bento-builder           /nix rw, nix-daemon ← socat TCP, 127.0.0.1:17760
bento_<project>…        /nix ro, /tmp/nix.sock → host.microsandbox.internal:17760
                        NIX_REMOTE=unix:///tmp/nix.sock
```

- The builder is the only process that writes to the store. Locks don't
  work across microsandbox VMs, so two VMs writing the same store corrupt it.
- The box tools (claude-code, opencode, devenv, docker, git, just, python3, uv, socat, ncurses) come from a
  flake that is embedded in the binary (`tools/`). The builder builds it
  into the GC root `/nix/var/nix/gcroots/bento/tools`, and boxes have its
  `bin/` on PATH. Changing the flake rebuilds the tools on the next command.
- Every command checks the builder's health with a nix-daemon handshake.
  When the check fails, or bento changed, the builder is recreated, never
  restarted: published ports and background processes don't survive a
  restart.
- There is no garbage collection yet; the store only grows (up to the
  image cap).

## Development

```sh
nix develop --no-pure-eval   # same environment as `devenv shell`
```

Bumping the `microsandbox` crate means updating the release hashes in
`nix/package.nix`. Design notes and their evidence: `docs/handoff-shared-store.md`
and the `experiments/shared-store` branch.
