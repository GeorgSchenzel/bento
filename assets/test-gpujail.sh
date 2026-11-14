#!/usr/bin/env bash
# test-gpujail.sh — re-verify every gpujail.sh property, printing PASS/FAIL per
# property. Uses a scratch dir inside this checkout as the "forest" (plus the
# real bento checkout for the git tests, read-only attempts only).
#
#   ./test-gpujail.sh                    # headless display (the default)
#   BENTO_GPU_REAL_DISPLAY=1 ./test-gpujail.sh   # R5's real-display binding
#
# GPU tests need mesa-demos + vulkan-tools from /nix/store. The queue itself
# (submit/serialize/recover) is tested in Rust (src/gpu.rs) and end-to-end
# against `bento serve`; R5's bash queue prototype (gpujob.sh) is gone.

set -u
here=$(cd "$(dirname "$0")" && pwd)
GJ=$here/gpujail.sh
T=$here/scratch/test   # scratch "forest"; safe to delete, gitignored by *
FOREST=$T/forest
CANARY=supersecret-canary
REAL=${BENTO_GPU_REAL_DISPLAY:-0}

pass=0 fail=0 skipped=0
ok()  { pass=$((pass+1)); printf 'PASS  %s\n' "$1"; }
bad() { fail=$((fail+1)); printf 'FAIL  %s\n' "$1"; }
skip() { skipped=$((skipped+1)); printf 'SKIP  %s\n' "$1"; }

# run in jail against the scratch forest, canary in the wrapper env
J()  { SECRETCANARY=$CANARY "$GJ" "$FOREST" -- "$@"; }

# ---- setup -------------------------------------------------------------
rm -rf "$T"
mkdir -p "$FOREST/game/.gpu-jobs"
echo "gitdir: /nonexistent (fake checkout for tests)" > "$FOREST/game/.git"
leaks_before=$(ls -d /tmp/gpujail-home.* 2>/dev/null | wc -l)

# GPU tools
gpu_tool() {
  local p
  p=$(find /nix/store -maxdepth 1 -type d -name "*-$1-*" 2>/dev/null | head -1)
  [ -n "$p" ] && { ls "$p"/bin/"$2" >/dev/null 2>&1 && printf '%s\n' "$p/bin/$2" && return 0; }
  return 1
}
EGLINFO=$(gpu_tool mesa-demos eglinfo || true)
EGLGEARS=$(gpu_tool mesa-demos eglgears_wayland || true)
VULKANINFO=$(gpu_tool vulkan-tools vulkaninfo || true)

