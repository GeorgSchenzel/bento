#!/usr/bin/env bash
# bento's VM tests: plan.md phases 1 and 2, on the host, under a scratch
# BENTO_HOME. Run with `just e2e`. Needs KVM, nix, network.
#
# It builds bento (`nix build .#bento`), writes a scratch instance config
# (prefix p0-, loopback 127.78.0.0/24, nix_port 17762), creates the boxes
# p0-a and p0-b, and removes them, the builder and every host process it
# started (by PID) on exit. The instance's store (a few GB) is kept for the
# next run unless E2E_CLEAN=1. It never touches the live boxes; it checks so
# by comparing the live msb processes and dirs before and after.
#
# Optional:
#   E2E_GITHUB_REPO=https://github.com/<you>/<repo>  clone + push a branch (and
#   E2E_GITLAB_REPO=https://gitlab.com/<you>/<repo>  delete it), with the tokens
#     in ~/.config/bento/secrets/{github,gitlab}
#   E2E_AGENTS=1   also build claude and opencode as tools and run --version
#   E2E_BENTO_HOME, E2E_NIX_PORT, E2E_LOOPBACK  override the scratch instance
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BH=${E2E_BENTO_HOME:-/tmp/bento-e2e}
PORT=${E2E_NIX_PORT:-17762}
NET=${E2E_LOOPBACK:-127.78.0.0/24}
H=$BH/host
export BENTO_HOME=$BH

for tool in python3 curl script git cmp; do
  if ! command -v $tool >/dev/null; then
    exec nix shell nixpkgs#python3 nixpkgs#curl nixpkgs#util-linux nixpkgs#git nixpkgs#diffutils \
      --command "$0" "$@"
  fi
done

PIDS=()
pass=0
failed=()
skipped=()
ok() { echo "ok    $1"; pass=$((pass + 1)); }
bad() { echo "FAIL  $1"; failed+=("$1"); }
skip() { echo "skip  $1"; skipped+=("$1"); }
check() { local what=$1; shift; if "$@"; then ok "$what"; else bad "$what"; fi; }
# expect_code <what> <code> <command…>: stdin from /dev/null
expect_code() {
  local what=$1 want=$2; shift 2
  "$@" >"$H/last.out" 2>"$H/last.err" </dev/null
  local got=$?
  if [ "$got" = "$want" ]; then ok "$what"; else bad "$what (exit $got, want $want: $(tail -c 300 "$H/last.err"))"; fi
}
section() { echo; echo "== $1"; }

echo "building bento..."
OUT=$(nix build "$ROOT#bento" --no-link --print-out-paths) || { echo "build failed"; exit 1; }
B=$OUT/bin/bento
bento() { "$B" "$@"; }
bx() { "$B" exec "$@" </dev/null; } # exec without stdin
out_of() { "$B" exec "$@" </dev/null 2>/dev/null; }
json_get() { python3 -c "import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))" "$1"; }

# --- the live instance, before -------------------------------------------
live_snapshot() {
  {
    ls -la --time-style=+%s "$HOME/.microsandbox" "$HOME/.microsandbox/sandboxes" \
      "$HOME/.local/share/bento" 2>/dev/null | grep -v ' \.\.$' # `..` is $HOME, ~/.local/share: anything touches them
    for p in /proc/[0-9]*; do
      local c
      c=$(tr '\0' ' ' <"$p/cmdline" 2>/dev/null) || continue
      case $c in *msb*|*bento*|*krun*) ;; *) continue ;; esac
      # ours: they carry the scratch MSB_HOME (or BENTO_HOME) in their env
      if tr '\0' '\n' <"$p/environ" 2>/dev/null | grep -qx "MSB_HOME=$BH/msb\|BENTO_HOME=$BH"; then continue; fi
      [ "${p#/proc/}" = $$ ] && continue
      case $c in *e2e/run.sh*|*"$BH"*) continue ;; esac
      echo "${p#/proc/} $c"
    done
  } | sort
}

