#!/usr/bin/env python3
"""Slice 2 QA (merge-aware-deploy): procedure "Preview a merge without changing the server".

Reuses the helpers of qa/slice-1/run_qa.py. Same setup: disposable pinned delfer/alpine-ftp-server
container with USERS='test|test|/', profile `qa` (remote_root "/") in a temporary HOME, the FTP port in
/tmp/qa-cp, and the password stored with `set-password qa` inside a fresh kernel session keyring.
"""
import importlib.util, json, os, sys

spec = importlib.util.spec_from_file_location("s1", os.path.join(os.path.dirname(__file__), "..", "slice-1", "run_qa.py"))
s1 = importlib.util.module_from_spec(spec); spec.loader.exec_module(s1)
check, cli, manifest, uploads, write, git, seed = s1.check, s1.cli, s1.manifest, s1.uploads, s1.write, s1.git, s1.seed

SERVER = {"Mails.php": b"1\n2\n3 server\n4 caf\xe9\n5\n6\n", "a.txt": b"a base\n", "b.txt": b"b base\n"}
BASE_MAILS = b"1\n2\n3\n4 caf\xe9\n5\n6\n"
ARGS = ("deploy-branch", "qa", "--repo-root", s1.SITE, "--base", "base", "--head", "head", "--mode", "merge", "--dry-run")


def server_unchanged(mcp, tag, expect):
    for n, d in expect.items():
        check(f"{tag} server {n} unchanged", mcp.download(n) == d)


def proc(mcp):
    print("== Procedure: Preview a merge without changing the server")
    s1.new_repo(); s1.clear_server()
    s1.write("Mails.php", BASE_MAILS); s1.write("a.txt", b"a base\n"); s1.write("b.txt", b"b base\n")
    git("add", "."); git("commit", "-qm", "base"); git("tag", "base")
    s1.write("Mails.php", b"1\n2 head\n3\n4 caf\xe9\n5\n6\n"); s1.write("a.txt", b"a head\n"); s1.write("b.txt", b"b head\n")
    git("add", "."); git("commit", "-qm", "head"); git("tag", "head")
    seed(SERVER)

    p = cli(*ARGS)
    check("1 exit nonzero", p.returncode != 0, p.returncode)
    m = manifest(p)
    check("1 dry_run true", m.get("dry_run") is True, m.get("dry_run"))
    check("1 mode merge", m.get("mode") == "merge", m.get("mode"))
    check("1 blocked_by_conflicts true", m.get("blocked_by_conflicts") is True)
    check("1 success false", m.get("success") is False)
    u = uploads(m)
    check("1 Mails.php conflict", u["Mails.php"]["merge_status"] == "conflict", u["Mails.php"])
    for n in ("a.txt", "b.txt"):
        check(f"1 {n} fast_forward/planned", u[n]["merge_status"] == "fast_forward" and u[n]["upload_status"] == "planned", u[n])
    print("   1 manifest:", json.dumps(m)[:1500])
    server_unchanged(mcp, "2", SERVER)

    # step 3: restore the server's Mails.php to base
    seed({"Mails.php": BASE_MAILS})
    expect = dict(SERVER, **{"Mails.php": BASE_MAILS})
    p = cli(*ARGS)
    check("3 exit 0", p.returncode == 0, (p.returncode, p.stderr[-300:]))
    m = manifest(p)
    check("3 dry_run true / mode merge", m.get("dry_run") is True and m.get("mode") == "merge")
    check("3 blocked_by_conflicts false", m.get("blocked_by_conflicts") is False)
    check("3 success true", m.get("success") is True)
    for n, x in uploads(m).items():
        check(f"3 {n} has merge status", bool(x.get("merge_status")) and x["merge_status"] != "conflict", x)
        check(f"3 {n} planned", x["upload_status"] == "planned", x)
    check("3 three files", len(m["uploads"]) == 3, len(m["uploads"]))
    print("   3 manifest:", json.dumps(m)[:1500])
    server_unchanged(mcp, "3", expect)


if __name__ == "__main__":
    mcp = s1.Mcp()
    try:
        proc(mcp)
    finally:
        mcp.close()
    print("FAILED: %s" % s1.fails if s1.fails else "ALL PASS")
    sys.exit(1 if s1.fails else 0)
