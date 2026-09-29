#!/usr/bin/env python3
"""Slice 1 QA (merge-aware-deploy): drives the zed-ftp-mcp CLI and `serve` MCP tool
against a disposable delfer/alpine-ftp-server container.

Setup used by Slice QA (a pre-existing branch-deploy defect forces the deviation, see the QA report):
- container: pinned image, USERS='test|test|/' (home is `/`), profile `qa` with remote_root = "/" in a
  temporary HOME/XDG_CONFIG_HOME (QA_HOME), and the FTP port written to /tmp/qa-cp.
- keychain: run inside a fresh kernel session keyring and store the password with `set-password qa`.
- the pinned image cannot restart a stopped container; procedure 3 step 4 stops it, so recreate it per run.

Procedures: "Merge a branch into a server that has its own edits",
"A conflicting file blocks the whole merge", "Overwrite deploys behave as before".
"""
import base64, ftplib, io, json, os, subprocess, sys, tempfile, time, shutil

BIN = os.environ.get("ZED_FTP_BIN", "/home/user/zed-ftp-mcp/target/debug/zed-ftp-mcp")
QA_HOME = os.environ.get("QA_HOME", "/tmp/qa-home")
SITE = os.environ.get("QA_SITE", "/tmp/qa-site")
CONTAINER = os.environ.get("QA_CONTAINER", "qa-ftp")
PORT = int(open("/tmp/qa-cp").read())
ENV = dict(os.environ, HOME=QA_HOME, XDG_CONFIG_HOME=QA_HOME + "/.config")
fails = []


def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name + ("" if cond else "  -> " + str(detail)))
    if not cond:
        fails.append(name)


def git(*a, cwd=None):
    subprocess.run(["git", *a], cwd=cwd or SITE, check=True, capture_output=True,
                   env=dict(os.environ, GIT_AUTHOR_NAME="q", GIT_AUTHOR_EMAIL="q@q",
                            GIT_COMMITTER_NAME="q", GIT_COMMITTER_EMAIL="q@q"))


def write(path, data):
    full = os.path.join(SITE, path)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    open(full, "wb").write(data)


def lines(*ls, sep=b"\n"):
    return b"".join(l if isinstance(l, bytes) else l.encode() for l in [x + "\n" if isinstance(x, str) else x + b"\n" for x in ls])


def new_repo():
    shutil.rmtree(SITE, ignore_errors=True)
    os.makedirs(SITE)
    git("init", "-q", "-b", "main")


def ftp():
    f = ftplib.FTP()
    f.connect("127.0.0.1", PORT, timeout=20)
    f.login("test", "test")
    f.set_pasv(True)
    f.cwd("/")
    return f


def clear_server():
    f = ftp()
    for name in ("Mailer.php", "Mails.php", "logo.bin", "same.txt", "new.txt", "a.txt", "b.txt"):
        try:
            f.delete("/" + name)
        except ftplib.error_perm:
            pass
    f.quit()


def seed(files):
    f = ftp()
    for name, data in files.items():
        f.storbinary("STOR " + name, io.BytesIO(data))
    f.quit()


def cli(*args):
    p = subprocess.run([BIN, *args], capture_output=True, env=ENV, timeout=120)
    return p


def manifest(p):
    out = p.stdout.decode(errors="replace")
    return json.loads(out)


class Mcp:
    def __init__(self):
        self.p = subprocess.Popen([BIN, "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, env=ENV)
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "qa", "version": "0"}})
        self.p.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n'); self.p.stdin.flush()

    def rpc(self, method, params):
        self.n += 1
        self.p.stdin.write((json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}) + "\n").encode())
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            msg = json.loads(line)
            if msg.get("id") == self.n:
                return msg

    def download(self, path):
        r = self.rpc("tools/call", {"name": "ftp_download_file", "arguments": {"profile": "qa", "remote_path": path}})
        res = r["result"]
        sc = res.get("structuredContent") or json.loads(res["content"][0]["text"])
        return base64.b64decode(sc["content"]) if sc["encoding"] == "base64" else sc["content"].encode()

    def close(self):
        self.p.stdin.close(); self.p.wait(timeout=10)