cleanup() {
  section "cleanup"
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null; done
  for b in p0-a p0-b; do bento rm --force "$b" >/dev/null 2>&1 && echo "removed box $b"; done
  bento builder rm >/dev/null 2>&1 && echo "removed the builder"
  live_snapshot >"$H/live-after.txt"
  if diff -u "$H/live-before.txt" "$H/live-after.txt" >"$H/live.diff"; then
    ok "the live boxes and builder are untouched"
  else
    bad "the live instance changed (diff in $H/live.diff; a live box may have stopped on its own)"
  fi
  if [ "${E2E_CLEAN:-}" = 1 ]; then rm -rf "$BH"; echo "removed $BH"; fi
  echo
  echo "passed: $pass  failed: ${#failed[@]}  skipped: ${#skipped[@]}"
  for f in "${failed[@]}"; do echo "  FAIL $f"; done
  for s in "${skipped[@]}"; do echo "  skip $s"; done
  [ ${#failed[@]} = 0 ]
}

mkdir -p "$H"
live_snapshot >"$H/live-before.txt"
trap 'cleanup; exit $?' EXIT
trap 'exit 130' INT TERM

if (exec 3<>/dev/tcp/127.0.0.1/$PORT) 2>/dev/null; then
  echo "127.0.0.1:$PORT is busy; set E2E_NIX_PORT"; exit 1
fi

# --- the scratch instance's config ---------------------------------------
REV=$(python3 -c "import json; print(json.load(open('$ROOT/flake.lock'))['nodes']['nixpkgs']['locked']['rev'])")
mkdir -p "$H/ro" "$H/cpdir/sub"
echo ro-content >"$H/ro/file"
echo copy-1 >"$H/copy.txt"
printf 'secret-1\n' >"$H/secret"
echo A >"$H/cpdir/a"
echo B >"$H/cpdir/sub/b"
GIT=""
[ -f "$HOME/.config/bento/secrets/github" ] && GIT+="  github.com: ~/.config/bento/secrets/github"$'\n'
[ -f "$HOME/.config/bento/secrets/gitlab" ] && GIT+="  gitlab.com: ~/.config/bento/secrets/gitlab"$'\n'
TOOLS="  hello: github:NixOS/nixpkgs/$REV#hello"$'\n'
if [ "${E2E_AGENTS:-}" = 1 ]; then
  TOOLS+="  claude: github:NixOS/nixpkgs/$REV#claude-code"$'\n'
  TOOLS+="  opencode: github:NixOS/nixpkgs/$REV#opencode"$'\n'
fi
write_config() {
  cat >"$BH/config.yaml" <<EOF
instance: {prefix: p0-, loopback: $NET, nix_port: $PORT}
disk_gib: 8
channels:
  e2e: {socket: $H/echo.sock}
tools:
$TOOLS
files:
  - {guest: ~/.config/e2e-ro, host: $H/ro}
copy:
  - {guest: /etc/e2e-copy, host: $H/copy.txt}
env:
  E2E_ENV: one
$1
env_files:
  E2E_SECRET: $H/secret
${GIT:+git:
$GIT}
EOF
}
write_config ""

# the channel's host side: an echo server
cat >"$H/echo.py" <<'EOF'
import os, socket, sys, threading
p = sys.argv[1]
try: os.unlink(p)
except FileNotFoundError: pass
s = socket.socket(socket.AF_UNIX); s.bind(p); s.listen(64)
def serve(c):
    with c:
        while True:
            b = c.recv(1 << 16)
            if not b: return
            c.sendall(b)
while True:
    c, _ = s.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
EOF
python3 "$H/echo.py" "$H/echo.sock" &
PIDS+=($!)

# in the box: N clients, 16 MiB each way, no half-close
cat >"$H/client.py" <<'EOF'
import hashlib, os, socket, sys, threading
N = 16 << 20
def one(i, res):
    data = os.urandom(N)
    s = socket.socket(socket.AF_UNIX); s.connect("/run/bento/e2e.sock")
    t = threading.Thread(target=s.sendall, args=(data,)); t.start()
    got = bytearray()
    while len(got) < N:
        b = s.recv(1 << 20)
        if not b: break
        got += b
    t.join(); s.close()
    res[i] = bytes(got) == data
n = int(sys.argv[1]); res = [False] * n
ts = [threading.Thread(target=one, args=(i, res)) for i in range(n)]
[t.start() for t in ts]; [t.join() for t in ts]
print(sum(res)); sys.exit(0 if all(res) else 1)
EOF

# --- phase 1 ---------------------------------------------------------------
section "cli"
check "--version names contract 2" sh -c "'$B' --version | grep -q '(contract 2)'"
expect_code "a bad name is 120" 120 bento create Bad_Name
expect_code "t<digits> is 120" 120 bento create t12
expect_code "no such box is 121" 121 bento exec p0-none -- true
expect_code "cp with two box paths is 120" 120 bento cp a:/x b:/y

section "create"
expect_code "create p0-a" 0 bento create p0-a
check "ls shows p0-a running" sh -c "'$B' ls --json | grep -q '\"name\":\"p0-a\",\"state\":\"running\"'"
expect_code "create of an existing name is 123" 123 bento create p0-a
expect_code "docker answers right after create" 0 bx p0-a -- docker info
check "the box's builder is the instance's" sh -c "'$B' ls 2>&1 >/dev/null | grep -q '^builder: running'"
expect_code "create -e" 0 bento create p0-b -e BOXVAR=b1
check "create -e is set in the box" test "$(out_of p0-b -- printenv BOXVAR)" = b1
expect_code "create -e of bento's own name is 120" 120 bento create p0-c -e PATH=/x

section "exec"
head -c 16777216 /dev/urandom >"$H/blob"
check "exec -i --no-start: 16 MiB of stdin and stdout exact" \
  sh -c "'$B' exec p0-a -i --no-start -- cat <'$H/blob' | cmp -s - '$H/blob'"
check "stderr is byte-exact too" \
  sh -c "'$B' exec p0-a -i -- sh -c 'cat >&2' <'$H/blob' 2>&1 >/dev/null | cmp -s - '$H/blob'"
check "without -i stdin is empty" test "$(echo hi | "$B" exec p0-a -- sh -c 'cat | wc -c' 2>/dev/null)" = 0
expect_code "the command's exit code" 7 bx p0-a -- sh -c 'exit 7'
expect_code "a signal is 128+n" 137 bx p0-a -- sh -c 'kill -9 $$'
expect_code "not found is 127" 127 bx p0-a -- no-such-command-e2e
expect_code "-w relative is 120" 120 bx p0-a -w rel -- true
expect_code "-w missing is 125" 125 bx p0-a -w /no/such/dir -- true
check "-w sets the dir" test "$(out_of p0-a -w /etc -- pwd)" = /etc
check "the default dir is /workspace" test "$(out_of p0-a -- pwd)" = /workspace
expect_code "-t without a terminal is 120" 120 bx p0-a -t -- true
expect_code "-e of bento's own name is 120" 120 bx p0-a -e HOME=/ -- true
check "global env" test "$(out_of p0-a -- printenv E2E_ENV)" = one
check "env_files, trailing newline dropped" test "$(out_of p0-a -- printenv E2E_SECRET)" = secret-1
check "-e wins" test "$(out_of p0-a -e E2E_ENV=two -- printenv E2E_ENV)" = two
check "HOME is the host's" test "$(out_of p0-a -- printenv HOME)" = "$HOME"
mv "$H/secret" "$H/secret.away"
expect_code "an unreadable env_files file is 125" 125 bx p0-a -- true
mv "$H/secret.away" "$H/secret"

# the caller drops: SIGHUP reaches the command
"$B" exec p0-a -- sh -c 'trap "echo hup > /workspace/hup; exit 0" HUP; rm -f /workspace/hup; while :; do sleep 0.1; done' </dev/null >/dev/null 2>&1 &
EXEC_PID=$!
sleep 3
kill -9 $EXEC_PID 2>/dev/null; wait $EXEC_PID 2>/dev/null
sleep 2
check "a dropped connection hangs up the command" test "$(out_of p0-a -- cat /workspace/hup)" = hup

section "files, copy, tools"
check "files: read-only file content" test "$(out_of p0-a -- cat "$HOME/.config/e2e-ro/file")" = ro-content
expect_code "files: not writable" 1 bx p0-a -- sh -c "echo x > $HOME/.config/e2e-ro/file || exit 1"
check "copy: copied in" test "$(out_of p0-a -- cat /etc/e2e-copy)" = copy-1
check "tools: on PATH" sh -c "'$B' exec p0-a -- hello </dev/null | grep -q 'Hello, world'"
if [ "${E2E_AGENTS:-}" = 1 ]; then
  expect_code "claude runs" 0 bx p0-a -- claude --version
  expect_code "opencode runs" 0 bx p0-a -- opencode --version
else
  skip "claude and opencode as tools (E2E_AGENTS=1)"
fi

section "the data disk"
bx p0-a -- sh -c 'echo w > /workspace/w; echo h > ~/h; mkdir -p ~/.local/state/herdr-core; echo id1 > ~/.local/state/herdr-core/core.json; echo r > /etc/rootfs-marker; docker volume create e2evol' >/dev/null 2>&1
bx p0-a -- sh -c 'echo copy-edited > /etc/e2e-copy' >/dev/null 2>&1
echo copy-2 >"$H/copy.txt"
expect_code "stop" 0 bento stop p0-a
expect_code "stop of a stopped box is 0" 0 bento stop p0-a
check "ls shows p0-a stopped" sh -c "'$B' ls --json | grep -q '\"name\":\"p0-a\",\"state\":\"stopped\"'"
expect_code "exec --no-start on a stopped box is 122" 122 bx p0-a --no-start -- true
check "ports of a stopped box: not running, no addresses" \
  sh -c "'$B' ports p0-a --json | grep -q '\"running\":false'"
expect_code "start" 0 bento start p0-a
check "after stop/start: workspace, HOME, hc's state, docker" \
  test "$(out_of p0-a -- sh -c 'cat /workspace/w ~/h ~/.local/state/herdr-core/core.json; docker volume ls -q | grep -c e2evol')" = "w
h
id1
1"
check "after stop/start: the rootfs stays" test "$(out_of p0-a -- cat /etc/rootfs-marker)" = r
check "copy: re-copied at boot" test "$(out_of p0-a -- cat /etc/e2e-copy)" = copy-2

write_config "  E2E_ENV2: x"
ls_of() { "$B" ls --json | json_get "[(b['state'], b['config_changed']) for b in d['boxes'] if b['name']=='$1']"; }
check "a changed config shows as config_changed" test "$(ls_of p0-a)" = "[('running', True)]"
check "start on a running box warns about the config" sh -c "'$B' start p0-a 2>&1 | grep -q 'config changed'"
expect_code "restart recreates" 0 bento restart p0-a
check "after a recreate: workspace, HOME, hc's state, docker" \
  test "$(out_of p0-a -- sh -c 'cat /workspace/w ~/h ~/.local/state/herdr-core/core.json; docker volume ls -q | grep -c e2evol')" = "w
h
id1
1"
expect_code "after a recreate: the rootfs is reset" 1 bx p0-a -- test -e /etc/rootfs-marker
check "after a recreate: the new env" test "$(out_of p0-a -- printenv E2E_ENV2)" = x
check "config_changed is false again" test "$(ls_of p0-a)" = "[('running', False)]"

section "enter, stop under a command"
bento stop p0-a >/dev/null 2>&1
check "enter starts a stopped box" sh -c "printf 'echo entered-\$((40+2))\nexit\n' | script -qec '$B enter p0-a' /dev/null | grep -q entered-42"
"$B" exec p0-a -- sleep 1000 </dev/null >/dev/null 2>&1 &
EXEC_PID=$!
sleep 2
bento stop p0-a >/dev/null 2>&1
wait $EXEC_PID
code=$?
if [ $code = 129 ] || [ $code = 137 ]; then ok "stop ends a running exec with 128+n ($code)"; else bad "stop under an exec: exit $code"; fi
bento start p0-a >/dev/null 2>&1

# --- phase 2 ---------------------------------------------------------------
section "channels"
check "a channel: 16 MiB each way, 8 clients" sh -c "'$B' exec p0-a -i -- python3 - 8 <'$H/client.py' >/dev/null 2>&1"
bento restart p0-a >/dev/null 2>&1
check "a channel after a restart" sh -c "'$B' exec p0-a -i -- python3 - 1 <'$H/client.py' >/dev/null 2>&1"
kill "${PIDS[0]}" 2>/dev/null
sleep 0.5
check "no host listener: EOF before any reply" sh -c "'$B' exec p0-a -- python3 -c '
import socket; s=socket.socket(socket.AF_UNIX); s.connect(\"/run/bento/e2e.sock\"); s.sendall(b\"x\"); print(repr(s.recv(10)))' 2>/dev/null | grep -qx \"b''\""
python3 "$H/echo.py" "$H/echo.sock" &
PIDS+=($!)

section "ports"
for b in p0-a p0-b; do
  bx $b -- sh -c "mkdir -p /workspace/web && echo served-by-$b > /workspace/web/index.html && printf 'ports: [5173]\n' > /workspace/bento.yaml" >/dev/null 2>&1
done
check "a new port is pending" sh -c "'$B' ports p0-a --json | grep -q '\"guest\":5173,\"address\":null,\"state\":\"pending\"'"
check "start on a running box warns about pending ports" sh -c "'$B' start p0-a 2>&1 | grep -q pending"
for b in p0-a p0-b; do
  expect_code "restart $b publishes its ports" 0 bento restart $b
  bx $b -- sh -c 'cd /workspace/web && setsid -f python3 -m http.server 5173 --bind 0.0.0.0 >/dev/null 2>&1' >/dev/null 2>&1
done
sleep 2
addr() { "$B" ports "$1" --json | json_get "[p['address'] for p in d['ports'] if p['guest']==$2][0]"; }
A=$(addr p0-a 5173); Bb=$(addr p0-b 5173)
check "two boxes serve 5173 at once ($A, $Bb)" sh -c "test '$A' != '$Bb' && curl -s http://$A/ | grep -q served-by-p0-a && curl -s http://$Bb/ | grep -q served-by-p0-b"
check "after a restart: the workspace is intact" test "$(out_of p0-a -- cat /workspace/w)" = w

IPB=${Bb%:*}
python3 -c "import socket,time; s=socket.socket(); s.bind(('$IPB', 8081)); s.listen(); time.sleep(900)" &
PIDS+=($!)
sleep 0.5
bx p0-b -- sh -c "printf 'ports: [5173, 8081]\n' > /workspace/bento.yaml" >/dev/null 2>&1
check "a busy host port warns, doesn't fail" sh -c "'$B' restart p0-b 2>&1 | grep -q busy"
check "the busy port shows as busy" sh -c "'$B' ports p0-b --json | grep -q '\"guest\":8081,\"address\":null,\"state\":\"busy\"'"

bx p0-a -- sh -c "git init -q /workspace/repo && printf 'ports: [9000]\nextra: 1\n' > /workspace/repo/bento.yaml && mkdir -p /workspace/notrepo && printf 'ports: [9001]\n' > /workspace/notrepo/bento.yaml" >/dev/null 2>&1
check "a repo cloned into a running box: its port is pending" sh -c "'$B' ports p0-a --json 2>/dev/null | grep -q '\"guest\":9000,\"address\":null,\"state\":\"pending\"'"
check "an unknown key warns" sh -c "'$B' ports p0-a 2>&1 >/dev/null | grep -q 'unknown key .extra.'"
check "a dir without .git doesn't count" sh -c "! '$B' ports p0-a --json | grep -q 9001"
expect_code "restart" 0 bento restart p0-a
check "... and published after the restart" sh -c "'$B' ports p0-a --json | grep -q '\"guest\":9000,\"address\":\"127.*:9000\",\"state\":\"published\"'"
check "... with the data disk intact" test "$(out_of p0-a -- cat /workspace/w)" = w

section "cp"
rm -rf "$H/out1" "$H/out2" "$H/c.txt" # a previous run's copies: cp into an existing dir nests
expect_code "cp a dir in" 0 bento cp "$H/cpdir" p0-a:/workspace/
check "... lands as /workspace/cpdir" test "$(out_of p0-a -- cat /workspace/cpdir/sub/b)" = B
expect_code "cp a dir out to a new path" 0 bento cp p0-a:/workspace/cpdir "$H/out1"
check "... its contents land there" test "$(cat "$H/out1/sub/b")" = B
expect_code "cp a file out" 0 bento cp p0-a:/etc/e2e-copy "$H/c.txt"
check "... exact" test "$(cat "$H/c.txt")" = copy-2
bx p0-a -- sh -c 'mkdir -p /workspace/evil && ln -sfn /etc /workspace/evil/l && ln -sfn / /workspace/evil/root' >/dev/null 2>&1
expect_code "cp out a dir with links" 0 bento cp p0-a:/workspace/evil "$H/out2"
check "... links stay links, never followed" sh -c "test -L '$H/out2/l' && test -L '$H/out2/root' && test \$(ls -A '$H/out2' | wc -l) = 2"
expect_code "cp of a missing source is 125" 125 bento cp p0-a:/no/such "$H/x"
expect_code "cp into a missing parent is 125" 125 bento cp "$H/cpdir/a" p0-a:/no/such/dir/a

section "git"
gitpush() { # <repo url> <what>
  bx p0-a -- sh -c 'cd /workspace && rm -rf gp && git clone -q "$1" gp && cd gp && git checkout -q -b "$2" && git -c user.name=bento-e2e -c user.email=e2e@bento commit -q --allow-empty -m "bento e2e" && git push -q origin "$2" && git push -q origin --delete "$2"' sh "$1" "bento-e2e-$$"
}
if [ -n "${E2E_GITHUB_REPO:-}" ] && [ -f "$HOME/.config/bento/secrets/github" ]; then
  expect_code "GitHub: clone, push a branch, delete it" 0 gitpush "$E2E_GITHUB_REPO"
else
  skip "GitHub clone and push (E2E_GITHUB_REPO and ~/.config/bento/secrets/github)"
fi
if [ -n "${E2E_GITLAB_REPO:-}" ] && [ -f "$HOME/.config/bento/secrets/gitlab" ]; then
  expect_code "GitLab: clone, push a branch, delete it" 0 gitpush "$E2E_GITLAB_REPO"
else
  skip "GitLab clone and push (E2E_GITLAB_REPO and ~/.config/bento/secrets/gitlab)"
fi

section "rm"
expect_code "rm" 0 bento rm p0-b
check "... the box and its disk are gone" sh -c "! '$B' ls --json | grep -q p0-b && test ! -e '$BH/boxes/p0-b'"
expect_code "rm of a missing box is 121" 121 bento rm p0-b
expect_code "rm --force of a missing box is 0" 0 bento rm --force p0-b
