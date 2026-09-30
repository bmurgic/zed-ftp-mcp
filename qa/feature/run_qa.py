#!/usr/bin/env python3
"""Feature QA (merge-aware-deploy): runs every qa.md procedure once, end to end, through the public
interfaces (the zed-ftp-mcp CLI and `zed-ftp-mcp serve` over stdio JSON-RPC).

Self-contained setup, the same credential route the slice QA runs used:
- a fresh kernel session keyring (the keychain is otherwise unavailable here), joined before anything else;
- a temporary HOME/XDG_CONFIG_HOME with profile `qa`; the password is stored with `set-password qa` through a pty
  and never printed;
- a disposable container from the pinned delfer/alpine-ftp-server image with USERS='test|test|/' and remote_root "/"
  (slice 1 workaround for the deferred GDD-F0004 MKD-on-existing-directory bug). The pinned image cannot restart a
  stopped container, so a procedure that stops the server gets a fresh container afterwards.

Order:
  branch-deployment
    1 Merge a branch into a server that has its own edits          (qa/slice-1 proc1)
    2 A conflicting file blocks the whole merge                    (qa/slice-1 proc2)
    3 Preview a merge without changing the server                  (qa/slice-2 proc)
    4 Overwrite deploys behave as before                           (qa/slice-1 proc3, server reseeded to the
                                                                    "conflicting file" state first)
  server-drift-guard
    5 Deploy a directory without clobbering server changes         (qa/slice-3, run as its own process)
    6 Upload one file only when the server matches what you expect (qa/slice-3, run as its own process)
"""
import ctypes, importlib.util, os, platform, pty, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
IMAGE = "delfer/alpine-ftp-server:latest@sha256:60bb774d8408d9d4d5c74d05d1c086a34ce192c6c1a142ffac268cac0dbc6fac"
CONTAINER = "qa-feature-ftp"
PASSIVE = 21160

nr = 250 if platform.machine() == "x86_64" else 219  # keyctl
if ctypes.CDLL(None, use_errno=True).syscall(nr, 1, None) < 0:  # KEYCTL_JOIN_SESSION_KEYRING
    sys.exit("could not join a fresh session keyring")

os.environ["QA_HOME"] = tempfile.mkdtemp(prefix="qaf-home-")
os.environ["QA_SITE"] = tempfile.mkdtemp(prefix="qaf-site-")
os.environ["QA_CONTAINER"] = CONTAINER
open("/tmp/qa-cp", "w").write("0")  # slice-1 reads it at import; the real port is set below


def load(name, rel):
    spec = importlib.util.spec_from_file_location(name, os.path.join(HERE, "..", rel, "run_qa.py"))
    mod = importlib.util.module_from_spec(spec); spec.loader.exec_module(mod)
    return mod


s1 = load("s1", "slice-1")
s1.ENV.pop("RUST_BACKTRACE", None)  # show the CLI output a user sees, not a developer backtrace
_cli = s1.cli


def cli_logged(*args):
    """Run the CLI as slice 1 does, and log what the user sees on stderr and the exit code."""
    p = _cli(*args)
    print("   $ zed-ftp-mcp " + " ".join(args) + "  -> exit %d" % p.returncode)
    for line in p.stderr.decode(errors="replace").strip().splitlines()[:8]:
        print("   stderr| " + line)
    return p


s1.cli = cli_logged
s2 = load("s2", "slice-2")
s2.s1 = s1  # share one check()/fails list and one server port
s2.check, s2.cli, s2.manifest, s2.uploads, s2.write, s2.git, s2.seed = s1.check, s1.cli, s1.manifest, s1.uploads, s1.write, s1.git, s1.seed


def start_server():
    subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True)
    subprocess.run(["docker", "run", "-d", "--name", CONTAINER, "-e", "USERS=test|test|/", "-e", "ADDRESS=127.0.0.1",
                    "-e", f"MIN_PORT={PASSIVE}", "-e", f"MAX_PORT={PASSIVE}", "-p", "127.0.0.1::21",
                    "-p", f"127.0.0.1:{PASSIVE}:{PASSIVE}", IMAGE], check=True, capture_output=True)
    out = subprocess.check_output(["docker", "port", CONTAINER, "21/tcp"]).decode().split("\n")[0]
    s1.PORT = int(out.rsplit(":", 1)[1])
    deadline, ok = time.time() + 60, 0
    while ok < 3:  # three consecutive logins: the port proxy accepts before vsftpd is ready
        try:
            s1.ftp().quit(); ok += 1
        except Exception:
            ok = 0
            if time.time() > deadline:
                raise
        time.sleep(1)
    cfg = os.path.join(s1.QA_HOME, ".config", "zed-ftp")
    os.makedirs(cfg, exist_ok=True)
    open(os.path.join(cfg, "connections.toml"), "w").write(
        f'[profiles.qa]\nhost = "127.0.0.1"\nport = {s1.PORT}\nuser = "test"\nremote_root = "/"\n'
        f'local_root = "{s1.SITE}"\npassive = true\nignore = [".git"]\n')
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(s1.BIN, [s1.BIN, "set-password", "qa"], s1.ENV)
    time.sleep(1); os.write(fd, b"test\n"); time.sleep(1); os.write(fd, b"test\n")
    _, status = os.waitpid(pid, 0)
    assert status == 0, "set-password failed"


def branch_deployment():
    start_server()
    mcp = s1.Mcp()
    try:
        s1.proc1(mcp)
        s1.proc2(mcp)
        s2.proc(mcp)
        # Overwrite procedure starts from the "conflicting file" repository and server state.
        print("== (reset) repository and server to the 'A conflicting file blocks the whole merge' state")
        s1.new_repo(); s1.clear_server()
        s1.write("Mails.php", s2.BASE_MAILS); s1.write("a.txt", b"a base\n"); s1.write("b.txt", b"b base\n")
        s1.git("add", "."); s1.git("commit", "-qm", "base"); s1.git("tag", "base")
        s1.write("Mails.php", b"1\n2 head\n3\n4 caf\xe9\n5\n6\n"); s1.write("a.txt", b"a head\n"); s1.write("b.txt", b"b head\n")
        s1.git("add", "."); s1.git("commit", "-qm", "head"); s1.git("tag", "head")
        s1.seed(s2.SERVER)
        s1.check("reset server state matches", all(mcp.download(n) == d for n, d in s2.SERVER.items()))
        s1.proc3(mcp)  # stops the container at step 4
    finally:
        mcp.close()
        subprocess.run(["docker", "rm", "-f", CONTAINER], capture_output=True)


if __name__ == "__main__":
    branch_deployment()
    print("== server-drift-guard procedures (qa/slice-3)")
    r = subprocess.run([sys.executable, os.path.join(HERE, "..", "slice-3", "run_qa.py")])
    if r.returncode != 0:
        s1.fails.append("slice-3 procedures (see output above)")
    print("FAILED: %s" % s1.fails if s1.fails else "ALL PASS")
    sys.exit(1 if s1.fails else 0)