def uploads(m):
    return {u["path"] if "path" in u else u.get("git_path"): u for u in m["uploads"]}


def proc1(mcp):
    print("== Procedure: Merge a branch into a server that has its own edits")
    new_repo(); clear_server()
    base = {"Mailer.php": lines(*"abcdefgh"), "Mails.php": lines(*"123456"),
            "logo.bin": b"\x00BASE\x00", "same.txt": b"same base\n"}
    for k, v in base.items(): write(k, v)
    git("add", "."); git("commit", "-qm", "base"); git("tag", "base")
    head_mailer = lines("a", "b head", *"cdefgh")
    write("Mailer.php", head_mailer)
    write("Mails.php", lines("1", "2 head", *"3456"))
    write("logo.bin", b"\x00HEAD\x00\x01")
    write("new.txt", b"new\n"); write("same.txt", b"same head\n")
    git("add", "."); git("commit", "-qm", "head"); git("tag", "head")
    head_mails = lines("1", "2 head", *"3456")
    write("Mails.php", head_mails + b"WORKING TREE EDIT\n")
    seed({"Mailer.php": lines(*"abcdefg", "BCC staging", "h"), "Mails.php": base["Mails.php"],
          "logo.bin": base["logo.bin"], "same.txt": b"same head\n"})
    p = cli("deploy-branch", "qa", "--repo-root", SITE, "--base", "base", "--head", "head", "--mode", "merge")
    check("1.1 exit 0", p.returncode == 0, (p.returncode, p.stderr[-300:]))
    m = manifest(p)
    if m.get("failures"): print("   failures:", json.dumps(m["failures"]))
    check("1.1 mode merge", m.get("mode") == "merge", m.get("mode"))
    check("1.1 blocked_by_conflicts false", m.get("blocked_by_conflicts") is False)
    check("1.1 success true", m.get("success") is True)
    u = uploads(m)
    mailer = u["Mailer.php"]
    head_id = subprocess.check_output(["git", "rev-parse", "head:Mailer.php"], cwd=SITE).decode().strip()
    merged = lines("a", "b head", *"cdefg", "BCC staging", "h")
    check("1.2 Mailer merged", mailer["merge_status"] == "merged" and mailer["uploaded_from"] == "merged", mailer)
    check("1.2 Mailer bytes == merged size != head size", mailer["bytes"] == len(merged) and mailer["bytes"] != len(head_mailer), mailer)
    check("1.2 Mailer object_id is head blob", mailer["object_id"] == head_id, (mailer["object_id"], head_id))
    for n in ("Mails.php", "logo.bin"):
        check(f"1.2 {n} fast_forward", u[n]["merge_status"] == "fast_forward", u[n])
    check("1.2 new.txt new_file", u["new.txt"]["merge_status"] == "new_file", u["new.txt"])
    s = u["same.txt"]
    check("1.2 same.txt already_deployed/not_needed", s["merge_status"] == "already_deployed" and s["upload_status"] == "not_needed" and s["verification_status"] == "not_needed", s)
    for n, x in u.items():
        if n != "same.txt":
            check(f"1.2 {n} verified", x["verification_status"] == "verified", x)
    got = mcp.download("Mailer.php")
    check("1.3 Mailer has head change + BCC staging", b"b head" in got and b"BCC staging" in got and got == merged, got)
    got = mcp.download("Mails.php")
    check("1.4 Mails equals committed head", got == head_mails, got)
    return m


