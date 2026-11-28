#!/usr/bin/env bash
# Manage and verify the patched TiKV v8.5.8 binary pinned in tools/tikv-patched.version.
#
# Usage:
#   tools/tikv-patched.sh [--version-file FILE]
#       Ensure the patched tikv-server for this host is cached and verified, then print its path.
#   tools/tikv-patched.sh --check [--version-file FILE]
#       Verify the configured or cached tikv-server without downloading; print its path.
#   tools/tikv-patched.sh --check-bin <path> [--version-file FILE]
#       Verify that <path> --version reports the pinned version and commit hash.
#   tools/tikv-patched.sh --check-status <host:port> [--version-file FILE]
#       Verify a running TiKV status endpoint (/status and /metrics tikv_server_info).
#   tools/tikv-patched.sh --check-pd <pd-endpoints> [--version-file FILE]
#       Verify all active TiKV stores registered in PD (/pd/api/v1/stores).
#   tools/tikv-patched.sh --inspect-pd <pd-endpoints> [--version-file FILE]
#       Verify PD stores and print live TiKV build metadata as JSON.
#   tools/tikv-patched.sh --check-log <tikv.log> [--version-file FILE]
#       Verify the Release Version and Git Commit Hash logged in <tikv.log>.
#   tools/tikv-patched.sh --print-info [--json] [--version-file FILE]
#       Print the pinned TiKV build metadata (human summary or JSON).
#
# Environment:
#   TIDB_RS_TIKV_BIN          Explicit path to a tikv-server binary (still verified).
#   TIDB_RS_TIKV_SRC          Path to a local dulao5/tikv checkout with built tikv-server.
#   TIDB_RS_TIKV_CACHE        Cache root (default: ~/.cache/tidb-rs/tikv).
#   TIDB_RS_TIKV_VERSION_FILE Override path to tools/tikv-patched.version.
#   TIDB_RS_TIKV_URL_BASE     Base URL for release tarballs (default: GitHub Releases).
#   TIDB_RS_TIKV_PD_WAIT      Seconds to wait for PD stores during startup (default: 15).
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
version_file=${TIDB_RS_TIKV_VERSION_FILE:-$root/tools/tikv-patched.version}

mode=ensure
arg=
json_out=0
while [ $# -gt 0 ]; do
    case "$1" in
        --version-file)
            [ $# -ge 2 ] || { echo "tikv-patched: --version-file requires a path" >&2; exit 2; }
            version_file=$2
            shift 2
            ;;
        --check|--print-info)
            mode=${1#--}
            shift
            ;;
        --json)
            json_out=1
            shift
            ;;
        --check-bin|--check-status|--check-pd|--inspect-pd|--check-log)
            [ $# -ge 2 ] || { echo "tikv-patched: $1 requires an argument" >&2; exit 2; }
            mode=${1#--}
            arg=$2
            shift 2
            ;;
        -h|--help)
            sed -n '2,28p' "$0"
            exit 0
            ;;
        *)
            echo "tikv-patched: unknown argument '$1'" >&2
            exit 2
            ;;
    esac
done

fail() {
    echo "tikv-patched: $*" >&2
    exit 1
}

[ -f "$version_file" ] || fail "version file not found: $version_file"

field() {
    sed -n "s/^$1=//p" "$version_file" | head -n 1 | tr -d '\r'
}

repo=$(field repo)
branch=$(field branch)
tag=$(field tag)
version=$(field version)
commit=$(field commit | tr '[:upper:]' '[:lower:]')

if ! [[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ && -n "$branch" && -n "$tag" \
    && "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.A-Za-z0-9]+)?$ \
    && "$commit" =~ ^[0-9a-f]{7,40}$ ]]; then
    fail "malformed $version_file (needs repo, branch, tag, version, and 7-40 hex commit)"
fi

detect_platform() {
    local os arch
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=darwin ;;
        *) fail "unsupported OS '$(uname -s)' (supported: Linux, Darwin)" ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64) arch=amd64 ;;
        aarch64|arm64) arch=arm64 ;;
        *) fail "unsupported architecture '$(uname -m)' (supported: amd64, arm64)" ;;
    esac
    case "$os-$arch" in
        linux-amd64|linux-arm64|darwin-arm64) echo "$os-$arch" ;;
        *) fail "unsupported platform '$os-$arch' (supported: linux-amd64, linux-arm64, darwin-arm64)" ;;
    esac
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

version_matches() {
    [ "${1#v}" = "${2#v}" ]
}

