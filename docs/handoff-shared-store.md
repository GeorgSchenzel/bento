# Handoff: bento rewrite on a shared nix store

Status: implemented 2026-10-13 (steps 1–5 below); open: verify `bento claude`
with a real token (ro `plugins/`, `.claude.json` in `CLAUDE_CONFIG_DIR`).
Written 2026-10-13.

The current bento is a prototype and gets **rewritten**, not migrated. Guiding
rule: KISS and YAGNI. Remove old hacks rather than preserving them.

Evidence for the design lives on branch `experiments/shared-store` (not
merged on purpose):

- `EXPERIMENTS.md` §1–4: probes, numbers, failure modes. Read
  `git show experiments/shared-store:EXPERIMENTS.md` before starting
- `compare/RESULTS.md`: the image-size comparison that motivated this
- `examples/probe-*.rs`: runnable probes (`git switch experiments/shared-store`,
  then `BENTO_PROBE_STORE=<case-sensitive dir> nix develop --no-pure-eval -c
  cargo run --example <probe>`). `probe-sharedstore.rs` (daemon mode) and
  `probe-devenv-daemon.rs` are the closest thing to a reference
  implementation of the builder/box wiring

## Goal

One builder VM per host owns a shared nix store; every project/worktree box
mounts that store read-only and talks to the builder's nix-daemon. A box is
just a sandbox you enter with bash. No per-project images, no devenv
dependency, no PID 1 / services / autostart.

Why: today each project builds its own ~5.4 GB image via `devenv container`
on the host (needs a linux builder, full rebuild per devenv change). Shared
store: ~6–12 GB total for 5–10 projects, new worktree on a warm store in
30–50 s, re-entry in 0.4 s (EXPERIMENTS §4).

## Decisions (final)

