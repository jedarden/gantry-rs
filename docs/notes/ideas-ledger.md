# Ideas Ledger

Every ideation run against `docs/plan/plan.md` records ALL considered ideas here — winners and losers alike — so future runs dedupe against history and dead ideas stay dead unless their kill objection stops holding.

## Run 2026-07-21 — pool 104, kept 10

Stats: 104 generated (8 lenses × 13) → 68 canonical after duplicate-merge → 30 triage survivors + 2 hybrids → 15 advancers (pairwise ranking, max 2/cluster) → 1 killed → gap round resurrected 1 (verdict-security territory) → **10 finalists**. Generation ran as blind 8-lens fan-out; all narrowing done inline single-context.

**Adoption (2026-07-21):** finalists 1–9 ADOPTED into plan.md (Q-2/Q-3/Q-7 resolved; components, failure modes, CLI surface, and phases updated; owner directive on #5: onboarding must be smooth and automatic, with agent-explorable CLI diagnostics). Finalist 10 (`gantry demo` = exit-code oracle) DECLINED by owner. Benched items not adopted.

### Finalists

1. **Faithful-argv remote (resolves Q-2)** — remote executes exactly the argv the caller typed; check/clippy gates are trusted-config opt-in, reported as separately-labeled `[gantry] gate:` results, never conflated with the test verdict. Cluster: strictness. *(merged 5: test-only default / remote-mirrors-local / strictness ladder / advisory extras / faithful-argv)*
2. **verdict.json + OOM/deadline are infra, never test failures** — remote contract emits a tiny structured verdict (phase, exit, oom, deadline from workflow node status); pod OOMKilled/deadline-exceeded classify as InfraFailure→fallback instead of masquerading as TestFailure. Cluster: verdict integrity. *(merged 2)*
3. **Write-ahead intent ledger** — append an OPEN intent record to runs.jsonl before dispatch; every exit path must close it with a verdict; `doctor` flags orphaned intents as lost runs. Makes a silently-skipped run a detectable dangling row. Cluster: verdict integrity. *(merged 4)*
4. **Fallback admission semaphore** — flock-based token files (default 3 concurrent) gate capped local fallback runs, queueing loudly, so a cluster outage under 20 agents can't recreate the runaway-build meltdown through gantry's own fallback path. Cluster: degraded-path load control. *(merged 5, incl. jobserver/CSMA variants)*
5. **SSH-first onboarding (hybrid)** — promote "remote = any SSH box" to the flagship v1 path, implemented NOT as a third backend but as a shipped command-template preset + the ~150-line contrib shell executor (which doubles as the remote-contract reference). Argo becomes the "advanced" backend. Cluster: adoption. *(merged 3 + hybrid)*
6. **`gantry why` / `--explain`** — replay the last run's full gate/decision trace from runs.jsonl, plus a dry-run mode showing what WOULD happen (gates, backend, exact ref) without touching the network. Cluster: explainability. *(merged 4)*
7. **Config last-known-good + loud debt counter (resolves Q-7)** — on broken config, serve the persisted last-good snapshot (DNS serve-stale transplant) with an escalating `[gantry] config broken since <ts>` banner on every run; never fabricate behavior from a half-parsed file, never rot silently. Cluster: config failure. *(merged 6)*
8. **Lease-based refs/gantry GC (resolves Q-3)** — refs carry an epoch (`refs/gantry/<epoch>-<sha>`) plus client-side lease records tied to run-ids; each push opportunistically sweeps expired refs whose runs are terminal — no cron, no daemon, no race against in-flight clones. Cluster: ref hygiene. *(merged 5)*
9. **`doctor --e2e` canary + `--drill` fault injection** — push a known-good fixture ref through the real pipeline and verify the round-trip verdict; drill mode injects a synthetic InfraFailure and asserts the whole loud-degrade chain fires. The fallback path gets fire-drilled on demand, not discovered broken during an outage. Cluster: self-verification. *(merged 3)*
10. **`gantry demo` = exit-code oracle (hybrid)** — one fixture crate with deterministic pass/fail/panic/timeout paths serves both the 5-minute user demo (proves exit-code fidelity end-to-end) and the CI conformance suite that release-gates every backend against byte-identical exit codes. Cluster: adoption/trust. *(merged 3 + hybrid)*

### Survived the kill pass, not selected (bench for next run)

- **Full-log artifact channel (Q-6)** — template tees full log to S3/artifact; output-param carries tail + pointer; truncation watermarked. Narrowly lost to doctor --e2e on operator impact. *(merged 5)*
- **Orphan adoption + idempotency keys** — durable run handles let a SIGHUP'd agent's workflow complete and be re-attached/adopted instead of resubmitted. *(merged 3, incl. stream-watchdog quarantine)*
- **Passthrough bench gate** — `gantry bench-passthrough` + CI tripwire making the <5ms budget a tested property. *(merged 2)*
- **macOS tiered cap (Q-5)** — taskpolicy + rlimits + RSS watchdog; doctor reports active cap tier. Objection: zero current demand on an all-Linux fleet. *(merged 4)*
- **SHA attestation echo** — executor echoes the checked-out sha; mismatch = InfraFailure. Gap-round resurrect (verdict-security territory); cheap, likely first pick next run.

### Killed at the kill pass

- **Remote circuit breaker** (merged 2: storm damping / Hystrix transplant) — KILL: introduces gantry's first cross-agent shared mutable health state and a false-positive blast radius (one agent's network blip opens the circuit for all 20) to optimize a cost already bounded by the fallback semaphore + per-backend submit deadlines. A per-process "skip remote after K consecutive InfraFailures" heuristic folds into the semaphore for free.