def proc2(mcp):
    print("== Procedure: A conflicting file blocks the whole merge")
    new_repo(); clear_server()
    base_mails = b"1\n2\n3\n4 caf\xe9\n5\n6\n"
    write("Mails.php", base_mails); write("a.txt", b"a base\n"); write("b.txt", b"b base\n")
    git("add", "."); git("commit", "-qm", "base"); git("tag", "base")
    write("Mails.php", b"1\n2 head\n3\n4 caf\xe9\n5\n6\n"); write("a.txt", b"a head\n"); write("b.txt", b"b head\n")
    git("add", "."); git("commit", "-qm", "head"); git("tag", "head")
    server_state = {"Mails.php": b"1\n2\n3 server\n4 caf\xe9\n5\n6\n", "a.txt": b"a base\n", "b.txt": b"b base\n"}
    seed(server_state)
    p = cli("deploy-branch", "qa", "--repo-root", SITE, "--base", "base", "--head", "head", "--mode", "merge")
    check("2.1 exit nonzero", p.returncode != 0, p.returncode)
    m = manifest(p)
    check("2.1 blocked_by_conflicts true, success false", m.get("blocked_by_conflicts") is True and m.get("success") is False, (m.get("blocked_by_conflicts"), m.get("success")))
    u = uploads(m); mm = u["Mails.php"]
    check("2.2 conflict/text_conflict/not_attempted", mm["merge_status"] == "conflict" and mm["conflict_reason"] == "text_conflict" and mm["upload_status"] == "not_attempted", mm)
    mt = mm.get("marked_text", "")
    check("2.2 markers", all(t in mt for t in ("<<<<<<<", "|||||||", "=======", ">>>>>>>")), mt)
    check("2.2 lines 2 and 3 inside markers", "2 head" in mt and "3 server" in mt, mt)
    check("2.2 0xE9 shown as U+FFFD", "�" in mt and "\xe9" not in mt, repr(mt))
    check("2.2 failures has merge stage for Mails.php", any(f.get("stage") == "merge" and "Mails.php" in json.dumps(f) for f in m["failures"]), m["failures"])
    for n in ("a.txt", "b.txt"):
        check(f"2.3 {n} fast_forward/not_attempted", u[n]["merge_status"] == "fast_forward" and u[n]["upload_status"] == "not_attempted", u[n])
    for n, d in server_state.items():
        check(f"2.4 server {n} unchanged", mcp.download(n) == d)


def proc3(mcp):
    print("== Procedure: Overwrite deploys behave as before")
    # same repo and server state as procedure 2 (still in place)
    seed_state = {"Mails.php": b"1\n2\n3 server\n4 caf\xe9\n5\n6\n", "a.txt": b"a base\n", "b.txt": b"b base\n"}
    p = cli("deploy-branch", "qa", "--repo-root", SITE, "--base", "base", "--head", "head", "--mode", "rebase")
    err = (p.stderr + p.stdout).decode(errors="replace")
    check("3.1 rebase fails", p.returncode != 0, p.returncode)
    check("3.1 usage error names allowed modes", "overwrite" in err and "merge" in err, err[-400:])
    for n, d in seed_state.items():
        check(f"3.1 server {n} unchanged", mcp.download(n) == d)
    p = cli("deploy-branch", "qa", "--repo-root", SITE, "--base", "base", "--head", "head")
    check("3.2 exit 0", p.returncode == 0, (p.returncode, p.stderr[-300:]))
    m = manifest(p)
    check("3.2 mode overwrite", m.get("mode") == "overwrite", m.get("mode"))
    check("3.2 no merge_status fields", "merge_status" not in json.dumps(m), "")
    check("3.3 Mails.php equals head", mcp.download("Mails.php") == b"1\n2 head\n3\n4 caf\xe9\n5\n6\n")
    subprocess.run(["docker", "stop", CONTAINER], check=True, capture_output=True)
    try:
        p = cli("deploy-branch", "qa", "--repo-root", SITE, "--base", "base", "--head", "head", "--dry-run")
        check("3.4 dry run exit 0", p.returncode == 0, (p.returncode, p.stderr[-300:]))
        m = manifest(p)
        check("3.4 planned manifest", m.get("mode") == "overwrite" and m.get("dry_run") is True and len(m["uploads"]) == 3, m)
    finally:
        pass  # the pinned image cannot restart a stopped container; recreate it for a rerun


if __name__ == "__main__":
    mcp = Mcp()
    try:
        proc1(mcp); proc2(mcp); proc3(mcp)
    finally:
        mcp.close()
    print("FAILED: %s" % fails if fails else "ALL PASS")
    sys.exit(1 if fails else 0)
