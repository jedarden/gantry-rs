#!/usr/bin/env sh
# gantry-exec.sh — POSIX reference executor for the command-template backend.
#
# Executable documentation of the remote contract (plan §Components 5, §Phase 2
# "SSH-first onboarding"): a run is addressed by content — the epoch ref
# `refs/gantry/<epoch>-<sha>` the client pushed — never by a branch (S-1);
# the executor clones that ref into a throwaway worktree and runs exactly the
# argv the caller typed.
#
# The command contract is exit-code-only (plan §"command"): 0 = pass, 1 =
# test-failure, >=2 = infra-failure — an InfraFailure makes the client
# degrade to a capped local run whose exit code wins. wait therefore
# normalizes: a suite that ran and failed exits 1 (never cargo's raw 101,
# which would misread a real test failure as infra and trigger that local
# re-run); a suite killed by a signal (raw >=128) exits 2, infra-shaped like
# the OOM classification the Argo template carries in verdict.json; and this
# script uses exit 2 for everything it could not honestly run (missing ref,
# failed fetch, undecodable args) — a verdict the remote never earned is
# worse than no remote verdict. The suite's raw exit code is kept in the
# run's exit-code file. verdict.json is the Argo template's richer channel;
# this script deliberately does not produce one.
#
# Usage (matches the shipped command-template preset, src/backend/command.rs):
#   gantry-exec.sh submit <repo_url> <sha> <args_json>   prints handle
#   gantry-exec.sh wait <handle>                         runs the suite, exits with its code
#   gantry-exec.sh logs <handle>                         prints the captured log (best-effort)
#
# submit stores the request; wait executes it. The client streams `logs`
# (empty until wait has run) and takes the verdict from wait's exit code.
#
# Dependencies: git, a cargo toolchain. jq is required only when args_json
# is a non-empty array (argv decoding); "[]" needs nothing beyond git.
#
# Environment:
#   GANTRY_EXEC_STATE_DIR  run state (default /tmp/gantry-runs; each handle
#                          dir holds repo/rev/args/output.log/exit-code)
#   GANTRY_EXEC_CARGO      cargo to run (default "cargo"; point it at the
#                          real toolchain when a PATH shim is installed)

set -eu

STATE_DIR="${GANTRY_EXEC_STATE_DIR:-/tmp/gantry-runs}"
CARGO_BIN="${GANTRY_EXEC_CARGO:-cargo}"

die() {
    printf '[gantry-exec] %s\n' "$1" >&2
    exit "${2:-2}"
}

# S-5: the client may hand this script a credentialed URL — git needs it to
# fetch — but the log never carries it. Mask any userinfo before echoing the
# remote (the Argo template redacts identically; the client redacts again at
# the runlog layer).
redact_url() {
    printf '%s' "$1" | sed -E 's#(//)[^/@]+@#\1[redacted]@#'
}

usage() {
    printf 'usage: %s submit <repo_url> <sha> <args_json>\n' "$0" >&2
    printf '       %s wait <handle>\n' "$0" >&2
    printf '       %s logs <handle>\n' "$0" >&2
    exit 2
}

# Handles name a run directory under STATE_DIR, so they must be a single
# path-free component: submit generates them, but wait/logs take them from a
# caller, and a crafted "../" handle would walk out of the state dir.
check_handle() {
    case "$1" in
    "" | *[!0-9A-Za-z._-]* | .. | .) usage ;;
    esac
}