### Lost the pairwise ranking (or held out by the 2-per-cluster cap)

- Exit-code provenance trailer (merged 2) — capped: verdict-integrity cluster already sends #2 and #3; fold the trailer line into #2's implementation anyway.
- GitHub Actions backend preset — strong zero-code reach; lost to SSH-first as the flagship onboarding story.
- Shadow mode (Scientist transplant) — trust ramp, but doubles cost per run for weeks; demo/oracle covers trust cheaper.
- Heartbeat + queue-position + ETA (merged 3) — good anti-Ctrl-C UX; lost to `gantry why` in-cluster.
- First-failure summary block — daily value, parsing fragility objection; revisit post-v1.
- Kill-switch TTL / dead-man's switch (merged 3) — real fleet footgun fix; S-sized; strong next-run candidate.
- Contract preflight fingerprint — template-drift detection; overlaps doctor --e2e's coverage.
- Backend failover chain — outage resilience at the cost of tripled backend-config surface.
- Pipelined submit (push ∥ prep) — real latency win, premature before v1 exists.
- Bundle relay (offload without a remote) — reach, but breaks the "a commit on a remote" simplicity contract.
- Run-ledger CLI suite (runs/attach/cancel/tail --json, versioned schema; merged 7) — several pieces already planned; adopt piecemeal.
- Exit-code oracle conformance suite — merged into finalist #10.

### Cut at triage

- Welcome first-run banner; uninstall confidence receipt; Homebrew/binstall packaging; doctor --fix; gantry top / status --watch fleet console (merged 3); duration-history smart threshold; env-override one-shots + named profiles (merged 2); `run --at <sha>`; local-cap OOM postmortem line (folded into #2); fallback DEGRADED watermark class (folded into provenance); truncation watermark (folded into Q-6 bench idea); log-stream watchdog (folded into orphan adoption); per-run log sandboxing (mostly planned RunLog behavior); env-parity forwarding allowlist; one-shot resource escalation ladder; concurrency stampede benchmark suite (folded into --drill); config lint did-you-mean (folded into Q-7 finalist); zero-daemon flock JoinTable (mechanism detail of planned JoinTable); kubectl-free REST client (already a planned stretch item).