commit_matches() {
    local exp act
    exp=$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')
    act=$(printf '%s' "$2" | tr '[:upper:]' '[:lower:]')
    [[ "$act" =~ ^[0-9a-f]{7,40}$ ]] || return 1
    if [ "${#exp}" -eq 40 ] && [ "${#act}" -eq 40 ]; then
        [ "$act" = "$exp" ]
    else
        [ "${#act}" -ge "${#exp}" ] && [ "${act#"$exp"}" != "$act" ]
    fi
}

verify_bin() {
    local bin=$1 out got_ver got_hash
    [ -f "$bin" ] || fail "binary not found: $bin"
    [ -x "$bin" ] || fail "binary is not executable: $bin"
    out=$("$bin" --version 2>&1) || fail "failed to run '$bin --version': $out"
    got_ver=$(printf '%s\n' "$out" | sed -n 's/^Release Version:[[:space:]]*//p' | head -n 1 | tr -d '\r')
    got_hash=$(printf '%s\n' "$out" | sed -n 's/^Git Commit Hash:[[:space:]]*//p' | head -n 1 | tr -d '\r')
    if [ -z "$got_ver" ] || [ -z "$got_hash" ]; then
        fail "$bin --version did not report Release Version and Git Commit Hash"
    fi
    if ! version_matches "$version" "$got_ver"; then
        fail "$bin reports Release Version '$got_ver', expected '$version' (tag $tag)"
    fi
    if ! commit_matches "$commit" "$got_hash"; then
        fail "$bin reports Git Commit Hash '$got_hash', expected '$commit' (tag $tag)"
    fi
}

verify_status() {
    python3 -I - "$1" "$version" "$commit" "$tag" <<'PY'
import json
import re
import sys
import urllib.error
import urllib.request

raw_addr, exp_ver, exp_commit, tag = sys.argv[1:5]
addr = raw_addr.strip()
if not addr.startswith(("http://", "https://")):
    addr = "http://" + addr
addr = re.sub(r"/(status|metrics)$", "", addr.rstrip("/"))


def version_matches(a, b):
    return a.lstrip("v") == b.lstrip("v")


def commit_matches(exp, act):
    exp = exp.lower()
    act = act.lower()
    if not re.fullmatch(r"[0-9a-f]{7,40}", act):
        return False
    if len(exp) == 40 and len(act) == 40:
        return act == exp
    return len(act) >= len(exp) and act.startswith(exp)


try:
    with urllib.request.urlopen(f"{addr}/status", timeout=5) as resp:
        if resp.status >= 400:
            sys.exit(f"tikv-patched: {addr}/status returned HTTP {resp.status}")
        status_body = resp.read().decode("utf-8", errors="replace").strip()
except Exception as exc:
    sys.exit(f"tikv-patched: cannot reach {addr}/status: {exc}")

if status_body.startswith("{"):
    try:
        doc = json.loads(status_body)
        if "git_hash" in doc or "version" in doc:
            gv = str(doc.get("version", ""))
            gh = str(doc.get("git_hash", ""))
            if gv and not version_matches(exp_ver, gv):
                sys.exit(f"tikv-patched: {addr}/status reports version {gv!r}, expected {exp_ver!r} (tag {tag})")
            if gh and not commit_matches(exp_commit, gh):
                sys.exit(f"tikv-patched: {addr}/status reports git_hash {gh!r}, expected {exp_commit!r} (tag {tag})")
    except json.JSONDecodeError:
        pass

try:
    with urllib.request.urlopen(f"{addr}/metrics", timeout=5) as resp:
        metrics_text = resp.read().decode("utf-8", errors="replace")
except Exception as exc:
    sys.exit(f"tikv-patched: cannot reach {addr}/metrics: {exc}")

infos = []
for line in metrics_text.splitlines():
    m = re.match(r'^tikv_server_info\{([^}]*)\}\s+', line)
    if not m:
        continue
    labels = dict(re.findall(r'(\w+)="([^"]*)"', m.group(1)))
    infos.append(labels)

if not infos:
    sys.exit(f"tikv-patched: {addr}/metrics did not report tikv_server_info (expected commit {exp_commit})")

for labels in infos:
    gv = labels.get("version", "")
    gh = labels.get("hash", "")
    if not version_matches(exp_ver, gv):
        sys.exit(f"tikv-patched: {addr}/metrics tikv_server_info version={gv!r}, expected {exp_ver!r} (tag {tag})")
    if not commit_matches(exp_commit, gh):
        sys.exit(f"tikv-patched: {addr}/metrics tikv_server_info hash={gh!r}, expected {exp_commit!r} (tag {tag})")

print(f"{exp_ver.lstrip('v')} ({infos[0]['hash'][:9]}, tag {tag})")
PY
}

