#!/usr/bin/env python3
"""Slice 3 QA (merge-aware-deploy): drives `zed-ftp-mcp serve` over stdio JSON-RPC against a disposable,
pinned delfer/alpine-ftp-server container that this script starts and removes itself.

Procedures: "Deploy a directory without clobbering server changes" and
"Upload one file only when the server matches what you expect" (qa.md).

Setup, all automatic: temporary HOME with profile `qa` (remote_root "/", user home "/", slice 1 workaround for the
pinned image's MKD-on-existing-directory bug), a fresh kernel session keyring (the keychain is otherwise
unavailable in the container), and the password stored with `set-password qa` through a pty. Because the pinned
image cannot restart a stopped container, each procedure starts its own container.
Reuses the helpers of qa/slice-1/run_qa.py. Note: with the user home at "/", drifted remote paths are "/c.txt",
where qa.md writes "/home/test/c.txt".
"""
import ctypes, importlib.util, json, os, pty, subprocess, sys, tempfile, time

QA_HOME = tempfile.mkdtemp(prefix="qa3-home-")
os.environ["QA_HOME"] = QA_HOME
os.environ["QA_SITE"] = tempfile.mkdtemp(prefix="qa3-site-")
open("/tmp/qa-cp", "a").close() if os.path.exists("/tmp/qa-cp") else open("/tmp/qa-cp", "w").write("0")
# join a fresh session keyring so the keychain works (KEYCTL_JOIN_SESSION_KEYRING = 1)
ctypes.CDLL(None, use_errno=True).syscall(250, 1, None)

spec = importlib.util.spec_from_file_location("s1", os.path.join(os.path.dirname(__file__), "..", "slice-1", "run_qa.py"))
s1 = importlib.util.module_from_spec(spec); spec.loader.exec_module(s1)
check, git, write, seed = s1.check, s1.git, s1.write, s1.seed
IMAGE = "delfer/alpine-ftp-server:latest@sha256:60bb774d8408d9d4d5c74d05d1c086a34ce192c6c1a142ffac268cac0dbc6fac"
SITE = s1.SITE
ROOT = "/site"  # stands in for qa.md "/home/test": /home is root-owned in the pinned image and the server user home stays "/"


def start_server(passive):
    subprocess.run(["docker", "rm", "-f", s1.CONTAINER], capture_output=True)
    subprocess.run(["docker", "run", "-d", "--name", s1.CONTAINER, "-e", "USERS=test|test|/", "-e", "ADDRESS=127.0.0.1",
                    "-e", f"MIN_PORT={passive}", "-e", f"MAX_PORT={passive}", "-p", "127.0.0.1::21",
                    "-p", f"127.0.0.1:{passive}:{passive}", IMAGE], check=True, capture_output=True)
    out = subprocess.check_output(["docker", "port", s1.CONTAINER, "21/tcp"]).decode().split("\n")[0]
    s1.PORT = int(out.rsplit(":", 1)[1])
    deadline = time.time() + 60
    ok = 0  # readiness: three consecutive full logins (the image's port proxy accepts before vsftpd is ready)
    while ok < 3:
        try:
            s1.ftp().quit(); ok += 1
        except Exception:
            ok = 0
            if time.time() > deadline:
                print(subprocess.run(['docker', 'logs', '--tail', '20', s1.CONTAINER], capture_output=True, text=True).stdout)
                raise
        time.sleep(1)
    cfg = os.path.join(QA_HOME, ".config", "zed-ftp")
    os.makedirs(cfg, exist_ok=True)
    open(os.path.join(cfg, "connections.toml"), "w").write(
        f'[profiles.qa]\nhost = "127.0.0.1"\nport = {s1.PORT}\nuser = "test"\nremote_root = "{ROOT}"\n'
        f'local_root = "{SITE}"\npassive = true\nignore = [".git"]\n')
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(s1.BIN, [s1.BIN, "set-password", "qa"], s1.ENV)
    time.sleep(1); os.write(fd, b"test\n"); time.sleep(1); os.write(fd, b"test\n")
    _, status = os.waitpid(pid, 0)
    assert status == 0, "set-password failed"


class Mcp(s1.Mcp):
    def call(self, name, **args):
        r = self.rpc("tools/call", {"name": name, "arguments": dict(profile="qa", **args)})
        if "error" in r:
            return None, r["error"]
        res = r["result"]
        sc = res.get("structuredContent") or json.loads(res["content"][0]["text"])
        return (None, {"tool_error": sc}) if res.get("isError") else (sc, None)


def server_files(paths):
    f = s1.ftp(); out = {}
    for p in paths:
        buf = []
        try:
            f.retrbinary("RETR " + p, buf.append); out[p] = b"".join(buf)
        except Exception:
            out[p] = None
    f.quit(); return out


def put(path, data):
    import io
    f = s1.ftp(); f.storbinary("STOR " + path, io.BytesIO(data)); f.quit()


