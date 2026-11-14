#!/usr/bin/env bash
# gpujail.sh — run one command against one forest's files in a sandbox on the host.
#
#   gpujail.sh <forest-dir> -- <cmd> [args...]
#
# The queue (bento serve, src/gpu.rs) runs it as
#   timeout -k 5 <t> gpujail.sh <forest> -- bash -lc <cmd>
# with the job's cwd and result dirs in the wrapper env (never in argv, so
# no shell ever sees the command):
#   BENTO_GPU_CWD=<rel to the forest>   cwd inside the jail (default: the forest)
#   GPU_JOB_DIR / GPU_JOB_FILES         exported for the job verbatim
#   BENTO_GPU_REAL_DISPLAY=1            bind the user's real display (see Display)
#
# Properties (from R5, re-tested in assets/test-gpujail.sh):
#   - no network (own netns, nothing up, no resolver)
#   - only <forest-dir> is writable on disk (plus ephemeral tmpfs scratch: /tmp,
#     $HOME, /dev/shm); everything else is read-only or invisible
#   - ~/.ssh, other forests, ~/projects/repos, /run/user/<uid> (secrets.d, agent
#     sockets, dbus), nix daemon socket, opencode/claude credentials: invisible
#   - no host environment is passed in (clearenv) — credentials that live in env
#     vars (ANTHROPIC_API_KEY, …) do not leak into the job
#   - no controlling tty (--new-session): a job can never TIOCSTI keystrokes
#     into the wrapper's/daemon's terminal; the queue also runs the wrapper
#     with stdin from /dev/null, so a job cannot read the daemon's stdin
#   - GPU (NVIDIA + Mesa /dev/dri) and display work; see Display
#
# Tool: bubblewrap. Chose it over nsjail/firejail/systemd-run because it is the
# standard unprivileged sandbox on NixOS (flatpak's), available via
# `nix shell nixpkgs#bubblewrap`, and its bind-list model is exactly the shape
# of this problem. See notes/R5-jail.md for the comparison.
#
# Display (G1's change over R5): the default is a HEADLESS compositor per job —
# sway with WLR_BACKENDS=headless on the real GPU (WLR_RENDERER=gles2), started
# by the in-jail wrapper below with its own WAYLAND_DISPLAY on the jail's tmpfs
# /run, XWayland disabled. Proven to render with the NVIDIA GPU (eglinfo -B on
# the wayland platform, alacritty's GL output captured by grim). The user's
# real wayland/X11 sockets are NOT bound in this mode: a job cannot screenshot
# or inject input into the real desktop. Two consequences, accepted:
#   - no X11 in jobs (XWayland is off; DISPLAY is unset). X11-only tools need
#     BENTO_GPU_REAL_DISPLAY=1.
#   - the headless seat has no input devices (wl_seat capabilities 0); clients
#     with mesa-demos' eglut bug (eglgears_wayland and friends destroy a NULL
#     wl_pointer) crash on it. Real toolkits (SDL, alacritty, …) cope. Set
#     BENTO_GPU_REAL_DISPLAY=1 for such tools.
# XWayland being off is also what keeps the headless compositor from racing
# the user's session for X display numbers: an on-demand XWayland here grabbed
# :0 and the user's pipewire (module-x11-bell) segfaulted when it vanished.
# BENTO_GPU_REAL_DISPLAY=1 (an env var on the daemon) restores R5's behaviour:
# the user's wayland-1 socket and /tmp/.X11-unix are bound read-only.
#
# Exactly what is bound in, and why (R5's list, display lines per mode):
#   /                 -> /           ro    baseline: everything readable, nothing writable
#                                           (/nix/store = tools, /etc = fonts+passwd+ldcache, /sys)
#   /dev              -> /dev        new   fresh tmpfs with basics (null/random/tty/...)
#   /dev/shm          -> /dev/shm    new   writable shm; games/Chrome-style runtimes need it
#   /dev/dri          -> /dev/dri    rw    DRM nodes (card1, renderD128) for GL/EGL/Vulkan
#   /dev/nvidia*      -> /dev/nvidia* rw   proprietary driver nodes (0, -ctl, -modeset, -uvm, -uvm-tools)
#   /home             -> /home       new   tmpfs hides ALL homes (~/.ssh, other forests,
#                                           ~/projects/repos, ~/.claude, opencode auth); nothing
#                                           of the host home survives except the binds below
#   $scratch          -> $HOME       rw    ephemeral home (fontconfig/mesa/nvidia shader caches);
#                                           host mktemp -d, deleted after the job (see cleanup)
#   $forest           -> $forest     rw    THE point: the forest's files, same path as on host
#   <tree>/.git       -> itself     new   tmpfs over each checkout's real .git dir (a plain
#                                            clone's gitdir: no reads, no planted commits/hooks);
#                                            worktree .git FILEs are pointers, left alone
#   /run              -> /run        new   tmpfs hides /run/user/<uid> (secrets, agent sockets);
#                                           headless mode also puts the job's compositor runtime
#                                           dir on it (writable, gone with the jail)
#   /run/opengl-driver -> itself     ro    GL/EGL/Vulkan libs + ICD jsons
#   /run/current-system -> itself    ro    host PATH tools (symlink into /nix/store)
#   /run/systemd      -> itself      ro    harmless, some tools probe it
#   wayland-1 socket  -> itself      ro    the real display; REAL-DISPLAY MODE ONLY (connect()
#                                           on a unix socket is not a write)
#   /tmp              -> /tmp        new   writable ephemeral scratch; hides host /tmp
#   /tmp/.X11-unix    -> itself      ro    X11/XWayland socket; REAL-DISPLAY MODE ONLY (sway runs
#                                           XWayland without an Xauthority file)
#   /nix/var          -> /nix/var    new   tmpfs hides the nix daemon socket — else a jailed
#                                           `nix build` could write the store via the daemon
#   /proc             -> /proc       new   fresh procfs for the unshared pid ns
# Not bound: /run/user/<uid>/bus (dbus), pipewire/pulse (audio) — add per need later.
#
# Cleanup (the bug fix over R5's first version): the first version ended in
# `exec bwrap …`, which replaces the shell, so the EXIT trap that removes the
# scratch home never ran. Now bwrap runs as a child; we `wait` for it and
# propagate its status, and TERM/INT/HUP to this wrapper kill bwrap (bwrap also
# has --die-with-parent) and run the cleanup. A SIGKILL of the wrapper still
# leaks the scratch dir; the queue cleans stale ones (>1 day old) on startup.

