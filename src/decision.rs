// gantry — decision engine, verdict ladder, and retry semantics (plan §"Verdict
// semantics", DD-4, INV-7).
//
// The pipeline stages (GitGate → RefPusher → backend submit/wait) each end in a
// [`Verdict`]; this module is where a verdict becomes an *action*. The ladder is
// the load-bearing safety property of the whole tool:
//
//   Pass          → exit 0.
//   TestFailure   → exit non-zero, full stop. NEVER a local rerun (INV-7).
//   GateFailure   → exit non-zero, attributed to the named gate. NEVER a local
//                   rerun (INV-7) — an agent must fix the lint, not reroll dice.
//   InfraFailure  → no verdict was produced, so produce one locally: a capped
//                   local run whose exit code wins (DD-4).
//   Cancelled     → exit 130, no rerun (the user asked for it to stop).
//   Superseded    → a newer sha owns the answer; no rerun.
//
// "Capped" has two independent meanings in the plan and both are honored here as
// far as this phase reaches: the *attempt* cap lives in [`FallbackPolicy`] (a
// remote outage degrades to at most N local runs per invocation, default 1 — an
// infra failure can never become a retry loop), and the *resource* cap (cgroup
// slice, DD-9) plus the fallback admission semaphore land in their own Phase 1b
// beads (bf-xj0, bf-299) and hook into [`run_local`] when they do.
//
// Every rung logs why it was taken: "silently skipped a test run" must be an
// unrepresentable state (DD-4).

use crate::backend::command::CommandBackend;
use crate::backend::{RemoteBackend, RunHandle, RunSpec, Verdict};
use crate::config::Config;
use crate::refs::RefPusher;
use crate::shim;
use std::path::PathBuf;
use std::time::Instant;

/// How many local runs an infra failure may degrade into, per invocation.
///
/// One: an infra failure buys exactly one honest local answer. Zero would make a
/// backend outage block builds; more than one would turn a flaky cluster into a
/// local stampede — the meltdown gantry exists to prevent.
pub const DEFAULT_MAX_LOCAL_FALLBACKS: u32 = 1;

/// The cap on infra-driven local fallbacks (plan §"Local execution & caps").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FallbackPolicy {
    /// Maximum number of local fallback runs a single invocation may take.
    pub max_local_fallbacks: u32,
}

impl FallbackPolicy {
    /// The default policy: one capped local run per invocation.
    pub fn capped() -> Self {
        FallbackPolicy {
            max_local_fallbacks: DEFAULT_MAX_LOCAL_FALLBACKS,
        }
    }

    /// A policy with an explicit cap (0 disables local fallback entirely).
    pub fn with_cap(max_local_fallbacks: u32) -> Self {
        FallbackPolicy {
            max_local_fallbacks,
        }
    }

    /// A policy that never falls back locally — an infra failure is terminal.
    ///
    /// For callers that want the infra classification surfaced without a local run
    /// behind it (a fire drill, a CI box with no toolchain, this module's tests).
    pub fn no_fallback() -> Self {
        FallbackPolicy::with_cap(0)
    }

    /// True iff another local fallback is within budget.
    pub fn allows(&self, fallbacks_used: u32) -> bool {
        fallbacks_used < self.max_local_fallbacks
    }
}

impl Default for FallbackPolicy {
    fn default() -> Self {
        FallbackPolicy::capped()
    }
}

/// What to do with a verdict — the ladder's output.
///
/// This is a pure function of (verdict, fallbacks already used, policy); see
/// [`decide_retry`]. Keeping it a value rather than a control-flow side effect is
/// what makes INV-7 testable: "a TestFailure never yields [`RetryDecision::LocalFallback`]"
/// is an assertion over the whole enum, not a review of every call site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetryDecision {
    /// The verdict is final. Exit with this code; run nothing locally.
    Terminal {
        /// The caller's exit code (INV-3 exit-code fidelity).
        exit_code: i32,
    },
    /// An infra failure with budget left: run locally; the local exit code wins.
    LocalFallback {
        /// Human-readable infra reason, printed before the local run starts.
        reason: String,
        /// 1-based index of this fallback attempt.
        attempt: u32,
    },
    /// An infra failure with the fallback cap already spent: exit non-zero, loudly.
    FallbackExhausted {
        /// Human-readable infra reason.
        reason: String,
        /// The caller's exit code (non-zero — no verdict was ever produced).
        exit_code: i32,
    },
}