verify_pd() {
    local pd_arg=$1 inspect_json=$2
    python3 -I - "$pd_arg" "$version" "$commit" "$tag" "$repo" "${TIDB_RS_TIKV_PD_WAIT:-15}" "$inspect_json" "${TIDB_RS_TIKV_REQUIRE_STATUS:-0}" <<'PY'
import json
import re
import sys
import time
import urllib.request

pd_arg, exp_ver, exp_commit, tag, repo, wait_s, inspect_json, req_status = sys.argv[1:9]
wait_s = float(wait_s)
inspect_json = inspect_json == "1"
req_status = req_status == "1"

endpoints = []
for part in pd_arg.split(","):
    ep = part.strip().rstrip("/")
    if not ep:
        continue
    if not ep.startswith(("http://", "https://")):
        ep = "http://" + ep
    endpoints.append(ep)

if not endpoints:
    sys.exit("tikv-patched: no PD endpoints provided")


def version_matches(a, b):
    return a.lstrip("v") == b.lstrip("v")


def commit_matches(exp, act):
    exp = exp.lower()
    act = act.lower()
    if not re.fullmatch(r"[0-9a-f]{7,40}", act):
        return False
    if len(exp) == 40 and len(act) == 40:
        return act == exp
    return len(act) >= len(exp) and act.startswith(exp)


def fetch_stores():
    last_err = None
    for ep in endpoints:
        try:
            with urllib.request.urlopen(f"{ep}/pd/api/v1/stores", timeout=5) as resp:
                data = json.loads(resp.read().decode("utf-8"))
            return data.get("stores") or []
        except Exception as exc:
            last_err = exc
    raise RuntimeError(f"cannot reach PD ({', '.join(endpoints)}): {last_err}")


def check_status_metrics(status_addr):
    ep = status_addr.strip()
    if not ep:
        return
    if ep.startswith("0.0.0.0:"):
        ep = "127.0.0.1:" + ep.split(":", 1)[1]
    if not ep.startswith(("http://", "https://")):
        ep = "http://" + ep
    ep = ep.rstrip("/")
    try:
        with urllib.request.urlopen(f"{ep}/metrics", timeout=2) as resp:
            body = resp.read().decode("utf-8", errors="replace")
    except Exception as exc:
        if req_status:
            sys.exit(f"tikv-patched: cannot reach TiKV status endpoint {ep}/metrics: {exc}")
        return
    infos = []
    for line in body.splitlines():
        m = re.match(r'^tikv_server_info\{([^}]*)\}\s+', line)
        if m:
            infos.append(dict(re.findall(r'(\w+)="([^"]*)"', m.group(1))))
    if not infos and req_status:
        sys.exit(f"tikv-patched: {ep}/metrics missing tikv_server_info")
    for labels in infos:
        if not version_matches(exp_ver, labels.get("version", "")) or not commit_matches(exp_commit, labels.get("hash", "")):
            sys.exit(
                f"tikv-patched: TiKV {ep}/metrics reports version={labels.get('version', '')!r} "
                f"hash={labels.get('hash', '')!r}, expected version={exp_ver!r} commit={exp_commit!r} (tag {tag})"
            )


deadline = time.monotonic() + wait_s
active = []
last_err = None
while True:
    try:
        raw_stores = fetch_stores()
        active = [s.get("store", {}) for s in raw_stores if s.get("store", {}).get("state_name") != "Tombstone"]
        if active:
            break
    except Exception as exc:
        last_err = exc
    if time.monotonic() >= deadline:
        if last_err is not None and not active:
            sys.exit(f"tikv-patched: {last_err}")
        sys.exit(f"tikv-patched: no active TiKV stores registered in PD ({', '.join(endpoints)})")
    time.sleep(0.5)

checked = []
for st in active:
    sid = st.get("id", "?")
    addr = st.get("address", "?")
    gv = str(st.get("version", ""))
    gh = str(st.get("git_hash", ""))
    if not version_matches(exp_ver, gv) or not commit_matches(exp_commit, gh):
        sys.exit(
            f"tikv-patched: TiKV store {sid} ({addr}) runs version={gv!r} git_hash={gh!r}; "
            f"expected version={exp_ver!r} commit={exp_commit!r} (tag {tag}). "
            f"Restart the cluster with tools/tikv-patched.sh."
        )
    status_addr = str(st.get("status_address", ""))
    if status_addr and st.get("state_name") == "Up":
        check_status_metrics(status_addr)
    checked.append({"id": sid, "address": addr, "version": gv.lstrip("v"), "commit": gh})

info = {
    "repo": repo,
    "tag": tag,
    "version": checked[0]["version"],
    "commit": checked[0]["commit"],
    "stores": len(checked),
}
if inspect_json:
    print(json.dumps(info))
else:
    print(f"{info['version']} ({info['commit'][:9]}, tag {tag}, {info['stores']} store(s))")
PY
}

