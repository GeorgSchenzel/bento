#!/usr/bin/env python3
"""forest -- the box-side client for the bento bridge (spike, decisions.md §2).

Talks TCP JSON lines to `bento serve` on the host (decisions.md §1): one
connection per invocation, `hello` first, then one request per command.
Identity is the per-box token from the environment; fields naming a forest
are never sent.

Environment:
  BENTO_BRIDGE            host:port of the bridge (set by bento)
  BENTO_BRIDGE_TOKEN      per-box token (set by bento)
  FOREST_CLIENT_WORKSPACE test-only override of the /workspace prefix
  FOREST_CLIENT_POLL      test-only gpu poll interval in seconds (default 1)

Exit codes: 0 ok; 1 error (bad use, bridge error); 2 forest refused;
3 bridge down/unreachable; 124 gpu job timed out; otherwise the gpu job's
own exit code.
"""
import argparse
import json
import os
import shlex
import socket
import sys
import time

VERSION = "1.0"
CONNECT_TIMEOUT = 5.0
REPLY_TIMEOUT = 120.0  # forest.new can be slow (box create, first agent)
WORKSPACE = os.environ.get("FOREST_CLIENT_WORKSPACE", "/workspace")
POLL_INTERVAL = float(os.environ.get("FOREST_CLIENT_POLL", "1"))


class BridgeDown(Exception):
    """Transport-level failure; the bridge is not usable (§1: connect() lies)."""