/// Apply the verdict ladder (plan §"Verdict semantics", DD-4, INV-7).
///
/// Only [`Verdict::InfraFailure`] can yield a local run, and only while the
/// [`FallbackPolicy`] budget lasts. Every other verdict — including both real
/// failure kinds — is terminal by construction: the match has no arm that could
/// route a `TestFailure` or a `GateFailure` to [`RetryDecision::LocalFallback`],
/// which is INV-7 enforced by the type system rather than by discipline.
pub fn decide_retry(
    verdict: &Verdict,
    fallbacks_used: u32,
    policy: &FallbackPolicy,
) -> RetryDecision {
    match verdict {
        // No verdict was produced remotely, so producing one locally is strictly
        // better (DD-4) — while the cap allows it.
        Verdict::InfraFailure { reason } => {
            if policy.allows(fallbacks_used) {
                RetryDecision::LocalFallback {
                    reason: reason.clone(),
                    attempt: fallbacks_used + 1,
                }
            } else {
                RetryDecision::FallbackExhausted {
                    reason: reason.clone(),
                    exit_code: verdict.clone().to_exit_code(),
                }
            }
        }
        // Pass, TestFailure, GateFailure, Cancelled, Superseded: terminal. A real
        // failure is never re-rolled locally (INV-7) — rerunning would waste the
        // machine gantry protects and risks masking a real failure with an
        // environment-dependent local pass.
        terminal => RetryDecision::Terminal {
            exit_code: terminal.clone().to_exit_code(),
        },
    }
}

/// Run the decision pipeline for an intercepted subcommand.
///
/// This is the core remote execution path (plan §"Architecture"):
/// 1. Check GitGate eligibility — ineligible degrades to a local run with the reason.
/// 2. Push epoch ref via RefPusher — a push failure is an InfraFailure (plan §4).
/// 3. Submit to the backend, then wait for the authoritative verdict.
/// 4. Apply the verdict ladder ([`decide_retry`]) and return the faithful exit code.
///
/// ## Parameters
///
/// - `config`: The hardcoded Config.
/// - `tool`: Tool name (e.g., "cargo").
/// - `subcommand`: Subcommand (e.g., "test").
/// - `args`: The command arguments (e.g., ["--", "--nocapture"]).
/// - `repo_url`: The repository URL (file:// for local testing).
/// - `sha`: The commit SHA to run.
/// - `cwd_rel`: Caller's directory relative to repo root.
///
/// ## Returns
///
/// The exit code that faithfully represents the verdict (INV-3): 0 for Pass, the
/// preserved non-zero code for TestFailure/GateFailure, and — after an
/// InfraFailure — the *local* run's exit code, which wins by definition (DD-4).
pub fn run_remote(
    config: &Config,
    tool: &str,
    subcommand: &str,
    args: &[String],
    repo_url: &str,
    sha: &str,
    cwd_rel: PathBuf,
) -> i32 {
    let policy = config.fallback_policy();

    // Step 1: GitGate. Ineligibility never went remote at all, so there is no
    // verdict to ladder — it degrades straight to the capped local run, with the
    // reason printed (plan §"fallback / degradation": infra failure OR gate
    // ineligibility). Exiting non-zero here would fake a test failure out of a
    // dirty working tree.
    let eligibility = crate::gate::check_git_gate(&config.ci_remote);
    if !eligibility.eligible {
        eprintln!("[gantry] decision: local ({})", eligibility.reason);
        return run_local(config, subcommand, args, &eligibility.reason);
    }

    eprintln!("[gantry] decision: remote execution eligible");

    // Steps 2-3: everything from here to the verdict is the remote attempt, and
    // every way it can go wrong is an InfraFailure — never a test failure (DD-4:
    // "pod-never-started is exit 1, not fallback" is the bug being fixed here).
    let verdict = attempt_remote(config, tool, subcommand, args, repo_url, sha, cwd_rel);

    eprintln!("[gantry] verdict: {}", verdict);

    // Step 4: the ladder.
    match decide_retry(&verdict, 0, &policy) {
        RetryDecision::Terminal { exit_code } => exit_code,
        RetryDecision::LocalFallback { reason, attempt } => {
            eprintln!(
                "[gantry] fallback: local run after infra failure ({reason}) [{attempt}/{}]",
                policy.max_local_fallbacks
            );
            run_local(config, subcommand, args, &reason)
        }
        RetryDecision::FallbackExhausted { reason, exit_code } => {
            eprintln!("[gantry] fallback exhausted: {reason} (local fallback disabled)");
            exit_code
        }
    }
}

