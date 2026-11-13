"""Unit tests for forest-client.py.

Run:  python3 -m unittest test_forest_client         (from bento/tools/)
      python3 -m unittest discover -s bento/tools    (from a checkout root)

The client is exercised as a real subprocess against a fake bridge
(a threaded TCP server speaking decisions.md §1), so exit codes,
stdout and stderr are all tested end to end. FOREST_CLIENT_WORKSPACE
points the /workspace prefix at a temp dir; FOREST_CLIENT_POLL makes
gpu polling fast.
"""
import importlib.util
import json
import os
import shlex
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
CLIENT = os.path.join(HERE, "forest-client.py")

TOKEN = "abcd" * 8
BAD_TOKEN = "dead" * 8
JOB_ID = "20260101-120000-cafe"


def load_client():
    spec = importlib.util.spec_from_file_location("forest_client", CLIENT)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


class Reject(Exception):
    """Fake bridge error reply."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code, self.message = code, message


class FakeBridge(threading.Thread):
    """§1 fake: hello checks the token, then ops go to self.handler.

    handler(op, req) -> result dict, or raise Reject(code, message).
    self.ops records (op, req) in arrival order, hello included.
    """

    daemon = True

    def __init__(self, handler, token=TOKEN):
        super().__init__()
        self.handler = handler
        self.token = token
        self.ops = []
        self.srv = socket.socket()
        self.srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.srv.bind(("127.0.0.1", 0))
        self.srv.listen(8)
        self.port = self.srv.getsockname()[1]

    def start(self):
        super().start()
        return self

    def run(self):
        while True:
            try:
                conn, _ = self.srv.accept()
            except OSError:
                return
            threading.Thread(target=self.serve, args=(conn,), daemon=True).start()

    def serve(self, conn):
        f = conn.makefile("rb")
        helloed = False
        try:
            for raw in f:
                try:
                    req = json.loads(raw)
                except ValueError:
                    self._reply(conn, Reject("bad_request", "not JSON"))
                    continue
                op = req.get("op")
                if not helloed:
                    if op != "hello":
                        self._reply(conn, Reject("hello_required", "hello first"))
                        continue
                    if req.get("token") != self.token:
                        self._reply(conn, Reject("unknown_token", "unknown token"))
                        continue
                    helloed = True
                    self.ops.append((op, req))
                    self._reply(conn, {"box": "bento-box", "path": "/host/forest",
                                       "forest": "f1", "workforest": True})
                    continue
                self.ops.append((op, req))
                try:
                    self._reply(conn, self.handler(op, req))
                except Reject as e:
                    self._reply(conn, e)
                except Exception as e:  # handler bug -> error reply, not a hang
                    self._reply(conn, Reject("failed", f"fake: {e}"))
        except OSError:
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass

    @staticmethod
    def _reply(conn, r):
        if isinstance(r, Reject):
            obj = {"ok": False, "error": {"code": r.code, "message": r.message}}
        else:
            obj = {"ok": True, "result": r}
        conn.sendall((json.dumps(obj) + "\n").encode())


def canned(op, req):
    """Default handler: plausible §1 replies for every op."""
    if op == "forest.ls":
        return {"repos": ["bento", "forest", "ois"],
                "forest": {"name": "f1", "path": "/host/forest",
                           "branch": "spike/x", "repos": ["bento"]}}
    if op == "forest.add":
        return {"added": req["repos"], "restart_needed": True,
                "hint": "restart the box to pick up new repos"}
    if op == "forest.name":
        return {"old": "spike/x", "new": req["branch"], "hint": "dir not renamed"}
    if op == "forest.new":
        r = {"name": req["name"], "path": f"/host/forests/{req['name']}"}
        if "brief" in req:
            r.update({"box": f"bento-forest_{req['name']}", "machine": req["name"],
                      "terminal": f"{req['name']}/t1"})
        return r
    if op == "gpu.submit":
        return {"id": JOB_ID, "dir": f"/workspace/bento/.gpu-jobs/{JOB_ID}"}
    if op == "gpu.status":
        return {"id": req["id"], "state": "done", "exit": 0, "dir": "..."}
    if op == "gpu.ls":
        return {"jobs": []}
    raise Reject("unknown_op", op)


class ClientCase(unittest.TestCase):
    def setUp(self):
        self.ws = tempfile.TemporaryDirectory()
        os.makedirs(os.path.join(self.ws.name, "bento"))  # a checkout dir
        self.bridge = FakeBridge(canned).start()

    def tearDown(self):
        self.ws.cleanup()
        self.bridge.srv.close()

    def forest(self, *args, token=TOKEN, port=None, stdin="", env_extra=None,
               cwd=None):
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": self.ws.name,
            "BENTO_BRIDGE": f"127.0.0.1:{port or self.bridge.port}",
            "BENTO_BRIDGE_TOKEN": token,
            "FOREST_CLIENT_WORKSPACE": self.ws.name,
            "FOREST_CLIENT_POLL": "0.02",
        }
        if env_extra is not None:
            env.update(env_extra)
        return subprocess.run(
            [sys.executable, CLIENT, *args], env=env, input=stdin,
            capture_output=True, text=True, timeout=60, cwd=cwd)

    # -- basics -------------------------------------------------------------

    def test_refuses_without_token(self):
        p = self.forest("ls", token="", env_extra={"BENTO_BRIDGE_TOKEN": ""})
        self.assertEqual(p.returncode, 1)
        self.assertIn("only inside a bento box", p.stderr)
        self.assertIn("real forest", p.stderr)

    def test_hello_first_with_token(self):
        self.forest("ls")
        ops = [op for op, _ in self.bridge.ops]
        self.assertEqual(ops[0], "hello")
        self.assertEqual(self.bridge.ops[0][1]["token"], TOKEN)
        self.assertIn("forest-client/", self.bridge.ops[0][1]["client"])

    def test_bad_token(self):
        p = self.forest("ls", token=BAD_TOKEN)
        self.assertEqual(p.returncode, 1)
        self.assertIn("unknown token", p.stderr)

    # -- commands -------------------------------------------------------

    def test_ls(self):
        p = self.forest("ls")
        self.assertEqual(p.returncode, 0)
        self.assertIn("host repos: bento forest ois", p.stdout)
        self.assertIn("forest f1 branch spike/x: bento", p.stdout)

    def test_ls_json(self):
        p = self.forest("ls", "--json")
        self.assertEqual(p.returncode, 0)
        self.assertEqual(json.loads(p.stdout)["forest"]["branch"], "spike/x")

    def test_add(self):
        p = self.forest("add", "ois")
        self.assertEqual(p.returncode, 0)
        self.assertIn("added: ois", p.stdout)
        self.assertIn("restart needed", p.stdout)
        self.assertEqual(self.bridge.ops[-1][1]["repos"], ["ois"])

    def test_name(self):
        p = self.forest("name", "spike/new-name")
        self.assertEqual(p.returncode, 0)
        self.assertIn("spike/x -> spike/new-name", p.stdout)
        self.assertEqual(self.bridge.ops[-1][1], {"op": "forest.name",
                                                  "branch": "spike/new-name"})

    def test_new(self):
        p = self.forest("new", "demo", "bento", "ois", "--from", "main")
        self.assertEqual(p.returncode, 0)
        self.assertIn("new forest demo", p.stdout)
        req = self.bridge.ops[-1][1]
        self.assertEqual(req["name"], "demo")
        self.assertEqual(req["repos"], ["bento", "ois"])
        self.assertEqual(req["from"], "main")
        self.assertNotIn("brief", req)
        self.assertNotIn("lead agent", p.stdout)

    def test_new_with_brief_file(self):
        brief = os.path.join(self.ws.name, "brief.md")
        with open(brief, "w") as f:
            f.write("go west")
        p = self.forest("new", "demo", "bento", "--brief", brief)
        self.assertEqual(p.returncode, 0)
        self.assertEqual(self.bridge.ops[-1][1]["brief"], "go west")
        self.assertIn("lead agent terminal: demo/t1", p.stdout)

    def test_new_with_brief_stdin(self):
        p = self.forest("new", "demo", "bento", "--brief", "-", stdin="via stdin\n")
        self.assertEqual(p.returncode, 0)
        self.assertEqual(self.bridge.ops[-1][1]["brief"], "via stdin\n")

    # -- error mapping ----------------------------------------------------

    def test_refused_maps_to_2(self):
        def h(op, req):
            raise Reject("refused", "branch exists")
        self.bridge.handler = h
        p = self.forest("name", "taken")
        self.assertEqual(p.returncode, 2)
        self.assertIn("branch exists", p.stderr)

    def test_other_error_maps_to_1(self):
        def h(op, req):
            raise Reject("not_a_forest", "this box has no forest")
        self.bridge.handler = h
        p = self.forest("ls")
        self.assertEqual(p.returncode, 1)
        self.assertIn("no forest", p.stderr)

    # -- bridge down -------------------------------------------------------

    def test_bridge_down_port_nobody_listens(self):
        # grab a free port, close it again -> nothing listens there
        s = socket.socket()
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
        s.close()
        p = self.forest("ls", port=port)
        self.assertEqual(p.returncode, 3)
        self.assertIn("bridge not reachable", p.stderr)
        self.assertIn("bento serve", p.stderr)

    def test_bridge_down_listener_closes_immediately(self):
        srv = socket.socket()
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", 0))
        srv.listen(4)
        port = srv.getsockname()[1]

        def killer():
            while True:
                try:
                    conn, _ = srv.accept()
                    conn.close()  # accept then drop: in a box, connect() lies
                except OSError:
                    return

        threading.Thread(target=killer, daemon=True).start()
        p = self.forest("ls", port=port)
        self.assertEqual(p.returncode, 3)
        self.assertIn("bento serve", p.stderr)
        srv.close()

    # -- gpu ---------------------------------------------------------------

    def job_dir(self):
        d = os.path.join(self.ws.name, "bento", ".gpu-jobs", JOB_ID)
        os.makedirs(d, exist_ok=True)
        return d

    def test_gpu_run_happy_path_from_files(self):
        d = self.job_dir()
        with open(os.path.join(d, "stdout"), "w") as f:
            f.write("job says hi\n")
        with open(os.path.join(d, "stderr"), "w") as f:
            f.write("warn line\n")
        with open(os.path.join(d, "exit"), "w") as f:
            f.write("7")
        states = iter(["queued", "queued", "running", "done"])

        def h(op, req):
            if op == "gpu.submit":
                return {"id": JOB_ID, "dir": d}
            if op == "gpu.status":
                s = next(states)
                r = {"id": JOB_ID, "state": s, "dir": d}
                if s == "queued":
                    r["queued_before"] = 2
                if s == "done":
                    r["exit"] = 7
                return r
            return canned(op, req)

        self.bridge.handler = h
        cwd = os.path.join(self.ws.name, "bento")
        p = self.forest("gpu", "run", "--cwd", cwd, "--", "just", "run", "game",
                        cwd=cwd)
        self.assertEqual(p.returncode, 7)
        self.assertEqual(p.stdout, "job says hi\n")
        self.assertIn("job says hi", p.stdout)
        self.assertIn("warn line", p.stderr)
        self.assertIn("queued (2 before you)\n", p.stderr)
        self.assertIn("running\n", p.stderr)
        self.assertEqual(p.stderr.count("queued"), 1)  # once per change
        self.assertEqual(p.stderr.count("running"), 1)
        sub = [req for op, req in self.bridge.ops if op == "gpu.submit"][0]
        self.assertEqual(sub["cwd"], "bento")  # relative to /workspace
        self.assertEqual(sub["cmd"], shlex.join(["just", "run", "game"]))

    def test_gpu_run_one_arg_sent_as_is(self):
        self.bridge.handler = lambda op, req: (
            {"id": JOB_ID, "state": "done", "exit": 0, "dir": self.job_dir()}
            if op == "gpu.status" else canned(op, req))
        cwd = os.path.join(self.ws.name, "bento")
        # no --cwd: defaults to the current dir, sent relative to /workspace
        p = self.forest("gpu", "run", "--",
                        "just run game && echo done", cwd=cwd)
        self.assertEqual(p.returncode, 0)
        sub = [req for op, req in self.bridge.ops if op == "gpu.submit"][0]
        self.assertEqual(sub["cmd"], "just run game && echo done")

    def test_gpu_run_prefers_tails_and_times_out(self):
        d = self.job_dir()
        with open(os.path.join(d, "stdout"), "w") as f:
            f.write("stale file content\n")

        def h(op, req):
            if op == "gpu.status":
                return {"id": JOB_ID, "state": "done", "exit": 5,
                        "timed_out": True, "dir": d,
                        "stdout_tail": "tail-out\n", "stderr_tail": "tail-err\n"}
            return canned(op, req)

        self.bridge.handler = h
        cwd = os.path.join(self.ws.name, "bento")
        p = self.forest("gpu", "run", "--cwd", cwd, "--", "sleep 999", cwd=cwd)
        self.assertEqual(p.returncode, 124)
        self.assertEqual(p.stdout, "tail-out\n")  # tail wins over the file
        self.assertIn("tail-err", p.stderr)
        self.assertNotIn("stale file content", p.stdout)
        self.assertIn("timed out", p.stderr)

    def test_gpu_run_sends_timeout_and_no_wait(self):
        d = self.job_dir()
        p = self.forest("gpu", "run", "--timeout", "60", "--no-wait", "--cwd", d,
                        "--", "true")
        self.assertEqual(p.returncode, 0)
        self.assertIn(JOB_ID, p.stdout)
        self.assertIn(".gpu-jobs", p.stdout)
        ops = [op for op, _ in self.bridge.ops]
        self.assertEqual(ops, ["hello", "gpu.submit"])  # no polling
        sub = [req for op, req in self.bridge.ops if op == "gpu.submit"][0]
        self.assertEqual(sub["timeout_s"], 60)
        self.assertEqual(sub["cwd"], "bento/.gpu-jobs/" + JOB_ID)

    def test_gpu_run_rejects_cwd_outside_workspace(self):
        p = self.forest("gpu", "run", "--cwd", "/tmp", "--", "true")
        self.assertEqual(p.returncode, 1)
        self.assertIn("must be under", p.stderr)
        self.assertEqual([op for op, _ in self.bridge.ops], ["hello"])

    def test_gpu_run_rejects_bad_timeout(self):
        p = self.forest("gpu", "run", "--timeout", "9999", "--", "true",
                        cwd=os.path.join(self.ws.name, "bento"))
        self.assertEqual(p.returncode, 1)
        self.assertIn("3600", p.stderr)

    def test_gpu_status_and_ls(self):
        def h(op, req):
            if op == "gpu.status":
                return {"id": req["id"], "state": "done", "exit": 3, "dir": "d"}
            if op == "gpu.ls":
                return {"jobs": [{"id": JOB_ID, "state": "queued",
                                  "queued_before": 1, "dir": "d"}]}
            return canned(op, req)

        self.bridge.handler = h
        p = self.forest("gpu", "status", JOB_ID)
        self.assertEqual(p.returncode, 0)
        self.assertIn(f"job {JOB_ID}: done exit 3", p.stdout)
        p = self.forest("gpu", "ls")
        self.assertEqual(p.returncode, 0)
        self.assertIn(f"job {JOB_ID}: queued (1 before you)", p.stdout)

    def test_gpu_run_json_prints_status_only(self):
        d = self.job_dir()

        def h(op, req):
            if op == "gpu.status":
                return {"id": JOB_ID, "state": "done", "exit": 9, "dir": d,
                        "stdout_tail": "out-tail\n", "stderr_tail": ""}
            return canned(op, req)

        self.bridge.handler = h
        p = self.forest("gpu", "run", "--json", "--cwd",
                        os.path.join(self.ws.name, "bento"), "--", "true")
        self.assertEqual(p.returncode, 9)
        self.assertEqual(json.loads(p.stdout)["exit"], 9)
        self.assertEqual(json.loads(p.stdout)["stdout_tail"], "out-tail\n")


if __name__ == "__main__":
    unittest.main()