set -euo pipefail

usage() {
  sed -n '2,12p' "$0" >&2
  exit 2
}

[ $# -ge 3 ] || usage
forest=$1; shift
[ "${1:-}" = "--" ] || usage
shift

forest=$(realpath "$forest")
[ -d "$forest" ] || { echo "gpujail: forest dir not found: $forest" >&2; exit 2; }

uid=$(id -u)
runtime=/run/user/$uid

# The job's cwd (relative to the forest, no .. — the queue validated it, this
# is the second lock on the same door).
chdir=$forest
if [ -n "${BENTO_GPU_CWD:-}" ]; then
  case $BENTO_GPU_CWD in
    /*|*..*) echo "gpujail: bad BENTO_GPU_CWD '$BENTO_GPU_CWD'" >&2; exit 2 ;;
  esac
  [ -d "$forest/$BENTO_GPU_CWD" ] || { echo "gpujail: cwd not found: $BENTO_GPU_CWD" >&2; exit 2; }
  chdir=$(realpath "$forest/$BENTO_GPU_CWD")
  case $chdir in
    "$forest"|"$forest"/*) ;;
    *) echo "gpujail: cwd escapes the forest" >&2; exit 2 ;;
  esac
fi

bwrap_bin=$(command -v bwrap) || { echo "gpujail: bwrap not found in PATH" >&2; exit 2; }

# ephemeral home for the jail (caches etc.), removed afterwards
scratch=$(mktemp -d /tmp/gpujail-home.XXXXXX)
bwrap_pid=

cleanup() {
  if [ -n "$bwrap_pid" ]; then
    kill -TERM "$bwrap_pid" 2>/dev/null || true
    wait "$bwrap_pid" 2>/dev/null || true
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
trap 'exit 129' HUP

# The headless in-jail wrapper: starts one compositor per job on the jail's
# tmpfs /run, waits for its socket, runs the command with that WAYLAND_DISPLAY,
# then kills the compositor. Runs as `bash -c <script> gpujob "$@"`, so the
# original argv passes as argv — nothing is re-quoted through a shell.
HEADLESS_WRAPPER='
set -u
rt=$XDG_RUNTIME_DIR
mkdir -p "$rt" && chmod 700 "$rt" 2>/dev/null || exit 250
printf "%s\n" "output HEADLESS-1 mode --custom 1920x1080@60.000Hz" "xwayland disable" > "$rt/headless-sway.conf"
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=gles2 \
  sway -c "$rt/headless-sway.conf" > "$rt/sway.log" 2>&1 &
sw=$!
wd=
for i in $(seq 1 100); do # 10 s for the compositor to come up
  for s in "$rt"/wayland-[0-9]*; do
    [ -S "$s" ] && { wd=${s##*/}; break 2; }
  done
  kill -0 "$sw" 2>/dev/null || break
  sleep 0.1