# submit: record the run request, emit the handle on stdout.
cmd_submit() {
    [ $# -eq 3 ] || usage
    handle="run-$(date +%s)-$$"
    dir="$STATE_DIR/$handle"
    mkdir -p "$dir"
    # One file per field, never a sourced key=value file: repo URLs and args
    # arrive from untrusted repos, and sourcing them would hand them a shell
    # (S-4). cat + command substitution is byte-exact for JSON (one line).
    printf '%s\n' "$1" >"$dir/repo"
    printf '%s\n' "$2" >"$dir/rev"
    printf '%s\n' "$3" >"$dir/args"
    printf '%s\n' "$handle"
}

# fetch_and_checkout <repo> <rev> <workdir>: fetch the epoch refspace and
# check out the ref whose name ends in the requested sha. A bare-sha fetch
# would need uploadpack.allowAnySHA1InWant on the server, which gantry
# cannot assume; fetching refs/gantry/* needs no server opt-in. --depth=1
# keeps it bounded; the client-side GC sweep keeps the refspace small.
fetch_and_checkout() {
    repo=$1
    rev=$2
    workdir=$3
    git init -q "$workdir"
    # Hidden content-addressed refs only (INV-2: no branch ever moves).
    git -C "$workdir" fetch -q --depth=1 "$repo" \
        '+refs/gantry/*:refs/gantry/*' ||
        die "fetch of refs/gantry/* from $(redact_url "$repo") failed"
    # The || true masks only for-each-ref dying on SIGPIPE when a very large
    # refspace outgrows the pipe after head has its line — any real failure
    # also empties the variable, and the guard below reports that honestly
    # (same rationale as the Argo template's clone step).
    gantry_ref=$(git -C "$workdir" for-each-ref --format='%(refname)' \
        --sort=refname "refs/gantry/*-$rev" | head -n 1 || true)
    [ -n "$gantry_ref" ] ||
        die "no refs/gantry/* ref for $rev on $(redact_url "$repo") (not pushed, or GC'd)"
    git -C "$workdir" checkout -q --detach "$gantry_ref"
}

# wait: execute the stored run; exit with the suite's own exit code.
cmd_wait() {
    [ $# -eq 1 ] || usage
    check_handle "$1"
    dir="$STATE_DIR/$1"
    # cd-into-it is the existence check and canonicalizes a relative
    # GANTRY_EXEC_STATE_DIR in one step: wait enters a worktree below, and a
    # relative state path would silently resolve against it from there.
    dir=$(cd "$dir" && pwd) || die "unknown run: $1"
    # A missing field file means the record was truncated (submit died
    # mid-write, full disk). Exit 2 — the suite never ran, so a test-failure
    # verdict (1) would be a lie the client acts on.
    [ -f "$dir/repo" ] && [ -f "$dir/rev" ] && [ -f "$dir/args" ] ||
        die "incomplete run record: $1" 2
    repo=$(cat "$dir/repo")
    rev=$(cat "$dir/rev")
    args_json=$(cat "$dir/args")
    # The rev is a content address, never a ref name: hex-only keeps a
    # hostile handle from glob-matching a different epoch ref through the
    # for-each-ref suffix pattern below ('*' and '[' are wildcards there),
    # and pins S-1 — no spelling of this asks the executor for a branch.
    case "$rev" in
    "" | *[!0-9a-f]*) die "rev must be a hex commit sha: $rev" ;;
    esac

    work_root=""
    trap '[ -n "$work_root" ] && rm -rf "$work_root"' EXIT
    trap '[ -n "$work_root" ] && rm -rf "$work_root"; exit 130' INT TERM
    # The worktree's ancestors must be manifest-free, or cargo resolves the
    # nearest ancestor Cargo.toml as its workspace root and refuses to run
    # ("current package believes it's in a workspace when it's not") — a 101
    # the ladder would report as a test failure the run never earned. Shared
    # machines collect stray /tmp/Cargo.toml, so wait wraps the worktree in
    # its own throwaway workspace root listing the worktree as a member:
    # cargo resolves the workspace there (a fetched repo that is itself a
    # workspace resolves to its own root manifest first) and whatever sits
    # above /tmp becomes irrelevant. An `exclude` entry does NOT work here —
    # cargo keeps walking past an excluded candidate.
    work_root=$(mktemp -d "${TMPDIR:-/tmp}/gantry-exec.XXXXXX")
    work="$work_root/run"
    mkdir "$work"
    # resolver = "2" matches what the edition-2021 members imply; without it
    # cargo prints a resolver warning into every run log.
    printf '[workspace]\nmembers = ["run"]\nresolver = "2"\n' >"$work_root/Cargo.toml"

    fetch_and_checkout "$repo" "$rev" "$work"

    # Run inside the fetched worktree: the suite must test the commit the
    # client pushed, never whatever tree `wait` was invoked from — on a real
    # remote that cwd is an unrelated home directory, and testing it would be
    # the silent wrong-tree verdict AS-4 exists to prevent.
    cd "$work" || die "cannot enter worktree $work"

    # Decode args_json into positional parameters. jq's @sh emits each
    # element shell-quoted (quotes, newlines, metacharacters stay data), so
    # the eval sees only jq-quoted text — the args are never interpolated
    # raw (S-4), and hostile argv round-trips byte-exact (EC-06 / INV-5).
    set --
    case "$args_json" in
    "" | "[]") : ;;
    *)
        command -v jq >/dev/null 2>&1 ||
            die "jq is required to decode non-empty args_json"
        printf '%s' "$args_json" |
            jq -e 'type == "array" and all(.[]; type == "string")' >/dev/null ||
            die "args_json must be a JSON array of strings"
        quoted=$(printf '%s' "$args_json" | jq -j 'map(@sh) | join(" ")')
        [ -n "$quoted" ] && eval "set -- $quoted"
        ;;
    esac

    # Exactly the caller's argv, plus the fixed subcommand: v1 intercepts
    # `cargo test` only, so the client sends args after the subcommand.
    log="$dir/output.log"
    set +e
    "$CARGO_BIN" test "$@" >"$log" 2>&1
    exit_code=$?
    set -e
    printf '%s\n' "$exit_code" >"$dir/exit-code"
    # Normalize to the contract ladder rather than passing the suite's exit
    # code through: cargo reports a failing suite as 101 and a signal-killed
    # one as >=128, and passed through raw, 101 would read as InfraFailure —
    # the client would degrade to a capped local run and re-run a real test
    # failure, which the verdict ladder exists to prevent. The suite's own
    # code stays in exit-code for the record.
    if [ "$exit_code" -eq 0 ]; then
        exit 0
    elif [ "$exit_code" -ge 128 ]; then
        die "suite killed by signal (raw exit $exit_code)" 2
    elif [ "$exit_code" -eq 127 ]; then
        # 127 is the shell's command-not-found: the toolchain itself never
        # ran, so a test-failure verdict (1) would be a lie about a suite
        # that never executed — infra (2), whose capped local fallback still
        # lands the caller a real result (a cargo suite failure is 101, and
        # a missing command *inside* a test fails that test rather than
        # surfacing as the run's own exit code). The Argo template's final
        # ladder normalizes 127 identically — contract parity between the
        # two executors.
        die "cargo not runnable on this remote (exit 127): check GANTRY_EXEC_CARGO / PATH" 2
    else
        exit 1
    fi
}

# logs: print the captured log. Best-effort per the RemoteBackend contract —
# nothing streamed yet (wait has not run) is an empty success, not an error.
cmd_logs() {
    [ $# -eq 1 ] || usage
    check_handle "$1"
    log="$STATE_DIR/$1/output.log"
    [ -f "$log" ] && cat "$log"
    exit 0
}

case "${1:-}" in
submit)
    shift
    cmd_submit "$@"
    ;;
wait)
    shift
    cmd_wait "$@"
    ;;
logs)
    shift
    cmd_logs "$@"
    ;;
*)
    usage
    ;;
esac