def proc_deploy():
    print("== Procedure: Deploy a directory without clobbering server changes")
    start_server(21150)
    s1.new_repo()
    base = {"a.txt": b"a base\n", "b.txt": b"b base\n", "c.txt": b"c base\n", "sub/d.txt": b"d base\n"}
    for k, v in base.items(): write(k, v)
    git("add", "."); git("commit", "-qm", "base"); git("tag", "base")
    work = {"a.txt": b"a work\n", "b.txt": b"b work\n", "c.txt": b"c work\n", "sub/d.txt": b"d base\n"}
    for k in ("a.txt", "b.txt", "c.txt"): write(k, work[k])
    f = s1.ftp()
    for d in (ROOT, ROOT + "/sub"): f.mkd(d)
    f.quit()
    for k, v in base.items(): put(ROOT + "/" + k, v)
    put(ROOT + "/c.txt", b"c SERVER ONLY\n")
    names = [ROOT + "/" + n for n in ("a.txt", "b.txt", "c.txt", "sub/d.txt")]
    before = server_files(names)
    mcp = Mcp()
    try:
        r, e = mcp.call("ftp_deploy", dry_run=True, expect_ref="base")
        check("1 no error", e is None, e); r = r or {}
        dc = r.get("drift_check", {})
        print("   dry-run response:", json.dumps(r)[:600])
        planned = sorted(u.get("remote") for u in r.get("uploaded", []))
        check("1 dry_run planned files listed", r.get("dry_run") is True and planned == sorted(names), (r.get("dry_run"), planned))
        check("1 refused true", dc.get("refused") is True, dc)
        check("1 one drifted /c.txt content_differs", dc.get("drifted") == [{"remote_path": ROOT + "/c.txt", "reason": "content_differs"}], dc)
        check("1 checked 4", dc.get("checked") == 4, dc)
        check("1 server unchanged", server_files(names) == before)

        r, e = mcp.call("ftp_deploy", expect_ref="base")
        check("2 no error", e is None, e); r = r or {}
        print("   step2 response:", json.dumps(r)[:600])
        check("2 files_uploaded 0 and directories_created 0", r.get("files_uploaded") == 0 and r.get("directories_created") == 0, r)
        check("2 refused true", r.get("drift_check", {}).get("refused") is True, r)
        check("2 a.txt still base", mcp.download("a.txt") == b"a base\n")
        check("2 server unchanged", server_files(names) == before)

        put(ROOT + "/c.txt", base["c.txt"])
        r, e = mcp.call("ftp_deploy", expect_ref="base")
        check("3 no error", e is None, e); r = r or {}
        dc = r.get("drift_check", {})
        check("3 refused false and drifted empty", dc.get("refused") is False and dc.get("drifted") == [], dc)
        check("3 files uploaded", r.get("files_uploaded") == 4, r)
        got = server_files(names)
        check("3 server equals working tree", all(got[ROOT + "/" + k] == v for k, v in work.items()), got)

        put(ROOT + "/c.txt", b"c SERVER ONLY\n")
        r, e = mcp.call("ftp_deploy")
        check("4 no error", e is None, e); r = r or {}
        check("4 no drift_check", "drift_check" not in r, r)
        check("4 c.txt overwritten", mcp.download("c.txt") == work["c.txt"])
    finally:
        mcp.close()
    subprocess.run(["docker", "stop", "-t", "1", s1.CONTAINER], check=True, capture_output=True)
    mcp = Mcp()
    try:
        r, e = mcp.call("ftp_deploy", dry_run=True)
        planned = sorted(u.get("remote") for u in (r or {}).get("uploaded", []))
        check("5 offline dry run lists planned files, no connection error", e is None and r.get("dry_run") is True and planned == sorted(names) and "drift_check" not in r, (r, e))
    finally:
        mcp.close()


def proc_upload():
    print("== Procedure: Upload one file only when the server matches what you expect")
    start_server(21151)
    s1.new_repo()
    head = b"<?php // head version\n"
    write("Mails.php", head); git("add", "."); git("commit", "-qm", "head")
    write("Mails.php", head + b"// uncommitted edit\n")
    open("/tmp/loose.txt", "w").write("loose\n")
    f = s1.ftp()
    for d in (ROOT,): f.mkd(d)
    f.quit()
    put(ROOT + "/Mails.php", head)
    lp = os.path.join(SITE, "Mails.php")
    mcp = Mcp()
    try:
        r, e = mcp.call("ftp_upload_file", local_path=lp, remote_path="Mails.php", before_changes=True, expect_ref="HEAD")
        check("1 upload succeeds", e is None and r is not None, e); r = r or {}
        check("1 refused false", r.get("drift_check", {}).get("refused") is False, r)
        check("1 bytes == HEAD size", r.get("bytes") == len(head), r)
        check("1 server has HEAD version", mcp.download("Mails.php") == head)

        edited = b"<?php // server edit\n"
        put(ROOT + "/Mails.php", edited)
        r, e = mcp.call("ftp_upload_file", local_path=lp, remote_path="Mails.php", expect_ref="HEAD")
        check("2 no error", e is None, e); r = r or {}
        check("2 bytes 0", r.get("bytes") == 0, r)
        check("2 content_differs ROOT/Mails.php", r.get("drift_check", {}).get("drifted") == [{"remote_path": ROOT + "/Mails.php", "reason": "content_differs"}], r)
        check("2 server keeps edit", mcp.download("Mails.php") == edited)

        r, e = mcp.call("ftp_upload_file", local_path=lp, remote_path="Mails.php", expect_ref="no-such-branch")
        print("   step3 error:", json.dumps(e)[:300])
        check("3 invalid-params error", e is not None and e.get("code") == -32602, (r, e))
        check("3 server unchanged", mcp.download("Mails.php") == edited)

        r, e = mcp.call("ftp_upload_file", local_path="/tmp/loose.txt", remote_path="loose.txt", expect_ref="HEAD")
        print("   step4 error:", json.dumps(e)[:300])
        check("4 invalid-params error", e is not None and e.get("code") == -32602, (r, e))
        check("4 no loose.txt on server", server_files([ROOT + "/loose.txt"])[ROOT + "/loose.txt"] is None)
    finally:
        mcp.close()


if __name__ == "__main__":
    try:
        proc_deploy(); proc_upload()
    finally:
        subprocess.run(["docker", "rm", "-f", s1.CONTAINER], capture_output=True)
    print("FAILED: %s" % s1.fails if s1.fails else "ALL PASS")
    sys.exit(1 if s1.fails else 0)