verify_log() {
    python3 -I - "$1" "$version" "$commit" "$tag" <<'PY'
import os
import re
import sys

path, exp_ver, exp_commit, tag = sys.argv[1:5]
if not os.path.isfile(path):
    sys.exit(f"tikv-patched: log file not found: {path}")

ver = None
commit = None
with open(path, encoding="utf-8", errors="replace") as f:
    for line in f:
        m = re.search(r"Release Version:\s*([^\s\"'\]]+)", line)
        if m:
            ver = m.group(1)
        m = re.search(r"Git Commit Hash:\s*([^\s\"'\]]+)", line)
        if m:
            commit = m.group(1)

if not ver or not commit:
    sys.exit(f"tikv-patched: {path} does not contain Release Version and Git Commit Hash lines")


def commit_matches(exp, act):
    exp = exp.lower()
    act = act.lower()
    if not re.fullmatch(r"[0-9a-f]{7,40}", act):
        return False
    if len(exp) == 40 and len(act) == 40:
        return act == exp
    return len(act) >= len(exp) and act.startswith(exp)


if ver.lstrip("v") != exp_ver.lstrip("v") or not commit_matches(exp_commit, commit):
    sys.exit(
        f"tikv-patched: {path} reports version={ver!r} git_hash={commit!r}; "
        f"expected version={exp_ver!r} commit={exp_commit!r} (tag {tag})"
    )

print(f"{ver.lstrip('v')} ({commit[:9]}, tag {tag})")
PY
}