class BridgeError(Exception):
    """The bridge replied {"ok": false, ...}."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.message = message


class Bridge:
    """One TCP connection, JSON lines, hello first (§1)."""

    def __init__(self, host, port, token):
        self.host, self.port, self.token = host, port, token
        self.addr = f"{host}:{port}"
        self.sock = None
        self.rfile = None

    def _down(self, why):
        raise BridgeDown(why)

    def connect(self):
        try:
            self.sock = socket.create_connection((self.host, self.port), CONNECT_TIMEOUT)
            self.rfile = self.sock.makefile("rb")
        except OSError as e:
            self._down(str(e))
        # First request must be hello; identity comes only from the token.
        self.request({"op": "hello", "token": self.token,
                      "client": f"forest-client/{VERSION}"})

    def close(self):
        for f in (self.rfile, self.sock):
            try:
                if f:
                    f.close()
            except OSError:
                pass

    def request(self, req, timeout=REPLY_TIMEOUT):
        try:
            self.sock.settimeout(timeout)
            self.sock.sendall((json.dumps(req) + "\n").encode())
            line = self.rfile.readline()
        except OSError as e:  # includes timeout, ECONNRESET, lost connection
            self._down(str(e))
        if not line:
            self._down("connection closed")
        try:
            reply = json.loads(line)
        except ValueError:
            raise BridgeError("failed", f"unparseable bridge reply: {line[:120]!r}")
        if reply.get("ok") is not True:
            err = reply.get("error") or {}
            raise BridgeError(err.get("code", "?"), err.get("message", str(reply)))
        return reply.get("result", {})


# ---------- human output (short); --json prints the raw result instead ----------

def human_ls(r):
    print(f"host repos: {' '.join(r.get('repos') or []) or '-'}")
    f = r.get("forest") or {}
    name = f.get("name") or "(unnamed)"
    print(f"forest {name} branch {f.get('branch') or '-'}: "
          f"{' '.join(f.get('repos') or []) or '-'}")


def human_add(r):
    print(f"added: {' '.join(r.get('added') or []) or '-'}")
    if r.get("restart_needed"):
        print(f"restart needed: {r.get('hint') or 'restart this box to see the new repos'}")


def human_name(r):
    print(f"branch renamed: {r.get('old')} -> {r.get('new')}")
    if r.get("hint"):
        print(r["hint"])


def human_new(r):
    print(f"new forest {r.get('name')} at {r.get('path')}")
    if r.get("terminal"):
        print(f"box {r.get('box')} machine {r.get('machine')} "
              f"lead agent terminal: {r.get('terminal')}")


def human_job(j):
    parts = [f"job {j.get('id')}: {j.get('state')}"]
    if j.get("exit") is not None:
        parts.append(f"exit {j['exit']}")
    if j.get("timed_out"):
        parts.append("timed out")
    if j.get("queued_before") is not None:
        parts.append(f"({j['queued_before']} before you)")
    if j.get("dir"):
        parts.append(j["dir"])
    print(" ".join(parts))


def human_gpu_ls(r):
    jobs = r.get("jobs") or []
    for j in jobs:
        human_job(j)
    if not jobs:
        print("no gpu jobs")


# ---------- commands ----------

def read_brief(spec):
    if spec == "-":
        return sys.stdin.read()
    try:
        with open(spec) as f:
            return f.read()
    except OSError as e:
        die(f"cannot read brief {spec!r}: {e}")


def gpu_run(args, br):
    ws = os.path.realpath(WORKSPACE)
    cwd = os.path.realpath(args.cwd or os.getcwd())
    rel = os.path.relpath(cwd, ws)
    if rel == "." or rel.startswith(".."):
        die(f"gpu run: --cwd must be under {ws}/<checkout> (got {args.cwd or cwd})")
    if args.timeout is not None and not 1 <= args.timeout <= 3600:
        die("gpu run: --timeout must be between 1 and 3600 s")
    # One arg = a shell snippet, sent as-is; several = argv, joined (§2).
    cmd = args.cmd[0] if len(args.cmd) == 1 else shlex.join(args.cmd)
    os.sync()  # box -> host write lag before the host reads the tree (§6)
    req = {"op": "gpu.submit", "cwd": rel.replace(os.sep, "/"), "cmd": cmd}
    if args.timeout is not None:
        req["timeout_s"] = args.timeout
    sub = br.request(req)
    jid, jdir = sub.get("id"), sub.get("dir", "")
    if args.no_wait:
        if args.json:
            print(json.dumps(sub))
        else:
            print(f"job {jid} submitted")
            print(f"dir {jdir}")
        return 0
    seen = None
    while True:
        st = br.request({"op": "gpu.status", "id": jid})
        if st.get("state") == "done":
            break
        label = (f"queued ({st.get('queued_before', '?')} before you)"
                 if st.get("state") == "queued" else st.get("state", "?"))
        if label != seen:  # progress to stderr, once per change
            print(label, file=sys.stderr, flush=True)
            seen = label
        time.sleep(POLL_INTERVAL)
    if args.json:
        print(json.dumps(st))  # the raw final status (has tails + exit)
        return 124 if st.get("timed_out") else _exit_code(st)
    sys.stdout.buffer.write(_tail_or_file(st, "stdout_tail", jdir, "stdout"))
    sys.stdout.buffer.flush()
    sys.stderr.buffer.write(_tail_or_file(st, "stderr_tail", jdir, "stderr"))
    sys.stderr.buffer.flush()
    if st.get("timed_out"):
        print("forest: gpu job timed out", file=sys.stderr)
        return 124
    return _exit_code(st)


def _exit_code(st):
    return st["exit"] if isinstance(st.get("exit"), int) else 1


def _tail_or_file(st, key, jdir, name):
    """Prefer the status tails; else read the file in the returned dir (§6)."""
    tail = st.get(key)
    if tail is not None:
        return tail.encode()
    try:
        with open(os.path.join(jdir, name), "rb") as f:
            return f.read()
    except OSError:
        return b""


def dispatch(args, br):
    if args.cmd == "ls":
        return simple(br, {"op": "forest.ls"}, human_ls, args)
    if args.cmd == "add":
        return simple(br, {"op": "forest.add", "repos": args.repo}, human_add, args)
    if args.cmd == "name":
        return simple(br, {"op": "forest.name", "branch": args.branch}, human_name, args)
    if args.cmd == "new":
        req = {"op": "forest.new", "name": args.name, "repos": args.repo}
        if args.from_ is not None:
            req["from"] = args.from_
        if args.brief is not None:
            req["brief"] = read_brief(args.brief)
        return simple(br, req, human_new, args)
    if args.gcmd == "run":
        return gpu_run(args, br)
    if args.gcmd == "status":
        return simple(br, {"op": "gpu.status", "id": args.id}, human_job, args)
    return simple(br, {"op": "gpu.ls"}, human_gpu_ls, args)


def simple(br, req, human, args):
    r = br.request(req)
    if args.json:
        print(json.dumps(r))
    else:
        human(r)
    return 0


def build_parser():
    # --json is accepted before the subcommand and on each subcommand; the
    # subparser default is SUPPRESS so it never overwrites the top-level flag.
    p = argparse.ArgumentParser(prog="forest", description="forest client (bento bridge)",
                                 epilog="Only inside a bento box; on the host use the real forest.")
    p.add_argument("--json", action="store_true", default=False,
                   help="print the raw bridge result object")
    sub = p.add_subparsers(dest="cmd", required=True)

    def sp(*a, **kw):
        s = sub.add_parser(*a, **kw)
        s.add_argument("--json", action="store_true", default=argparse.SUPPRESS,
                       help="print the raw bridge result object")
        return s

    sp("ls", help="repos available on the host + this forest")
    a = sp("add", help="add repos to this forest (needs a box restart)")
    a.add_argument("repo", nargs="+")
    n = sp("name", help="name this forest's work (renames the branch)")
    n.add_argument("branch")
    w = sp("new", help="new forest on the host; --brief starts a lead agent there")
    w.add_argument("name")
    w.add_argument("repo", nargs="+")
    w.add_argument("--from", dest="from_", metavar="BRANCH", help="base branch")
    w.add_argument("--brief", metavar="FILE|-", help="brief for the lead agent")
    g = sub.add_parser("gpu", help="host GPU jobs (jailed, one at a time)")
    g.add_argument("--json", action="store_true", default=argparse.SUPPRESS,
                   help=argparse.SUPPRESS)
    gs = g.add_subparsers(dest="gcmd", required=True)

    def gsp(*a, **kw):
        s = gs.add_parser(*a, **kw)
        s.add_argument("--json", action="store_true", default=argparse.SUPPRESS,
                       help=argparse.SUPPRESS)
        return s

    r = gsp("run", help="queue a job; waits, prints its output, exits with its code")
    r.add_argument("--timeout", type=int, help="job timeout in seconds (default 600, max 3600)")
    r.add_argument("--no-wait", action="store_true", help="submit only; print id and dir")
    r.add_argument("--cwd", help="job cwd (default: current dir; under /workspace/<checkout>)")
    r.add_argument("cmd", nargs="+", metavar="cmd", help="after --: one arg = shell snippet, several = argv")
    st = gsp("status", help="show one job")
    st.add_argument("id")
    gsp("ls", help="list this forest's jobs")
    return p


def die(msg, code=1):
    print(f"forest: {msg}", file=sys.stderr)
    sys.exit(code)


def main(argv):
    args = build_parser().parse_args(argv)
    token = os.environ.get("BENTO_BRIDGE_TOKEN", "")
    if not token:
        die("only inside a bento box; on the host use the real forest")
    addr = os.environ.get("BENTO_BRIDGE", "")
    host, _, port = addr.rpartition(":")
    if not host or not port.isdigit():
        die(f"bad BENTO_BRIDGE {addr!r} (want host:port); only inside a bento box")
    br = Bridge(host.strip("[]"), int(port), token)
    try:
        br.connect()
        return dispatch(args, br)
    except BridgeDown as e:
        print(f"forest: bridge not reachable at {br.addr} ({e}): "
              f"is `bento serve` running on the host?", file=sys.stderr)
        return 3
    except BridgeError as e:
        print(f"forest: {e.message}", file=sys.stderr)
        return 2 if e.code == "refused" else 1
    finally:
        br.close()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