done
if [ -z "$wd" ]; then
  echo "gpujail: headless compositor failed to start:" >&2
  tail -5 "$rt/sway.log" >&2 2>/dev/null
  kill "$sw" 2>/dev/null; wait "$sw" 2>/dev/null
  exit 250
fi
export WAYLAND_DISPLAY=$wd
"$@"
rc=$?
kill "$sw" 2>/dev/null; wait "$sw" 2>/dev/null
exit $rc
'

display_binds=()
env_extra=()
if [ "${BENTO_GPU_REAL_DISPLAY:-0}" = "1" ]; then
  wayland_sock=${WAYLAND_DISPLAY:-wayland-1}
  display=${DISPLAY:-:0}
  [ -S "$runtime/$wayland_sock" ] || { echo "gpujail: no wayland socket $runtime/$wayland_sock" >&2; exit 2; }
  display_binds=(
    --ro-bind "$runtime/$wayland_sock" "$runtime/$wayland_sock"
    --ro-bind-try /tmp/.X11-unix /tmp/.X11-unix
  )
  env_extra=(--setenv WAYLAND_DISPLAY "$wayland_sock" --setenv DISPLAY "$display")
else
  # headless: the wrapper above runs the compositor inside the jail; the
  # original command argv passes through as argv, never re-quoted
  set -- bash -c "$HEADLESS_WRAPPER" gpujob "$@"
fi

# Overlay every checkout's real .git DIRECTORY with a tmpfs (M5). Worktrees
# have a .git FILE (a pointer to a hidden gitdir) — harmless, left alone. A
# checkout that is a plain clone has the real gitdir inside the forest, and
# the rw bind below would hand it to the job: it could read the repo's
# objects, plant commits, or write hooks. The tmpfs keeps the path (git in
# the jail sees an empty .git = not a repository) while the real contents
# stay on the host, untouched. Symlinked .git entries are skipped, not
# followed (bwrap resolves mount targets through symlinks).
git_overlays=()
for d in "$forest"/*/; do
  [ -d "$d.git" ] && [ ! -L "$d.git" ] || continue
  git_overlays+=(--tmpfs "${d%/}/.git")
done

env_job=()
[ -n "${GPU_JOB_DIR:-}" ] && env_job+=(--setenv GPU_JOB_DIR "$GPU_JOB_DIR")
[ -n "${GPU_JOB_FILES:-}" ] && env_job+=(--setenv GPU_JOB_FILES "$GPU_JOB_FILES")

# NOTE: --clearenv — the job sees ONLY the env below. The wrapper's environment
# (which may hold ANTHROPIC_API_KEY & friends from the session that queued the
# job) is not passed on. Tools come from the system profile and the per-user
# profile; anything fancier must be referenced by store path.
"$bwrap_bin" \
  --ro-bind / / \
  --dev /dev \
  --tmpfs /dev/shm \
  --dev-bind /dev/dri /dev/dri \
  --dev-bind-try /dev/nvidia0 /dev/nvidia0 \
  --dev-bind-try /dev/nvidiactl /dev/nvidiactl \
  --dev-bind-try /dev/nvidia-modeset /dev/nvidia-modeset \
  --dev-bind-try /dev/nvidia-uvm /dev/nvidia-uvm \
  --dev-bind-try /dev/nvidia-uvm-tools /dev/nvidia-uvm-tools \
  --tmpfs /home \
  --bind "$scratch" "$HOME" \
  --tmpfs /run \
  --ro-bind /run/opengl-driver /run/opengl-driver \
  --ro-bind-try /run/opengl-driver-32 /run/opengl-driver-32 \
  --ro-bind /run/current-system /run/current-system \
  --ro-bind-try /run/systemd /run/systemd \
  "${display_binds[@]}" \
  --tmpfs /tmp \
  --bind "$forest" "$forest" \
  "${git_overlays[@]}" \
  --tmpfs /nix/var \
  --proc /proc \
  --unshare-net \
  --unshare-pid \
  --unshare-ipc \
  --new-session \
  --die-with-parent \
  --chdir "$chdir" \
  --clearenv \
  --setenv HOME "$HOME" \
  --setenv PATH "/run/current-system/sw/bin:/etc/profiles/per-user/$(id -un)/bin" \
  --setenv TERM "${TERM:-xterm-256color}" \
  --setenv LANG C.UTF-8 \
  --setenv XDG_RUNTIME_DIR "$runtime" \
  --setenv XDG_DATA_DIRS "/run/opengl-driver/share" \
  --setenv XDG_CACHE_HOME "$HOME/.cache" \
  --setenv TMPDIR /tmp \
  "${env_extra[@]}" \
  "${env_job[@]}" \
  -- "$@" &
bwrap_pid=$!

status=0
wait "$bwrap_pid" || status=$?
bwrap_pid=
exit "$status"
