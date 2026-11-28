#!/usr/bin/env python3
"""Tests for tools/tikv-patched.sh and tools/tikv-patched.version."""

import hashlib
import http.server
import io
import json
import os
import platform
import stat
import subprocess
import tarfile
import tempfile
import threading
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "tools", "tikv-patched.sh")
VERSION_FILE = os.path.join(ROOT, "tools", "tikv-patched.version")

PATCHED_COMMIT = "a87863eb7ce3b848f1d9ea964e4a84c823da2359"
STOCK_COMMIT = "a1fdd6a59bb9c8a282c15677e1894501a52425cf"


def host_sha_key():
    os_name = platform.system().lower()
    machine = platform.machine().lower()
    arch = "amd64" if machine in ("x86_64", "amd64") else "arm64"
    return f"sha256_{os_name}_{arch}", f"{os_name}-{arch}"


def make_fake_tikv_script(path, version="8.5.8", git_hash=PATCHED_COMMIT):
    body = f"""#!/usr/bin/env bash
if [ "${{1:-}}" = "--version" ]; then
  cat <<'EOF'
TiKV
Release Version:   {version}
Edition:           Community
Git Commit Hash:   {git_hash}
Git Commit Branch: rust-tidb/v8.5.8-stable-rpn-fn
Profile:           dist_release
EOF
  exit 0
fi
exit 1
"""
    with open(path, "w") as f:
        f.write(body)
    os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def make_tarball(tar_path, binary_bytes, arcname="tikv-server"):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        info = tarfile.TarInfo(name=arcname)
        info.size = len(binary_bytes)
        info.mode = 0o755
        tf.addfile(info, io.BytesIO(binary_bytes))
    data = buf.getvalue()
    with open(tar_path, "wb") as f:
        f.write(data)
    return hashlib.sha256(data).hexdigest()


def write_version_file(path, commit=PATCHED_COMMIT, sha_overrides=None):
    sha_overrides = sha_overrides or {}
    lines = [
        "repo=dulao5/tikv",
        "branch=rust-tidb/v8.5.8-stable-rpn-fn",
        "tag=tikv-v8.5.8-tidbrs.1",
        "version=8.5.8",
        f"commit={commit}",
        f"sha256_linux_amd64={sha_overrides.get('sha256_linux_amd64', 'pending')}",
        f"sha256_linux_arm64={sha_overrides.get('sha256_linux_arm64', 'pending')}",
        f"sha256_darwin_arm64={sha_overrides.get('sha256_darwin_arm64', 'pending')}",
    ]
    with open(path, "w") as f:
        f.write("\n".join(lines) + "\n")


def run_tool(args, env_extra=None):
    env = os.environ.copy()
    for k in ("TIDB_RS_TIKV_BIN", "TIDB_RS_TIKV_SRC", "TIDB_RS_TIKV_CACHE",
              "TIDB_RS_TIKV_VERSION_FILE", "TIDB_RS_TIKV_URL_BASE",
              "TIDB_RS_TIKV_PD_WAIT", "TIDB_RS_TIKV_REQUIRE_STATUS"):
        env.pop(k, None)
    if env_extra:
        env.update(env_extra)
    return subprocess.run([SCRIPT] + args, capture_output=True, text=True, env=env)