# ---- 1. basics ----------------------------------------------------------
J true; [ $? -eq 0 ] && ok "exit code 0 propagates" || bad "exit code 0 propagates"
J bash -c 'exit 7'; [ $? -eq 7 ] && ok "nonzero exit code propagates (7)" || bad "nonzero exit code propagates (7)"
out=$(J bash -c 'echo out; echo err >&2' 2>&1)
[ "$out" = "out
err" ] && ok "stdout and stderr pass through" || bad "stdout and stderr pass through"
[ "$(J pwd)" = "$FOREST" ] && ok "cwd is the forest root (no BENTO_GPU_CWD)" || bad "cwd is the forest root"
BENTO_GPU_CWD=game J pwd | grep -q "$FOREST/game$" \
  && ok "BENTO_GPU_CWD sets the job cwd" || bad "BENTO_GPU_CWD sets the job cwd"
BENTO_GPU_CWD=../elsewhere J true 2>/dev/null && bad "BENTO_GPU_CWD with .. is rejected" || ok "BENTO_GPU_CWD with .. is rejected"
BENTO_GPU_CWD=/etc J true 2>/dev/null && bad "absolute BENTO_GPU_CWD is rejected" || ok "absolute BENTO_GPU_CWD is rejected"
out=$(GPU_JOB_DIR=/j GPU_JOB_FILES=/j/f GPU_JOB_CWD=game J bash -c 'echo "$GPU_JOB_DIR $GPU_JOB_FILES"')
BENTO_GPU_CWD=game GPU_JOB_DIR=/j GPU_JOB_FILES=/j/f J bash -c '[ "$GPU_JOB_DIR" = /j ] && [ "$GPU_JOB_FILES" = /j/f ]' \
  && ok "GPU_JOB_DIR/GPU_JOB_FILES are passed into the jail" || bad "GPU_JOB_DIR/GPU_JOB_FILES are passed into the jail"

# ---- 2. writability ------------------------------------------------------
J bash -c "echo x > '$FOREST/w'" && [ "$(cat "$FOREST/w")" = x ] \
  && ok "forest is writable" || bad "forest is writable"
J bash -c "echo x > \$HOME/hometest" && [ ! -e "$HOME/hometest" ] \
  && ok "jail \$HOME is writable and ephemeral (host home untouched)" || bad "jail \$HOME is writable and ephemeral"
J bash -c "echo x > /dev/shm/s" && ok "/dev/shm is writable" || bad "/dev/shm is writable"
J bash -c "echo x > /etc/t" 2>/dev/null && bad "/etc is not writable" || ok "/etc is not writable"
J bash -c "echo x > /nix/store/t" 2>/dev/null && bad "/nix/store is not writable" || ok "/nix/store is not writable"

# ---- 2b. system dirs are private tmpfs, host content invisible (M7) -----------
# /var /srv /mnt /media (whichever exist on the host) are overlaid with tmpfs:
# a job sees empty writable dirs, never the host's /var/lib, mounts, media.
for d in /var /srv /mnt /media; do
  [ -d "$d" ] || continue
  J stat -f -c %T "$d" 2>/dev/null | grep -q tmpfs \
    && ok "$d is a tmpfs (not the host's)" || bad "$d is a tmpfs (host fs visible: $(J stat -f -c %T "$d" 2>/dev/null))"
  J bash -c "echo x > '$d/bento-write-test'" 2>/dev/null \
    && ok "$d is writable in the jail (ephemeral tmpfs)" || bad "$d is writable in the jail (ephemeral tmpfs)"
done
# a host dir that must not leak through: /var/lib always has content
J bash -c '[ ! -e /var/lib/systemd ] && [ ! -e /var/lib/nixos ]' \
  && ok "host /var/lib content is invisible" || bad "host /var/lib content is visible in the jail"
[ ! -e /var/bento-write-test ] && ok "jail /var writes never reach the host" || bad "a jail write reached the host /var"

# ---- 3. invisibility ------------------------------------------------------
invis() { # name path-that-must-not-exist
  J bash -c "[ ! -e '$2' ]" && ok "$1 invisible" || bad "$1 invisible"
}
invis "~/.ssh"                  "/home/$USER/.ssh"
invis "~/.claude credentials"   "/home/$USER/.claude/.credentials.json"
invis "opencode auth.json"      "/home/$USER/.local/share/opencode/auth.json"
invis "~/.config/opencode"      "/home/$USER/.config/opencode"
invis "~/projects/repos"        "/home/$USER/projects/repos"
invis "nix daemon socket"       "/nix/var/nix/daemon.socket"
invis "/run/user/\$uid/bus"     "/run/user/$(id -u)/bus"
invis "/run/user/\$uid/secrets.d" "/run/user/$(id -u)/secrets.d"
invis "/run/user/\$uid/pipewire-0" "/run/user/$(id -u)/pipewire-0"
invis "/run/user/\$uid/systemd"  "/run/user/$(id -u)/systemd"

other_fail=0
# the REAL forest this suite lives in (…/forests/<own>/bento/…): its dirs are
# the scratch forest's ancestors — empty mountpoints, checked below
own_top=$(d=$here; while [ "$(basename "$(dirname "$d")")" != forests ]; do d=$(dirname "$d"); done; basename "$d")
for f in /home/$USER/projects/forests/*; do
  [ "$(basename "$f")" = "$own_top" ] && continue # own forest's ancestor chain, checked below
  J bash -c "[ ! -e '$f' ]" || { other_fail=1; echo "       visible: $f"; break; }
done
[ $other_fail -eq 0 ] && ok "all other forests (incl. exp-agent, spike-*) invisible" \
  || bad "all other forests (incl. exp-agent, spike-*) invisible"
# the bind of $FOREST forces empty mountpoint dirs for its ancestors; assert
# everything visible under the own forest dir is the chain to $FOREST or the
# forest's own content — nothing else
ok_path() {
  case "$1" in "$FOREST"|"$FOREST"/*) return 0 ;; esac # forest or inside it
  case "$FOREST" in "$1"|"$1"/*) return 0 ;; esac      # ancestor mountpoint chain
  return 1
}
stray=$(J find "/home/$USER/projects/forests/$own_top" -mindepth 1 2>/dev/null | while read -r p; do
  ok_path "$p" || echo "$p"
done)
[ -z "$stray" ] && ok "own forest's ancestors are empty mountpoint dirs (no other content)" \
  || bad "own forest's ancestors are empty mountpoint dirs (stray: $(echo "$stray" | head -3))"

# ---- 4. environment --------------------------------------------------------
J bash -c "[ \"\${SECRETCANARY:-}\" = \"$CANARY\" ]" 2>/dev/null \
  && bad "wrapper env (canary/credentials) leaks into jail" || ok "wrapper env (canary/credentials) does not leak into jail"
J bash -c '[ -n "${HOME:-}" ] && [ -n "${PATH:-}" ] && [ "${XDG_RUNTIME_DIR:-}" = /run/user/'"$(id -u)"' ]' \
  && ok "explicit jail env is set (HOME, PATH, XDG_RUNTIME_DIR)" || bad "explicit jail env is set"
# M1: even when the wrapper itself runs on a terminal (script gives it a pty),
# the jailed job must have NO controlling tty — /dev/tty must not openable
script -qefc "$GJ $FOREST -- bash -c '(exec 3<>/dev/tty) 2>/dev/null && echo HAS-CTTY || echo NO-CTTY'" /dev/null </dev/null 2>&1 | grep -q NO-CTTY \
  && ok "jail has no controlling tty (bwrap --new-session)" \
  || bad "jail has no controlling tty (bwrap --new-session)"

# ---- 5. no network ----------------------------------------------------------
J bash -c 'timeout 4 bash -c "exec 3<>/dev/tcp/1.1.1.1/443" 2>/dev/null' \
  && bad "no TCP to the internet" || ok "no TCP to the internet"
J getent hosts nixos.org >/dev/null 2>&1 \
  && bad "no DNS resolution" || ok "no DNS resolution"
[ "$(J sh -c 'ip -o link | wc -l')" = "1" ] \
  && ok "netns has only lo (no other interfaces)" || bad "netns has only lo"

# ---- 6. GPU + display --------------------------------------------------------
if [ "$REAL" = "1" ]; then
  J bash -c "[ -S /run/user/$(id -u)/wayland-1 ]" && ok "real wayland socket is there (real-display mode)" \
    || bad "real wayland socket is there (real-display mode)"
  J bash -c '[ -n "${DISPLAY:-}" ]' && ok "DISPLAY is set (real-display mode)" || bad "DISPLAY is set (real-display mode)"
else
  # headless: the socket exists per job, and WAYLAND_DISPLAY points at it
  J bash -c '[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]' \
    && ok "headless compositor socket is there" || bad "headless compositor socket is there"
  J bash -c '[ -z "${DISPLAY:-}" ]' \
    && ok "DISPLAY is unset (headless: no XWayland — see gpujail.sh)" \
    || bad "DISPLAY is unset (headless: no XWayland)"
  # the compositor is per job: two jobs must not see each other's sockets
  # (both run in their own jail /run tmpfs — trivially true, asserted anyway)
  J bash -c '[ "$(ls "$XDG_RUNTIME_DIR"/wayland-[0-9]* 2>/dev/null | wc -l)" -le 2 ]' \
    && ok "headless compositor is private to the job" || bad "headless compositor is private to the job"
fi
if [ -n "$EGLINFO" ]; then
  # eglinfo itself exits 1 when ANY enumerated EGL device fails (device #1 does,
  # a mesa dri2 screen); the property that matters is that EGL initialises on
  # the NVIDIA card — assert on the renderer string, not the exit code.
  J "$EGLINFO" -B 2>/dev/null | grep -q "NVIDIA GeForce" \
    && ok "EGL initialises on the NVIDIA GPU (eglinfo -B)" || bad "EGL initialises on the NVIDIA GPU (eglinfo -B)"
  # llvmpipe is always LISTED (Mesa's software device enumerates as device #1);
  # what matters is the vendor the platform picks
  J "$EGLINFO" -B 2>/dev/null | grep -q "EGL vendor string: NVIDIA" \
    && ok "EGL vendor on the wayland platform is NVIDIA (not llvmpipe)" \
    || bad "EGL vendor on the wayland platform is NVIDIA (not llvmpipe)"
else skip "EGL initialises (eglinfo -B) — mesa-demos not found"; fi
if [ -n "$VULKANINFO" ]; then
  if [ "$REAL" = "1" ]; then
    # with DISPLAY=:0 set, vulkaninfo's X11 WSI enumeration segfaults against
    # the on-demand XWayland; Vulkan itself is fine with the wayland display
    # (this is the real-display mode's quirk; headless has no DISPLAY at all)
    J env DISPLAY= "$VULKANINFO" --summary >/dev/null 2>&1 \
      && ok "Vulkan initialises (vulkaninfo --summary, X11 WSI off)" \
      || bad "Vulkan initialises (vulkaninfo --summary, X11 WSI off)"
  else
    J "$VULKANINFO" --summary >/dev/null 2>&1 && ok "Vulkan initialises (vulkaninfo --summary)" || bad "Vulkan initialises (vulkaninfo --summary)"
  fi
else skip "Vulkan initialises (vulkaninfo --summary) — vulkan-tools not found"; fi
if [ -n "$EGLGEARS" ]; then
  if [ "$REAL" = "1" ]; then
    J timeout 4 "$EGLGEARS" >/dev/null 2>&1; [ $? -eq 124 ] \
      && ok "eglgears_wayland runs on the real display (killed by timeout)" \
      || bad "eglgears_wayland runs on the real display"
  else
    # KNOWN LIMITATION, asserted so a fix shows up as a test change:
    # mesa-demos' eglut destroys a NULL wl_pointer when the seat has no
    # pointer (a headless seat has no input devices) — segfault, not a jail bug.
    J "$EGLGEARS" >/dev/null 2>&1; [ $? -eq 139 ] \
      && ok "eglgears_wayland hits the documented eglut empty-seat crash (139)" \
      || bad "eglgears_wayland empty-seat behaviour changed (was: segfault 139) — update this test"
  fi
else skip "eglgears_wayland — not found"; fi
rm -f "$FOREST/shot.png"
if [ "$REAL" = "1" ]; then
  J grim "$FOREST/shot.png" && [ "$(stat -c%s "$FOREST/shot.png" 2>/dev/null || echo 0)" -gt 500 ] \
    && ok "grim screenshot lands in the forest (real display)" || bad "grim screenshot lands in the forest (real display)"
else
  # a GL app (alacritty renders its terminal through OpenGL ES) + grim capture
  # against the job's own compositor; non-trivial size = actual content
  J bash -c 'timeout 4 alacritty -e bash -c "sleep 3" & sleep 2; grim "$0/shot.png"; wait' "$FOREST" \
    && [ "$(stat -c%s "$FOREST/shot.png" 2>/dev/null || echo 0)" -gt 5000 ] \
    && ok "GL app renders + grim captures it (headless compositor)" \
    || bad "GL app renders + grim captures it (headless compositor)"
fi

# ---- 7. scratch-home cleanup (R5's fixed bug) ---------------------------------
# The scratch home is mounted AT $HOME inside the jail, so its /tmp name is only
# observable from the host while the job runs: start the job in the background,
# spot the new /tmp/gpujail-home.*, wait for the job, assert it is gone.
bg_cleanup_check() { # name cmd...
  local name=$1; shift
  SECRETCANARY=$CANARY "$GJ" "$FOREST" -- "$@" >/dev/null 2>&1 &
  local wpid=$! s=""
  for i in $(seq 1 50); do
    s=$(ls -dt /tmp/gpujail-home.* 2>/dev/null | head -1)
    [ -n "$s" ] && break
    sleep 0.1
  done
  wait "$wpid"
  sleep 0.3
  [ -n "$s" ] && [ ! -e "$s" ] && ok "$name" || bad "$name (scratch=${s:-unseen})"
}
bg_cleanup_check "scratch home removed after a normal run"  bash -c 'sleep 1.2; echo ok'
bg_cleanup_check "scratch home removed after a failing run"  bash -c 'sleep 1.2; exit 9'

SECRETCANARY=$CANARY "$GJ" "$FOREST" -- sleep 30 >/dev/null 2>&1 &
wpid=$!
s=""
for i in $(seq 1 50); do
  s=$(ls -dt /tmp/gpujail-home.* 2>/dev/null | head -1)
  [ -n "$s" ] && break
  sleep 0.1
done
kill -TERM "$wpid" 2>/dev/null
wait "$wpid"; wrc=$?
sleep 0.5
if [ -n "$s" ] && [ ! -e "$s" ] && [ "$wrc" -eq 143 ]; then
  ok "SIGTERM to wrapper: kills job, cleans scratch, exit 143"
else
  bad "SIGTERM to wrapper: kills job, cleans scratch, exit 143 (scratch=$s rc=$wrc)"
fi
leaks_after=$(ls -d /tmp/gpujail-home.* 2>/dev/null | wc -l)
[ "$leaks_before" -eq "$leaks_after" ] && ok "no /tmp/gpujail-home.* leaks after the whole suite" \
  || bad "no /tmp/gpujail-home.* leaks after the whole suite ($leaks_before -> $leaks_after)"

# ---- 8. git / worktree .git ---------------------------------------------------
# The checkout's gitdir lives in ~/projects/repos/<r>/.git/worktrees — hidden by
# the /home tmpfs, so git cannot commit from the jail. Decision: jobs don't
# commit, they only write files.
REALWT=$here/.. # this checkout (bento) — a real worktree
J2() { SECRETCANARY=$CANARY "$GJ" "$REALWT" -- "$@"; }
J2 bash -c "[ ! -e /home/$USER/projects/repos ]" \
  && ok "worktree gitdir (~/projects/repos) invisible in jail" || bad "worktree gitdir invisible in jail"
J2 git status >/dev/null 2>&1 && bad "git status in a checkout fails (no gitdir)" || ok "git status in a checkout fails (no gitdir)"
J2 git commit -am x >/dev/null 2>&1 && bad "git commit from the jail is impossible" || ok "git commit from the jail is impossible"
J2 bash -c '[ -f .git ]' && ok "the checkout's .git gitfile itself is still there (harmless)" || bad "the checkout's .git gitfile itself is still there"

# ---- 8b. a checkout with a real .git DIRECTORY: overlayed by a tmpfs (M5) -----
# Worktrees have a .git FILE (harmless). A checkout that is a plain clone has
# a real .git dir — the heart of the repo — and the forest bind would expose
# it rw to the job. The jail overlays each with a tmpfs: writes stay in RAM,
# the real contents are invisible.
mkdir -p "$FOREST/repo2/.git"
echo "real gitdir content" > "$FOREST/repo2/.git/HEAD-marker"
J bash -c "echo pwn > '$FOREST/repo2/.git/pwned'" 2>/dev/null \
  && ok "a checkout's .git dir is writable in jail (tmpfs)" || bad "a checkout's .git dir is writable in jail (tmpfs)"
[ ! -e "$FOREST/repo2/.git/pwned" ] \
  && ok "writes into a checkout .git never reach the host (tmpfs overlay)" \
  || bad "a job wrote into a checkout's real .git"
J bash -c "[ ! -e '$FOREST/repo2/.git/HEAD-marker' ]" \
  && ok "the real .git contents are hidden in the jail" || bad "real .git contents visible in jail"

# ---- summary ------------------------------------------------------------
echo
echo "---- $pass passed, $fail failed, $skipped skipped (display: $([ "$REAL" = 1 ] && echo real || echo headless))"
[ "$fail" -eq 0 ]