ensure_bin() {
    local check_only=$1 platform sha_key expected_sha
    if [ -n "${TIDB_RS_TIKV_BIN:-}" ]; then
        verify_bin "$TIDB_RS_TIKV_BIN"
        printf '%s\n' "$TIDB_RS_TIKV_BIN"
        return 0
    fi
    if [ -n "${TIDB_RS_TIKV_SRC:-}" ]; then
        local cand=
        for p in "$TIDB_RS_TIKV_SRC/bin/tikv-server" "$TIDB_RS_TIKV_SRC/target/release/tikv-server"; do
            if [ -x "$p" ]; then
                cand=$p
                break
            fi
        done
        [ -n "$cand" ] || fail "TIDB_RS_TIKV_SRC=$TIDB_RS_TIKV_SRC has no built bin/tikv-server or target/release/tikv-server"
        verify_bin "$cand"
        printf '%s\n' "$cand"
        return 0
    fi

    platform=$(detect_platform)
    sha_key="sha256_${platform//-/_}"
    expected_sha=$(field "$sha_key" | tr '[:upper:]' '[:lower:]')

    local cache_root dest_dir dest_bin stamp_file want_stamp
    cache_root=${TIDB_RS_TIKV_CACHE:-$HOME/.cache/tidb-rs/tikv}
    dest_dir=$cache_root/$tag
    dest_bin=$dest_dir/tikv-server
    stamp_file=$dest_dir/.tikv-patched-stamp
    want_stamp="$repo $tag $version $commit $platform $expected_sha"

    if [[ "$expected_sha" =~ ^[0-9a-f]{64}$ ]] && [ -f "$stamp_file" ] \
        && [ "$(cat "$stamp_file")" = "$want_stamp" ] && [ -x "$dest_bin" ]; then
        verify_bin "$dest_bin"
        printf '%s\n' "$dest_bin"
        return 0
    fi

    if [ "$check_only" = 1 ]; then
        if [ -f "$stamp_file" ]; then
            fail "$dest_bin is stale (stamp: $(cat "$stamp_file"); expected: $want_stamp)"
        fi
        fail "patched tikv-server is not cached at $dest_bin; run tools/tikv-patched.sh or set TIDB_RS_TIKV_BIN"
    fi

    if ! [[ "$expected_sha" =~ ^[0-9a-f]{64}$ ]]; then
        fail "no release SHA-256 is pinned for $platform in $version_file ($sha_key=${expected_sha:-unset}).
Publish release $tag from $repo ($branch @ $commit) and update $version_file,
or set TIDB_RS_TIKV_BIN=/path/to/patched/tikv-server (or TIDB_RS_TIKV_SRC=/path/to/tikv)."
    fi

    local asset tarball url_base
    asset="tikv-server-$tag-$platform.tar.gz"
    mkdir -p "$cache_root"
    tarball="$cache_root/$asset"

    if ! { [ -f "$tarball" ] && [ "$(sha256_of "$tarball")" = "$expected_sha" ]; }; then
        rm -f "$tarball.part"
        if [ -n "${TIDB_RS_TIKV_URL_BASE:-}" ]; then
            url_base=${TIDB_RS_TIKV_URL_BASE%/}
            echo "tikv-patched: fetching $asset from $url_base" >&2
            curl -fsSL --retry 3 -o "$tarball.part" "$url_base/$asset" \
                || fail "failed to download $url_base/$asset"
        else
            echo "tikv-patched: fetching $asset from github.com/$repo release $tag" >&2
            if ! curl -fsSL --retry 3 -o "$tarball.part" \
                "https://github.com/$repo/releases/download/$tag/$asset"; then
                if command -v gh >/dev/null 2>&1; then
                    local asset_id
                    asset_id=$(gh api "repos/$repo/releases/tags/$tag" \
                        --jq ".assets[] | select(.name==\"$asset\") | .id" 2>/dev/null || true)
                    [ -n "$asset_id" ] || fail "release $tag in $repo has no asset $asset"
                    gh api -H 'Accept: application/octet-stream' \
                        "repos/$repo/releases/assets/$asset_id" > "$tarball.part" \
                        || fail "failed to download $asset via GitHub API"
                else
                    fail "failed to download https://github.com/$repo/releases/download/$tag/$asset"
                fi
            fi
        fi
        local got_sha
        got_sha=$(sha256_of "$tarball.part")
        if [ "$got_sha" != "$expected_sha" ]; then
            rm -f "$tarball.part"
            fail "sha256 mismatch for $asset: got $got_sha, expected $expected_sha"
        fi
        mv "$tarball.part" "$tarball"
    fi

    local extracted
    _tikv_patched_work="$dest_dir.tmp.$$"
    cleanup_work() {
        if [ -n "${_tikv_patched_work:-}" ] && [ -e "$_tikv_patched_work" ]; then
            rm -rf "$_tikv_patched_work" 2>/dev/null || true
        fi
    }
    trap cleanup_work EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    rm -rf "$_tikv_patched_work"
    mkdir -p "$_tikv_patched_work"
    tar -xzf "$tarball" -C "$_tikv_patched_work"
    if [ -f "$_tikv_patched_work/tikv-server" ]; then
        extracted="$_tikv_patched_work/tikv-server"
    elif [ -f "$_tikv_patched_work/bin/tikv-server" ]; then
        extracted="$_tikv_patched_work/bin/tikv-server"
    else
        fail "unexpected archive layout in $asset (no tikv-server binary found)"
    fi
    chmod 0755 "$extracted"
    if [ "$extracted" != "$_tikv_patched_work/tikv-server" ]; then
        mv "$extracted" "$_tikv_patched_work/tikv-server"
    fi
    verify_bin "$_tikv_patched_work/tikv-server"
    printf '%s\n' "$want_stamp" > "$_tikv_patched_work/.tikv-patched-stamp"
    rm -rf "$dest_dir"
    mv "$_tikv_patched_work" "$dest_dir"
    _tikv_patched_work=
    trap - EXIT INT TERM

    printf '%s\n' "$dest_bin"
}

case "$mode" in
    ensure) ensure_bin 0 ;;
    check) ensure_bin 1 ;;
    check-bin)
        verify_bin "$arg"
        echo "ok: $arg ($version, ${commit:0:9}, tag $tag)"
        ;;
    check-status) verify_status "$arg" ;;
    check-pd) verify_pd "$arg" 0 ;;
    inspect-pd) verify_pd "$arg" 1 ;;
    check-log) verify_log "$arg" ;;
    print-info)
        if [ "$json_out" = 1 ]; then
            python3 -I - "$repo" "$branch" "$tag" "$version" "$commit" <<'PY'
import json, sys
repo, branch, tag, version, commit = sys.argv[1:6]
print(json.dumps({"repo": repo, "branch": branch, "tag": tag, "version": version, "commit": commit}))
PY
        else
            echo "$version (${commit:0:9}, tag $tag)"
        fi
        ;;
esac