class FileServer:
    def __init__(self, directory):
        class Handler(http.server.SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=directory, **kwargs)

            def log_message(self, fmt, *args):
                pass

        self.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.srv.server_address[1]
        self.thread = threading.Thread(target=self.srv.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return f"http://127.0.0.1:{self.port}"

    def __exit__(self, *args):
        self.srv.shutdown()
        self.srv.server_close()


class MockClusterServer:
    def __init__(self, routes):
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                code, body = routes.get(self.path, (404, "not found"))
                self.send_response(code)
                self.end_headers()
                self.wfile.write(body.encode("utf-8"))

            def log_message(self, fmt, *args):
                pass

        self.srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.srv.server_address[1]
        self.thread = threading.Thread(target=self.srv.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return f"127.0.0.1:{self.port}"

    def __exit__(self, *args):
        self.srv.shutdown()
        self.srv.server_close()


class TikvPatchedTest(unittest.TestCase):
    def test_shipped_version_file_parses(self):
        res = run_tool(["--print-info", "--json", "--version-file", VERSION_FILE])
        self.assertEqual(res.returncode, 0, res.stderr)
        doc = json.loads(res.stdout)
        self.assertEqual(doc["repo"], "dulao5/tikv")
        self.assertEqual(doc["branch"], "rust-tidb/v8.5.8-stable-rpn-fn")
        self.assertEqual(doc["version"], "8.5.8")
        self.assertEqual(doc["commit"], PATCHED_COMMIT)

    def test_malformed_version_file_is_rejected(self):
        with tempfile.TemporaryDirectory() as d:
            vf = os.path.join(d, "bad.version")
            with open(vf, "w") as f:
                f.write("repo=bad\n")
            res = run_tool(["--print-info", "--version-file", vf])
            self.assertNotEqual(res.returncode, 0)
            self.assertIn("malformed", res.stderr)

    def test_check_bin_accepts_patched_and_rejects_stock(self):
        with tempfile.TemporaryDirectory() as d:
            vf = os.path.join(d, "v")
            write_version_file(vf)
            patched = os.path.join(d, "tikv-patched")
            stock = os.path.join(d, "tikv-stock")
            wrong_ver = os.path.join(d, "tikv-857")
            make_fake_tikv_script(patched, "8.5.8", PATCHED_COMMIT)
            make_fake_tikv_script(stock, "8.5.8", STOCK_COMMIT)
            make_fake_tikv_script(wrong_ver, "8.5.7", PATCHED_COMMIT)

            r_ok = run_tool(["--check-bin", patched, "--version-file", vf])
            self.assertEqual(r_ok.returncode, 0, r_ok.stderr)

            r_stock = run_tool(["--check-bin", stock, "--version-file", vf])
            self.assertNotEqual(r_stock.returncode, 0)
            self.assertIn(STOCK_COMMIT, r_stock.stderr)

            r_ver = run_tool(["--check-bin", wrong_ver, "--version-file", vf])
            self.assertNotEqual(r_ver.returncode, 0)
            self.assertIn("8.5.7", r_ver.stderr)

    def test_override_bin_and_src_enforce_commit_hash(self):
        with tempfile.TemporaryDirectory() as d:
            vf = os.path.join(d, "v")
            write_version_file(vf)
            patched = os.path.join(d, "tikv-server")
            stock = os.path.join(d, "tikv-stock")
            make_fake_tikv_script(patched, "8.5.8", PATCHED_COMMIT)
            make_fake_tikv_script(stock, "8.5.8", STOCK_COMMIT)

            r_ok = run_tool(["--version-file", vf], {"TIDB_RS_TIKV_BIN": patched})
            self.assertEqual(r_ok.returncode, 0, r_ok.stderr)
            self.assertEqual(r_ok.stdout.strip(), patched)

            r_bad = run_tool(["--version-file", vf], {"TIDB_RS_TIKV_BIN": stock})
            self.assertNotEqual(r_bad.returncode, 0)

            src_dir = os.path.join(d, "tikv-checkout")
            os.makedirs(os.path.join(src_dir, "bin"))
            make_fake_tikv_script(os.path.join(src_dir, "bin", "tikv-server"), "8.5.8", PATCHED_COMMIT)
            r_src = run_tool(["--version-file", vf], {"TIDB_RS_TIKV_SRC": src_dir})
            self.assertEqual(r_src.returncode, 0, r_src.stderr)
            self.assertEqual(r_src.stdout.strip(), os.path.join(src_dir, "bin", "tikv-server"))

    def test_pending_sha_without_override_fails_clearly(self):
        with tempfile.TemporaryDirectory() as d:
            vf = os.path.join(d, "v")
            write_version_file(vf)
            res = run_tool(["--version-file", vf], {"TIDB_RS_TIKV_CACHE": os.path.join(d, "cache")})
            self.assertNotEqual(res.returncode, 0)
            self.assertIn("TIDB_RS_TIKV_BIN", res.stderr)

    def test_download_sha256_verification_and_caching(self):
        sha_key, plat = host_sha_key()
        with tempfile.TemporaryDirectory() as d:
            serve_dir = os.path.join(d, "serve")
            cache_dir = os.path.join(d, "cache")
            os.makedirs(serve_dir)
            bin_src = os.path.join(d, "raw-tikv")
            make_fake_tikv_script(bin_src, "8.5.8", PATCHED_COMMIT)
            with open(bin_src, "rb") as f:
                bin_bytes = f.read()

            tar_name = f"tikv-server-tikv-v8.5.8-tidbrs.1-{plat}.tar.gz"
            sha = make_tarball(os.path.join(serve_dir, tar_name), bin_bytes)
            vf = os.path.join(d, "v")
            write_version_file(vf, sha_overrides={sha_key: sha})

            # --check fails before download
            r_pre = run_tool(["--check", "--version-file", vf], {"TIDB_RS_TIKV_CACHE": cache_dir})
            self.assertNotEqual(r_pre.returncode, 0)

            with FileServer(serve_dir) as url:
                r_dl = run_tool(["--version-file", vf],
                                {"TIDB_RS_TIKV_CACHE": cache_dir, "TIDB_RS_TIKV_URL_BASE": url})
                self.assertEqual(r_dl.returncode, 0, r_dl.stderr)
                expected_bin = os.path.join(cache_dir, "tikv-v8.5.8-tidbrs.1", "tikv-server")
                self.assertEqual(r_dl.stdout.strip(), expected_bin)

            # Cached binary passes --check offline without URL_BASE
            r_post = run_tool(["--check", "--version-file", vf], {"TIDB_RS_TIKV_CACHE": cache_dir})
            self.assertEqual(r_post.returncode, 0, r_post.stderr)
            self.assertEqual(r_post.stdout.strip(), expected_bin)

            # Tampered tarball fails SHA-256 verification
            bad_vf = os.path.join(d, "v-bad")
            write_version_file(bad_vf, sha_overrides={sha_key: "0" * 64})
            bad_cache = os.path.join(d, "bad-cache")
            with FileServer(serve_dir) as url:
                r_mismatch = run_tool(["--version-file", bad_vf],
                                      {"TIDB_RS_TIKV_CACHE": bad_cache, "TIDB_RS_TIKV_URL_BASE": url})
                self.assertNotEqual(r_mismatch.returncode, 0)
                self.assertIn("sha256 mismatch", r_mismatch.stderr)
                self.assertFalse(os.path.exists(os.path.join(bad_cache, "tikv-v8.5.8-tidbrs.1", "tikv-server")))

    def test_download_rejects_tarball_whose_binary_has_wrong_commit(self):
        sha_key, plat = host_sha_key()
        with tempfile.TemporaryDirectory() as d:
            serve_dir = os.path.join(d, "serve")
            cache_dir = os.path.join(d, "cache")
            os.makedirs(serve_dir)
            bin_src = os.path.join(d, "stock-tikv")
            make_fake_tikv_script(bin_src, "8.5.8", STOCK_COMMIT)
            with open(bin_src, "rb") as f:
                bin_bytes = f.read()

            tar_name = f"tikv-server-tikv-v8.5.8-tidbrs.1-{plat}.tar.gz"
            sha = make_tarball(os.path.join(serve_dir, tar_name), bin_bytes)
            vf = os.path.join(d, "v")
            write_version_file(vf, sha_overrides={sha_key: sha})

            with FileServer(serve_dir) as url:
                res = run_tool(["--version-file", vf],
                               {"TIDB_RS_TIKV_CACHE": cache_dir, "TIDB_RS_TIKV_URL_BASE": url})
                self.assertNotEqual(res.returncode, 0)
                self.assertIn("Git Commit Hash", res.stderr)
                self.assertFalse(os.path.exists(os.path.join(cache_dir, "tikv-v8.5.8-tidbrs.1", "tikv-server")))

    def test_check_status_and_check_pd_and_check_log(self):
        with tempfile.TemporaryDirectory() as d:
            vf = os.path.join(d, "v")
            write_version_file(vf)

            ok_metrics = f'tikv_server_info{{version="v8.5.8",hash="{PATCHED_COMMIT}"}} 1764316800\n'
            bad_metrics = f'tikv_server_info{{version="v8.5.8",hash="{STOCK_COMMIT}"}} 1764316800\n'

            with MockClusterServer({"/status": (200, ""), "/metrics": (200, ok_metrics)}) as st_ok, \
                 MockClusterServer({"/status": (200, ""), "/metrics": (200, bad_metrics)}) as st_bad:
                r1 = run_tool(["--check-status", st_ok, "--version-file", vf])
                self.assertEqual(r1.returncode, 0, r1.stderr)

                r2 = run_tool(["--check-status", st_bad, "--version-file", vf])
                self.assertNotEqual(r2.returncode, 0)
                self.assertIn(STOCK_COMMIT, r2.stderr)

                pd_ok_body = json.dumps({
                    "count": 2,
                    "stores": [
                        {"store": {"id": 1, "address": "127.0.0.1:20160", "version": "8.5.8",
                                   "git_hash": PATCHED_COMMIT, "state_name": "Up", "status_address": st_ok}},
                        {"store": {"id": 2, "address": "127.0.0.1:20161", "version": "8.5.8",
                                   "git_hash": STOCK_COMMIT, "state_name": "Tombstone", "status_address": ""}},
                    ],
                })
                pd_mixed_body = json.dumps({
                    "count": 2,
                    "stores": [
                        {"store": {"id": 1, "address": "127.0.0.1:20160", "version": "8.5.8",
                                   "git_hash": PATCHED_COMMIT, "state_name": "Up", "status_address": st_ok}},
                        {"store": {"id": 3, "address": "127.0.0.1:20162", "version": "8.5.8",
                                   "git_hash": STOCK_COMMIT, "state_name": "Up", "status_address": st_bad}},
                    ],
                })
                with MockClusterServer({"/pd/api/v1/stores": (200, pd_ok_body)}) as pd_ok, \
                     MockClusterServer({"/pd/api/v1/stores": (200, pd_mixed_body)}) as pd_mixed:
                    rp = run_tool(["--inspect-pd", pd_ok, "--version-file", vf], {"TIDB_RS_TIKV_PD_WAIT": "2"})
                    self.assertEqual(rp.returncode, 0, rp.stderr)
                    info = json.loads(rp.stdout)
                    self.assertEqual(info["version"], "8.5.8")
                    self.assertEqual(info["commit"], PATCHED_COMMIT)
                    self.assertEqual(info["stores"], 1)

                    rm = run_tool(["--check-pd", pd_mixed, "--version-file", vf], {"TIDB_RS_TIKV_PD_WAIT": "2"})
                    self.assertNotEqual(rm.returncode, 0)
                    self.assertIn("store 3", rm.stderr)

            log_ok = os.path.join(d, "tikv-ok.log")
            log_bad = os.path.join(d, "tikv-bad.log")
            with open(log_ok, "w") as f:
                f.write(
                    f'[2026/11/28 08:00:00.000 +00:00] [INFO] [lib.rs:91] ["Release Version:   8.5.8"]\n'
                    f'[2026/11/28 08:00:00.000 +00:00] [INFO] [lib.rs:91] ["Git Commit Hash:   {STOCK_COMMIT}"]\n'
                    f'[2026/11/28 08:05:00.000 +00:00] [INFO] [lib.rs:91] ["Release Version:   8.5.8"]\n'
                    f'[2026/11/28 08:05:00.000 +00:00] [INFO] [lib.rs:91] ["Git Commit Hash:   {PATCHED_COMMIT}"]\n'
                )
            with open(log_bad, "w") as f:
                f.write(
                    f'[2026/11/28 08:00:00.000 +00:00] [INFO] [lib.rs:91] ["Release Version:   8.5.8"]\n'
                    f'[2026/11/28 08:00:00.000 +00:00] [INFO] [lib.rs:91] ["Git Commit Hash:   {STOCK_COMMIT}"]\n'
                )
            self.assertEqual(run_tool(["--check-log", log_ok, "--version-file", vf]).returncode, 0)
            self.assertNotEqual(run_tool(["--check-log", log_bad, "--version-file", vf]).returncode, 0)


if __name__ == "__main__":
    unittest.main()