/// Run the remote attempt and classify its outcome as a [`Verdict`].
///
/// Push, submit, and wait failures all produce no verdict about the *code*, so
/// each becomes an [`Verdict::InfraFailure`] carrying its reason (plan §4 "Push
/// failure = InfraFailure → local fallback"). Only the backend's own `wait` can
/// return Pass / TestFailure / GateFailure — the verdict is read from the backend's
/// status flow (exit code, or a `verdict.json` when the executor emits one), never
/// from parsing log text (INV-3).
fn attempt_remote(
    config: &Config,
    tool: &str,
    subcommand: &str,
    args: &[String],
    repo_url: &str,
    sha: &str,
    cwd_rel: PathBuf,
) -> Verdict {
    // Step 2: push the epoch ref so the remote has the content to check out.
    let push_result = RefPusher::push(config, sha);
    if !push_result.success {
        return Verdict::infra_failure(format!("push failed: {}", push_result.reason));
    }

    // Step 3: submit.
    let backend = CommandBackend::default_config();
    let spec = RunSpec::new(tool, subcommand, args.to_vec(), repo_url, sha, cwd_rel);

    let handle = match backend.submit(&spec) {
        Ok(h) => h,
        Err(e) => return Verdict::infra_failure(format!("submit failed: {}", e)),
    };

    match &handle {
        RunHandle::Command { handle: h } => eprintln!("[gantry] submitted: {}", h),
        RunHandle::Argo { workflow_name, .. } => eprintln!("[gantry] submitted: {}", workflow_name),
    }

    // Step 3b: wait for the authoritative verdict.
    let deadline = Instant::now() + config.deadline();
    match backend.wait(&handle, deadline) {
        Ok(v) => v,
        Err(e) => Verdict::infra_failure(format!("wait failed: {}", e)),
    }
}

/// Run the command locally and return its exit code faithfully (INV-3).
///
/// The bottom rung of the ladder (DD-4): reached after an InfraFailure, or when
/// GitGate ruled the tree ineligible. Resolves the real binary through
/// [`shim::resolve_real_binary`] — so the self-recursion guard applies here exactly
/// as it does on the passthrough path — and forwards `[subcommand, args...]` as an
/// argv array, never a shell string (S-4, INV-5).
///
/// `reason` is printed before the run so the transcript always says why the local
/// run happened; a resolution or spawn failure is loud and non-zero, never a
/// silent skip (INV-1).
///
/// The cgroup resource cap (DD-9) and the fallback admission semaphore (plan
/// §"Local execution & caps") wrap this call in their own Phase 1b beads; the
/// attempt cap that makes this a *capped* fallback is [`FallbackPolicy`], applied
/// by [`decide_retry`] before we get here.
pub fn run_local(config: &Config, subcommand: &str, args: &[String], reason: &str) -> i32 {
    eprintln!("[gantry] local: running `{subcommand}` locally ({reason})");

    let real = match shim::resolve_real_binary(config) {
        Ok(path) => path,
        Err(why) => {
            eprintln!("[gantry] {why}");
            return 1;
        }
    };

    match std::process::Command::new(&real)
        .arg(subcommand)
        .args(args)
        .status()
    {
        Ok(status) => exit_code_of(status),
        Err(why) => {
            eprintln!("[gantry] failed to run `{}`: {why}", real.display());
            1
        }
    }
}