| Topic | Decision |
|---|---|
| Image | `mirror.gcr.io/nixos/nix` pinned by digest, for builder **and** boxes. No custom image, no systemd/NixOS (considered and dropped: NixOS-in-builder, NixOS OCI image, debian-systemd — all add parts we don't need now) |
| Tools in boxes | An **embedded flake** (`include_str!` of `flake.nix` + `flake.lock` in the bento binary) whose `packages.default` is a `buildEnv` of opencode, claude-code (unfree → allowUnfree), git, socat. Builder `nix build`s it to the GC root `/nix/var/nix/gcroots/bento/tools`; boxes use `…/tools/bin` on PATH. Rebuild when the embedded flake's hash changes. No `nix profile` |
| Store | Case-sensitive APFS sparse image, **cap 60 GB**, at `~/.local/share/bento/store.sparseimage`, attached with `-nobrowse`; bento (re)attaches it on demand (after reboot) |
| Builder lifecycle | Auto-created by the first `start`/`enter`/`claude`/`opencode`. Stays up when boxes stop. Health = daemon port answers; otherwise **recreate** (never restart — see constraint 5). No `bento store …` commands |
| Daemon transport | nix-daemon ← socat TCP on a fixed port, published on **127.0.0.1 only** |
| GC | None in v1 (store just grows) |
| `enter` | Plain bash in `/workspace` with `NIX_REMOTE` + tools PATH. No devenv, no `nix develop` |
| Commands | `install`, `start`, `stop`, `enter`, `claude`, `opencode`, `builder` (bash in the builder VM), `clean`. Drop `build`, `check` |
| Agent auth | opencode: ro bind of host `~/.local/share/opencode/auth.json` (canonicalized — it is a sops symlink), as today. claude: long-lived token from `claude setup-token`, read from `~/.config/bento/claude-token`, passed as `CLAUDE_CODE_OAUTH_TOKEN` |
| Agent config | Same plugins/skills/settings as the host via **read-only** binds (box must not modify host config). Guest HOME = host HOME path (e.g. `/Users/georg`) so absolute paths in claude's plugin registry stay valid |
| Agent state | Per box, plain host dir (not msb named volumes): `~/.local/share/bento/boxes/<box>/{claude,opencode}` bound rw at `~/.claude` and `~/.local/share/opencode`; `CLAUDE_CONFIG_DIR` so `.claude.json` lands inside. `clean` deletes the dir. Not shared between boxes: sqlite/locks don't work across VMs (EXPERIMENTS §3a) |
| Kept features | Overlay dirs (`bento.yaml overlay:` → `.bento/ov/<path>`), git worktree boxes with in-box commits, path-derived box names + collision guard, bind quotas |
| Removed | per-project image build, `devenv container`, `BOX_NIX`/wrapper devenv, `/env/init-vibe` init shim, `materialize_wrapper`, `ensure_image`, justfile `box-*` recipes |
| Platform | macOS (aarch64-darwin) only for now |
| Install paths | Must work both via the flake **and** `cargo install`. Both use the microsandbox runtime the Rust crate pins (0.7.8, `~/.microsandbox`): cargo downloads it via the SDK's prebuilt build script; the flake stages the same release files with `fetchurl` (nix sandbox has no network). Nothing from nixpkgs' microsandbox. Bumping the crate means updating the hashes in `flake.nix`. Nix on the host is not required at runtime |

## Architecture

```
host
├── ~/.local/share/bento/store.sparseimage  (case-sensitive APFS, 60 GB cap)
│     mounted at e.g. /Volumes/bento-store; contains store/ and var/ — it *is* /nix
├── ~/.local/share/bento/boxes/<box>/{claude,opencode}   per-box agent state
│
├── bento-builder (sandbox, one per host, image nixos/nix@digest)
│     /nix ← store volume, rw, large quota
│     nix-daemon ── socat TCP-LISTEN:<port> ── published 127.0.0.1:<port>
│
└── bento_<project>[_<worktree>]  (one per checkout)
      /nix        ← store volume, READ-ONLY
      /workspace  ← checkout (rw bind, quota) + overlay binds + git binds
      $HOME/.claude, $HOME/.local/share/opencode ← per-box state (rw, quota)
        + ro binds of host config on top
      socat UNIX-LISTEN:/tmp/nix.sock → host.microsandbox.internal:<port>
      NIX_REMOTE=unix:///tmp/nix.sock
      PATH=/nix/var/nix/gcroots/bento/tools/bin:$PATH
```

The builder is the **only** writer of the store.

## Hard constraints (each cost a failed run)

1. **Never mount the store rw into more than one sandbox.** msb's virtiofs has
   no `getlk`/`setlk` (ENOSYS); locks don't exclude across VMs or the host.
   Two rw boxes corrupted the sqlite db in 14 s. EXPERIMENTS §3.
2. **Store on a case-sensitive filesystem.** Default APFS corrupts it
   (terminfo `a/` vs `A/`). EXPERIMENTS §1.
3. **Every writable bind needs an explicit `.quota(mib)`.** msb defaults to
   4 GiB guest-added bytes per bind (`DEFAULT_BIND_QUOTA_MIB`,
   `microsandbox/lib/sandbox/config.rs:35`) → ENOSPC while guest `df` lies;
   baseline taken at first write (looks flaky); near the limit msb re-walks
   the tree (one run: 16 min). Already fixed in the prototype (Phase 0 below).
4. **Clients need the `Host` network profile** to reach
   `host.microsandbox.internal`:
   `.network(|n| n.policy(NetworkPolicy::from_profiles([Public, Host])))`.
5. **Published ports and background processes don't survive stop→start**
   (EXPERIMENTS §2). Recreate the builder instead of starting it. Boxes:
   start the socat forwarder on every connect if it isn't running.
6. **The image's `/nix` is shadowed by the store mount.** Works only because
   the store was seeded from the same image; seeding = throwaway sandbox from
   the image with the store dir bound at `/shared`, `cp -a /nix/. /shared/`
   (~660 MB). Changing the image digest ⇒ the new image's nix must be copied
   into the store too (simplest: reseed-merge with the same cp; or keep the
   digest fixed and bump deliberately).
7. **Don't use host-side file presence as a sync signal** from a running box:
   guest writes to binds reach the host seconds later.

## Known-good recipe (from the probes)

Builder:

```sh
export NIX_CONFIG='sandbox = false
build-users-group =
experimental-features = nix-command flakes
substituters = https://cache.nixos.org
trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY='
nix build <flake dir>#default --out-link /nix/var/nix/gcroots/bento/tools   # NIXPKGS_ALLOW_UNFREE / allowUnfree in the flake
nohup nix-daemon >/var/log/nix-daemon.log 2>&1 </dev/null &
# wait for /nix/var/nix/daemon-socket/socket
nohup /nix/var/nix/gcroots/bento/tools/bin/socat TCP-LISTEN:$PORT,reuseaddr,fork \
  UNIX-CONNECT:/nix/var/nix/daemon-socket/socket </dev/null >/dev/null 2>&1 &
```

Box, after create/start:

```sh
nohup /nix/var/nix/gcroots/bento/tools/bin/socat UNIX-LISTEN:/tmp/nix.sock,fork \
  TCP:host.microsandbox.internal:$PORT </dev/null >/dev/null 2>&1 &
export NIX_REMOTE=unix:///tmp/nix.sock
export PATH=/nix/var/nix/gcroots/bento/tools/bin:$PATH
cd /workspace && exec bash
```

Store volume:
`hdiutil create -size 60g -type SPARSE -fs "Case-sensitive APFS" -volname
bento-store ~/.local/share/bento/store.sparseimage`, then
`hdiutil attach <path> -nobrowse` (no sudo). Inside `nix develop`, `cp` is
GNU; use `/bin/cp -c` for APFS clones.

## Host agent config layout (observed on the dev machine)

claude (`~/.claude`):
- config to bind ro: `settings.json`, `CLAUDE.md` (if present), `skills/`,
  `plugins/` (contains `installed_plugins.json`, `known_marketplaces.json`,
  `cache/`, `marketplaces/`), `agents/`, `commands/` (if present)
- state that stays box-local: `history.jsonl`, `projects/`, `sessions/`,
  `todos`, `shell-snapshots/`, `file-history/`, caches, `.claude.json`
  (normally `~/.claude.json`, outside the dir)
- credentials: macOS Keychain on the host (not mountable) → token instead
- plugin registry stores absolute host paths
  (`/Users/georg/.claude/plugins/cache/...`) → same HOME path in the box.
  Marketplaces pointing elsewhere (e.g. `~/.local/share/mise/...`) and
  plugins needing host binaries won't work in the box; accepted

opencode:
- `~/.config/opencode/`: `opencode.json` (home-manager symlink into the nix
  store → canonicalize before binding), `plugins/*.ts`, `commands/`,
  `node_modules/` + `package.json` (bun deps), `superpowers/`
- `~/.local/share/opencode/`: `auth.json` (sops symlink), sqlite db, logs →
  state, box-local except `auth.json`

To verify early (fallback: copy that dir into the box state on each entry
instead of binding it ro):
- claude with ro `plugins/` (may try to refresh marketplaces / write cache)
- opencode with ro `~/.config/opencode` (may `bun install` into it at start)
- `CLAUDE_CONFIG_DIR` really moves `.claude.json` into the dir; onboarding
  may need a pre-seeded `.claude.json` (`hasCompletedOnboarding: true`)
- `CLAUDE_CODE_OAUTH_TOKEN` works without any login step in the box

## Plan

Each step ends with its acceptance check. Write it as a fresh `src/` (split
into modules: store/builder, box, agent config, commands); reuse prototype
pieces where they fit (repo discovery, worktree pointers, overlays,
gitconfig, collision guard).

1. **Store volume** — create/attach sparse image idempotently; seed from the
   image if empty. Accept: volume attached after a fresh login, seeded store
   has `nix` and a valid db (`sqlite3 <store>/var/nix/db/db.sqlite
   'PRAGMA integrity_check'` = ok).
2. **Builder** — create `bento-builder` (store rw + large quota, port on
   127.0.0.1), write the embedded flake into it, build the tools root,
   start daemon + socat, health check; recreate when unhealthy; `bento
   builder` enters it. Accept: `bento builder` gives a shell;
   `/nix/var/nix/gcroots/bento/tools/bin/claude --version` works; killing
   the daemon then running any bento command recovers.
3. **Boxes** — `start`/`stop`/`enter`/`clean`: nixos/nix image, `/nix` ro,
   Host network profile, socat forwarder, checkout + overlay + worktree
   binds with quotas. Accept: `bento enter` in a checkout and in a worktree;
   in the box `nix-shell -p hello --run hello` works (lands in the shared
   store), `git commit` works in a worktree box, `touch /nix/x` fails;
   two boxes entering concurrently both ok; host integrity_check ok.
4. **Agents** — `bento claude` / `bento opencode`: same HOME path, per-box
   state dirs, ro config binds, token/auth. Accept: both start without a
   login prompt, see the host's skills/plugins/commands; writing to a bound
   host config path from the box fails; host files unchanged.
5. **Cleanup + packaging** — delete the old image path and devenv bits from
   code, flake (`nix2container`, `mk-shell-bin`, devenv inputs if only used
   for the image), README and justfile. Accept: `cargo install --path .`
   on a clean `~/.cargo` and `nix build .#bento` both produce a working
   `bento install && bento enter`.

## Prototype state worth knowing

- **Phase 0 (bind quotas) is implemented but uncommitted** in the master
  worktree: `quota_gib` in `bento.yaml` (default 64, applied to checkout,
  overlay and worktree git-dir binds, `bento.quota` label + recreate
  warning). Verified: in a box, `dd` of 6000 MB into `/workspace/target`
  succeeds, guest `df` shows 64G. Carry the idea into the rewrite.
- A test box `bento_bento_master` exists (stopped). The running
  `bento_bento` box belongs to another checkout (`~/hot/bento`) — leave it.
- msb named volumes are plain dirs under `~/.microsandbox/volumes/<name>/`;
  no subpath mounts for named volumes (one more reason for plain host dirs).

## Useful SDK facts (microsandbox 0.7.8)

- bind quota: `.volume(guest, |m| m.bind(host).quota(mib))` (u32 MiB)
- ro bind: `m.bind(host).readonly()`; nested ro binds over a rw mount work
- ports: `.port(host, guest)` binds 127.0.0.1; `port_bind(ip, h, g)` for
  others; conflicts and missing IPs fail silently
- `.init("auto" | "/abs/path")` hands PID 1 to an init (not used now)
- `.patch(|p| …)` writes files/dirs into the rootfs at create
- `Image::load(tar, tags)` loads an OCI archive (not needed in the rewrite)
- `Sandbox::get(name)`, `handle.connect()`, `start_detached()`,
  `attach_with(cmd, |a| a.cwd(..).envs(..))` — see the prototype's
  `ensure_running` / `enter`

## Environment gotchas

- System rustc (1.93) is too old for the dependency tree; build inside
  `nix develop --no-pure-eval`. The README must state the minimum rustc for
  `cargo install`.
- The home-manager `msb` CLI (0.6.11) can't read the runtime db written by the
  0.7.8 SDK (`database schema is newer than this msb binary`); use the SDK
  (or read `~/.microsandbox/db/msb.db` with `sqlite3 -readonly`).
- `bento enter` needs a TTY; for scripted checks:
  `(sleep 3; printf 'cmd; exit\n'; sleep 60) | script -q /dev/null bento enter`.
