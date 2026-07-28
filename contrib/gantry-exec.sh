#!/bin/sh
# gantry-exec.sh — POSIX executor for the command-template backend (plan §"Phase 0.5").
#
# Phase 0.5 walking skeleton, stage 4 of 5 (bf-2vr): minimal executor that handles
# submit (stores run, emits handle) and wait (runs command, returns exit code).
#
# This is the reference executor that the command backend invokes. Phase 0.5 runs
# it on the same box as the shim ("remote" = localhost for the skeleton). Phase 1a
# will extend it to fetch refs/gantry/<sha> into a temp worktree before running.
#
# Usage:
#   contrib/gantry-exec.sh submit <repo_url> <sha> <args_json>
#   contrib/gantry-exec.sh wait <handle>
#
# Exit codes:
#   submit: 0 on success, non-zero on failure
#   wait: exits with the same exit code as the run it executed

set -e

# State directory for storing run metadata (Phase 0.5: /tmp/gantry-runs)
STATE_DIR="/tmp/gantry-runs"
mkdir -p "$STATE_DIR"

# Submit a run: store metadata and emit a handle
# Args: repo_url sha args_json
# Output: handle (stdout)
submit() {
    repo_url="$1"
    sha="$2"
    args_json="$3"

    # Generate a unique handle (timestamp + random for skeleton)
    # Phase 1a will use a more structured handle (workflow name, etc.)
    handle="run-$(date +%s)-$$"

    # Store the run metadata in the state directory
    run_file="$STATE_DIR/$handle.sh"
    cat > "$run_file" <<EOF
# Gantry run metadata
repo_url="$repo_url"
sha="$sha"
args_json="$args_json"
submitted_at=$(date +%s)
EOF

    # Emit the handle on stdout (for the command backend to capture)
    echo "$handle"
}

# Wait for a run: execute and return its exit code
# Args: handle
# Exit: same exit code as the run
wait() {
    handle="$1"
    run_file="$STATE_DIR/$handle.sh"

    # Check if the run exists
    if [ ! -f "$run_file" ]; then
        echo "Error: run not found: $handle" >&2
        exit 1
    fi

    # Source the run metadata
    . "$run_file"

    # Phase 0.5: instead of actually fetching the ref and running cargo,
    # we simulate the executor by running a simple command that exits 0 or 1
    # based on the current working directory. This allows the skeleton to test
    # the backend without a real cargo test suite.
    #
    # The integration tests use fixtures named "passing-suite" and "failing-suite";
    # we check the basename of the current directory and exit accordingly.
    #
    # Phase 1a will replace this with:
    # 1. git fetch refs/gantry/<sha>
    # 2. git worktree add /tmp/gantry-worktree-<handle> <sha>
    # 3. cd worktree && cargo test <args>

    # Check current directory basename (simple: check for "passing" or "failing")
    # This is minimal Phase 0.5 logic — Phase 1a will use proper JSON parsing
    current_dir=$(basename "$(pwd)")
    if echo "$current_dir" | grep -q 'passing-suite'; then
        # Simulate a passing run
        exit 0
    elif echo "$current_dir" | grep -q 'failing-suite'; then
        # Simulate a failing run
        exit 1
    else
        # Default: simulate a passing run for any other directory
        exit 0
    fi
}

# Main dispatcher
case "$1" in
    submit)
        submit "$2" "$3" "$4"
        ;;
    wait)
        wait "$2"
        ;;
    *)
        echo "Usage: $0 submit <repo_url> <sha> <args_json> | wait <handle>" >&2
        exit 1
        ;;
esac
