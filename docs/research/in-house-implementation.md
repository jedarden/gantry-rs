# The In-House Implementation (gantry's ancestor)

Gantry is an extraction of a two-script system running in production since mid-2026 on two shared Linux workstations (a Hetzner EX44 and a lab box), where it transparently serves both interactive use and a fleet of up to ~20 concurrent AI coding agents (NEEDLE workers) that inherit it purely via PATH. This document records the system as-is, its remote contract, and the operational lessons that directly shaped gantry's design.

## Component 1: the PATH shim (`~/.local/bin/cargo`)

```bash
#!/usr/bin/env bash
# Cargo wrapper: offloads heavy commands to iad-ci, limits local runs via cgroup.
REAL_CARGO="$HOME/.cargo/bin/cargo"
SUBCOMMAND="${1:-}"

local_limited() {
    if systemd-run --scope --user --description="cargo $SUBCOMMAND" \
            -p CPUQuota=200% -p MemoryMax=6G -p MemorySwapMax=0 -q -- \
            "$REAL_CARGO" "$@" 2>/dev/null; then
        return $?
    fi
    # systemd-run unavailable (container/non-session env) — run directly
    exec "$REAL_CARGO" "$@"
}

if [[ "$SUBCOMMAND" == "test" ]]; then
    if git rev-parse --git-dir &>/dev/null 2>&1 && \
       git remote get-url origin &>/dev/null 2>&1; then
        exec cargo-remote "$@"
    fi
fi

local_limited "$@"
```