/// Map a child's exit status to an exit code faithfully (INV-3).
///
/// A normal exit yields its code; on Unix, death by signal `S` yields `128 + S`
/// (the shell's `$?` convention), matching [`crate::shim`]'s passthrough mapping so
/// a fallback run is indistinguishable from an unshimmed one. A platform that
/// withholds a code defaults to 1 rather than a silent success.
fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the ladder: one rung per verdict ------------------------------------

    #[test]
    fn pass_is_terminal_with_exit_zero() {
        assert_eq!(
            decide_retry(&Verdict::Pass, 0, &FallbackPolicy::capped()),
            RetryDecision::Terminal { exit_code: 0 },
        );
    }

    #[test]
    fn test_failure_is_terminal_and_preserves_its_exit_code() {
        // The remote ran the suite and it failed: exit non-zero, full stop.
        assert_eq!(
            decide_retry(&Verdict::test_failure(101), 0, &FallbackPolicy::capped()),
            RetryDecision::Terminal { exit_code: 101 },
        );
    }

    #[test]
    fn gate_failure_is_terminal_and_preserves_its_exit_code() {
        assert_eq!(
            decide_retry(
                &Verdict::gate_failure("clippy".to_string(), 101),
                0,
                &FallbackPolicy::capped(),
            ),
            RetryDecision::Terminal { exit_code: 101 },
        );
    }

    #[test]
    fn cancelled_is_terminal_at_130() {
        // Ctrl-C is the user's decision, not an infra fault: no local rerun.
        assert_eq!(
            decide_retry(&Verdict::Cancelled, 0, &FallbackPolicy::capped()),
            RetryDecision::Terminal { exit_code: 130 },
        );
    }

    #[test]
    fn superseded_is_terminal() {
        // A newer sha owns the answer; rerunning this one locally is pure waste.
        assert_eq!(
            decide_retry(
                &Verdict::Superseded {
                    newer_sha: "deadbeef".to_string()
                },
                0,
                &FallbackPolicy::capped(),
            ),
            RetryDecision::Terminal { exit_code: 0 },
        );
    }

    #[test]
    fn infra_failure_falls_back_locally_carrying_its_reason() {
        // DD-4: no verdict was produced, so produce one locally. The reason must
        // survive into the decision so the transcript can print it.
        let verdict = Verdict::infra_failure("pod was OOMKilled".to_string());
        assert_eq!(
            decide_retry(&verdict, 0, &FallbackPolicy::capped()),
            RetryDecision::LocalFallback {
                reason: "pod was OOMKilled".to_string(),
                attempt: 1,
            },
        );
    }

    // --- INV-7: real failures never retry locally ----------------------------

    #[test]
    fn inv7_test_and_gate_failures_never_fall_back_under_any_budget() {
        // INV-7 is a property over the whole (verdict, budget) space, not a
        // property of one call site: no budget, however generous, and no number of
        // prior fallbacks may turn a real failure into a local rerun.
        let real_failures = [
            Verdict::test_failure(1),
            Verdict::test_failure(101),
            Verdict::gate_failure("clippy".to_string(), 1),
            Verdict::gate_failure("fmt".to_string(), 101),
        ];
        for verdict in &real_failures {
            for used in 0..4u32 {
                for cap in 0..4u32 {
                    let decision = decide_retry(verdict, used, &FallbackPolicy::with_cap(cap));
                    assert!(
                        matches!(decision, RetryDecision::Terminal { exit_code } if exit_code != 0),
                        "INV-7: {verdict} must be terminal and non-zero (used={used}, cap={cap}), got {decision:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn inv7_only_infra_failure_ever_yields_a_local_run() {
        // The dual of the above: enumerate every verdict shape and assert that the
        // only one reaching LocalFallback is InfraFailure.
        let ladder = [
            Verdict::Pass,
            Verdict::test_failure(1),
            Verdict::gate_failure("clippy".to_string(), 1),
            Verdict::infra_failure("submit failed".to_string()),
            Verdict::Cancelled,
            Verdict::Superseded {
                newer_sha: "abc".to_string(),
            },
        ];
        for verdict in &ladder {
            let falls_back = matches!(
                decide_retry(verdict, 0, &FallbackPolicy::capped()),
                RetryDecision::LocalFallback { .. },
            );
            assert_eq!(
                falls_back,
                verdict.should_fallback(),
                "{verdict}: decide_retry and Verdict::should_fallback must agree",
            );
            assert_eq!(
                falls_back,
                matches!(verdict, Verdict::InfraFailure { .. }),
                "{verdict}: only InfraFailure may reach a local run (INV-7)",
            );
        }
    }

    // --- the cap -------------------------------------------------------------

    #[test]
    fn infra_fallback_is_capped_not_a_retry_loop() {
        // The default budget is exactly one local run: the second infra failure in
        // the same invocation is exhausted, not another local stampede.
        let verdict = Verdict::infra_failure("cluster unreachable".to_string());
        let policy = FallbackPolicy::capped();
        assert!(matches!(
            decide_retry(&verdict, 0, &policy),
            RetryDecision::LocalFallback { attempt: 1, .. },
        ));
        assert!(matches!(
            decide_retry(&verdict, 1, &policy),
            RetryDecision::FallbackExhausted { .. },
        ));
    }

    #[test]
    fn exhausted_fallback_exits_non_zero_with_the_reason() {
        // A backend outage with fallback disabled must still be loud and red — an
        // absent verdict may never surface as success.
        let verdict = Verdict::infra_failure("cluster unreachable".to_string());
        match decide_retry(&verdict, 0, &FallbackPolicy::no_fallback()) {
            RetryDecision::FallbackExhausted { reason, exit_code } => {
                assert_eq!(reason, "cluster unreachable");
                assert_ne!(exit_code, 0, "an infra failure may never exit 0");
            }
            other => panic!("expected FallbackExhausted, got {other:?}"),
        }
    }

    #[test]
    fn a_larger_cap_permits_later_attempts_and_numbers_them() {
        let verdict = Verdict::infra_failure("stream lost".to_string());
        let policy = FallbackPolicy::with_cap(3);
        for used in 0..3u32 {
            assert_eq!(
                decide_retry(&verdict, used, &policy),
                RetryDecision::LocalFallback {
                    reason: "stream lost".to_string(),
                    attempt: used + 1,
                },
            );
        }
        assert!(matches!(
            decide_retry(&verdict, 3, &policy),
            RetryDecision::FallbackExhausted { .. },
        ));
    }

    #[test]
    fn default_policy_is_the_documented_cap() {
        assert_eq!(FallbackPolicy::default(), FallbackPolicy::capped());
        assert_eq!(
            FallbackPolicy::capped().max_local_fallbacks,
            DEFAULT_MAX_LOCAL_FALLBACKS,
        );
        assert!(FallbackPolicy::capped().allows(0));
        assert!(!FallbackPolicy::capped().allows(1));
        assert!(!FallbackPolicy::no_fallback().allows(0));
    }

    #[test]
    fn config_supplies_the_capped_policy() {
        // The pipeline reads its budget from Config, not from a literal at the
        // call site, so a future config layer can change it in one place.
        assert_eq!(
            Config::hardcoded().fallback_policy(),
            FallbackPolicy::capped(),
        );
    }

    // --- backend status → verdict → decision, end to end ---------------------

    #[test]
    fn backend_exit_codes_ladder_through_to_decisions() {
        // The full path the bead names: a backend status (here, the command
        // backend's exit code contract) becomes a Verdict, and the Verdict becomes
        // a retry decision. 0 passes, 1 is a terminal test failure, and anything
        // above is infra — which is the only rung that runs anything locally.
        let policy = FallbackPolicy::capped();

        assert_eq!(
            decide_retry(&Verdict::from_exit_code(0), 0, &policy),
            RetryDecision::Terminal { exit_code: 0 },
        );
        assert_eq!(
            decide_retry(&Verdict::from_exit_code(1), 0, &policy),
            RetryDecision::Terminal { exit_code: 1 },
        );
        assert!(
            matches!(
                decide_retry(&Verdict::from_exit_code(2), 0, &policy),
                RetryDecision::LocalFallback { .. },
            ),
            "a backend that could not schedule the run is infra, not a test failure",
        );
    }

    #[test]
    fn oom_and_deadline_verdict_json_ladder_to_a_local_run() {
        // The classification the plan calls out by name: an OOMKilled pod and a
        // deadline-exceeded workflow are infrastructure caps firing, which say
        // nothing about the code — they must fall back, never report "tests failed".
        for json in [
            r#"{"schema_version":1,"phase":"Failed","exit_code":137,"oom":true}"#,
            r#"{"schema_version":1,"phase":"Failed","exit_code":1,"deadline_exceeded":true}"#,
        ] {
            let verdict = crate::backend::VerdictJson::parse(json)
                .expect("valid verdict.json")
                .expect("schema version 1 is supported")
                .to_verdict();
            assert!(
                matches!(
                    decide_retry(&verdict, 0, &FallbackPolicy::capped()),
                    RetryDecision::LocalFallback { .. },
                ),
                "{json} must classify as InfraFailure → local fallback, got {verdict}",
            );
        }
    }

    #[test]
    fn a_failed_run_with_a_failure_class_stays_terminal() {
        // The other side of the same contract: a real test failure carried by
        // verdict.json must NOT reach a local run (INV-7).
        let verdict = crate::backend::VerdictJson::parse(
            r#"{"schema_version":1,"phase":"Failed","exit_code":101,"failure_class":"compile_error"}"#,
        )
        .expect("valid verdict.json")
        .expect("schema version 1 is supported")
        .to_verdict();
        assert_eq!(
            decide_retry(&verdict, 0, &FallbackPolicy::capped()),
            RetryDecision::Terminal { exit_code: 101 },
        );
    }

    #[test]
    fn a_gate_failure_from_verdict_json_stays_terminal_and_names_the_gate() {
        // A failing enabled gate is red and attributed, and — like TestFailure —
        // never triggers a local retry (plan §"Verdict semantics", INV-7).
        let verdict = crate::backend::VerdictJson::parse(
            r#"{"schema_version":1,"phase":"Failed","exit_code":101,"gate_failed":"clippy"}"#,
        )
        .expect("valid verdict.json")
        .expect("schema version 1 is supported")
        .to_verdict();
        assert!(
            matches!(&verdict, Verdict::GateFailure { gate_name, .. } if gate_name == "clippy"),
            "gate attribution must survive into the verdict, got {verdict}",
        );
        assert_eq!(
            decide_retry(&verdict, 0, &FallbackPolicy::capped()),
            RetryDecision::Terminal { exit_code: 101 },
        );
    }

    // --- the local rung ------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn run_local_returns_the_child_exit_code_verbatim() {
        // The fallback's exit code wins (DD-4), so it must be faithful (INV-3).
        let dir = std::env::temp_dir().join(format!("gantry-decision-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let fixture = dir.join("cargo");
        std::fs::write(&fixture, b"#!/bin/sh\nexit 42\n").expect("write fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fixture");
        }

        let mut cfg = Config::hardcoded();
        cfg.real_binary = Some(fixture);
        assert_eq!(run_local(&cfg, "test", &[], "unit test"), 42);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_local_is_loud_and_non_zero_when_the_binary_cannot_be_resolved() {
        // A local rung that cannot run must still be a visible failure, never a
        // silent skip (INV-1) and never a success.
        let mut cfg = Config::hardcoded();
        cfg.real_binary = Some(PathBuf::from("/nonexistent/gantry-missing-cargo"));
        assert_ne!(run_local(&cfg, "test", &[], "unit test"), 0);
    }
}