Notes:
- Every non-test cargo invocation is cgroup-capped (CPUQuota=200%, MemoryMax=6G, no swap). These values are production-proven on a shared box.
- `cargo nextest run` is deliberately **not** intercepted (subcommand is `nextest`, not `test`) — the remote builder image may lack nextest; it runs locally under the cap.
- The real cargo is hardcoded to `~/.cargo/bin/cargo` (rustup's shim). Gantry must resolve this dynamically instead.

## Component 2: the submitter (`~/.local/bin/cargo-remote`)

```bash
#!/usr/bin/env bash
# Submit cargo test to iad-ci cluster via rust-verify WorkflowTemplate.
# Falls back to local (CPU-limited) on any submission failure.
set -euo pipefail

REAL_CARGO="$HOME/.cargo/bin/cargo"
KUBECONFIG_PATH="${KUBECONFIG:-$HOME/.kube/iad-ci.kubeconfig}"
KCT="kubectl --kubeconfig=$KUBECONFIG_PATH"
TEMPLATE="rust-verify"

local_limited() {
    echo "[cargo-remote] falling back to local (CPUQuota=200%, MemoryMax=6G)"
    exec systemd-run --scope --user -p CPUQuota=200% -p MemoryMax=6G -p MemorySwapMax=0 -q -- \
        "$REAL_CARGO" "$@"
}

# Require a git repo with a remote
REPO=$(git remote get-url origin 2>/dev/null) || { echo "[cargo-remote] no git remote"; local_limited "$@"; }

# Uncommitted changes can't be cloned — fall back
if ! git diff --quiet HEAD 2>/dev/null; then
    echo "[cargo-remote] uncommitted changes detected — running locally"
    local_limited "$@"
fi

REVISION=$(git rev-parse HEAD)
# TEST_ARGS = everything after the subcommand (e.g. "test -- --nocapture")
TEST_ARGS="${*:2}"

echo "[cargo-remote] pushing $REVISION..."
git push origin HEAD --quiet 2>&1 || { echo "[cargo-remote] push failed"; local_limited "$@"; }

echo "[cargo-remote] submitting $TEMPLATE (repo=${REPO} rev=${REVISION:0:8} args='${TEST_ARGS}')..."

WF_OUT=$($KCT create -n argo-workflows -o name -f - 2>&1 <<EOF
apiVersion: argoproj.io/v1alpha1
kind: Workflow
metadata:
  generateName: cargo-remote-
  namespace: argo-workflows
spec:
  workflowTemplateRef:
    name: $TEMPLATE
  arguments:
    parameters:
    - name: repo
      value: "$REPO"
    - name: revision
      value: "$REVISION"
    - name: test-args
      value: "$TEST_ARGS"
EOF
) || { echo "[cargo-remote] submit failed: $WF_OUT"; local_limited "$@"; }

WF_NAME="${WF_OUT#workflow.argoproj.io/}"
echo "[cargo-remote] workflow: $WF_NAME"
echo "[cargo-remote] waiting for pod..."

# Poll for a running pod (up to 3 minutes)
POD=""
for i in $(seq 1 90); do
    POD=$($KCT get pods -n argo-workflows \
        -l "workflows.argoproj.io/workflow=$WF_NAME" \
        -o jsonpath='{.items[?(@.status.phase!="Pending")].metadata.name}' 2>/dev/null | awk '{print $1}')
    [[ -n "$POD" ]] && break
    sleep 2
done

if [[ -z "$POD" ]]; then
    echo "[cargo-remote] pod never started — check Argo UI"
    exit 1
fi

echo "[cargo-remote] streaming logs from $POD..."
$KCT logs -n argo-workflows "$POD" -c main -f 2>/dev/null || true

# Poll for final workflow phase (up to 35 minutes beyond log stream)
for i in $(seq 1 210); do
    PHASE=$($KCT get workflow "$WF_NAME" -n argo-workflows \
        -o jsonpath='{.status.phase}' 2>/dev/null)
    case "$PHASE" in
        Succeeded) echo "[cargo-remote] PASSED"; exit 0 ;;
        Failed|Error) echo "[cargo-remote] FAILED (see $WF_NAME in Argo UI)"; exit 1 ;;
    esac
    sleep 10
done

echo "[cargo-remote] timed out — workflow: $WF_NAME"
exit 1
```

## Component 3: the remote contract (`rust-verify` WorkflowTemplate)

Lives in the GitOps repo (`declarative-config`, `k8s/iad-ci/argo-workflows/rust-verify-workflowtemplate.yml`), synced by ArgoCD. The contract gantry's `argo` backend must speak, and the basis for the reference template in `contrib/argo/`:

- **Input parameters:** `repo` (clone URL), `revision` (branch/sha — NEEDLE uses `wip/<worker>/<bead>` branches), `test-args` (string appended to `cargo test`), `builder-image` (default: a `needle-ci-builder:with-deps` image with toolchain + common deps pre-baked).
- **Output parameters:** `result` (`pass`/`fail` written to a file) and `output` (full combined log) — consumable by parent workflows (NEEDLE's per-bead verification gate reuses this same template).
- **Pipeline inside the pod:** clone (with optional GH/Forgejo token injection for private repos) → optional sccache→Garage-S3 warm cache (env gated on a secret existing; cold build otherwise) → `[profile.test] opt-level = 2` appended → `cargo check --all-targets` (fast fail) → `cargo clippy --all-targets -- -D warnings` → `cargo test $TEST_ARGS`.
- **Isolation:** `activeDeadlineSeconds: 1800`; resources 2–4 CPU, 4–8Gi memory. The 8Gi limit is the OOM guard — a runaway test kills the pod, not the workstation. This isolation is the original reason the system exists.
- **Note:** the template runs check+clippy+test, i.e. remote `cargo test` is actually a stricter *verify* gate than local `cargo test`. Gantry should make the remote command an explicit template concern and document the strictness delta.

## Operational lessons (each one shaped a gantry requirement)

1. **The push footgun (2026-07-19).** `git push origin HEAD` pushed an unfinished local `main` to origin, which a server-side mirror then propagated to a public GitHub repo. → DD-2: push only `refs/gantry/<sha>`, never branches.
2. **Untracked-files gap.** `git diff --quiet HEAD` passes when new untracked files exist, so a freshly added test file silently doesn't exist remotely. Never bit hard in practice only because NEEDLE workers commit before testing — a discipline gantry can't assume. → DD-3: `git status --porcelain`.
3. **YAML string splicing.** `TEST_ARGS` is interpolated raw into the Workflow manifest heredoc; args containing quotes/colons can corrupt the manifest. → DD-7: serialized payloads, argv arrays end-to-end.
4. **podGC races.** `podGC: OnPodCompletion` deletes pods the instant they finish: fast runs sometimes yield no streamed logs at all, and the phase poll (not the log tail) is what's authoritative. The template's `output` parameter exists partly to compensate. → backend contract: verdict from workflow status; logs best-effort; fetch `outputs.parameters.output` as the fallback log source.
5. **Pod-never-started is `exit 1`, not fallback.** If the cluster can't schedule within 3 min the script exits 1 *without* running tests — an infrastructure failure masquerading as a test failure. → DD-4: infra failures fall back to local; only real test failures are terminal.
6. **cgroup values are proven.** CPUQuota=200% / MemoryMax=6G / no swap has kept a ~20-agent shared box alive through daily runaway builds (previously: load 190–204 incidents from unconstrained builds). Keep as defaults.
7. **PATH inheritance is the whole trick.** NEEDLE workers were never told about offloading; they just run `cargo test`. Transparency is the product's proven core value.
8. **Toolchain redirects interact.** The workstation's `~/.cargo/config.toml` redirects `target-dir` to a shared `~/target/` — local fallback builds land there, remote builds don't. Gantry must not assume repo-local `target/` in any disk accounting.
9. **Concurrent submitters exist.** Multiple agents in one repo can race `cargo test` on the same commit; today that's two identical workflows. → dedup/join feature.
10. **Warm cache is make-or-break.** Cold remote clones + cold builds made early remote runs slower than local; sccache→S3 on the cluster side is what made offload a net win. The client should surface backend cache stats when available (the template already prints `sccache --show-stats`).
