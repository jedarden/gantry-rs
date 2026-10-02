// gantry — drill-scoped fault injection for `gantry doctor --drill`
// (plan Component 8, bf-2qq).
//
// The fallback ladder is gantry's core safety promise, and it otherwise only
// executes when things are already on fire (plan §"doctor --drill"): a fire
// drill must be able to light that fire on demand. This module is the
// injection point — a synthetic InfraFailure raised through the real decision
// pipeline's wait boundary, so the drill exercises the genuine degrade chain
// (banner, intent/verdict records, semaphore-gated capped run, faithful exit
// code) rather than a rehearsal script that mimics it.
//
// ## The never-outside-a-drill contract
//
// An injection hook in the remote pipeline is one miswired flag away from
// failing every real run, so the contract is deliberately narrow:
//
// - The armed state is a **private atomic** in this module. No environment
//   variable, config key, or CLI flag can set it — an operator or fleet agent
//   with a stray `GANTRY_DRILL=1` exported must never degrade a real run.
// - The only producer of an armed scope is the drill pipeline runner itself
//   (the hidden `gantry __drill-run` subcommand [`DRILL_RUN_SUBCOMMAND`] that
//   `doctor --drill` spawns) and the unit tests below, both of which are
//   drills by definition.
// - The only consumer is [`inject`], called from the explicit-offload
//   pipeline's backend construction ([`crate::decision::run_explicit`] — the
//   pipeline whose infra tails are the full admission-semaphore ladder the
//   drill exists to prove; the intercepted pipeline's wait tail stops at a
//   loud non-zero and has no ladder to rehearse). Unarmed it is a single
//   atomic load that returns the real backend untouched.
// - [`ArmScope`] disarms on drop, so a drill process cannot leak the armed
//   state past its own lifetime — the scope's lifetime *is* the drill's.
//
// The synthetic failure is raised as a **deadline expiry**
// ([`crate::backend::BackendError::deadline`]) — the one wait-failure shape
// the pipeline routes through the capped-local fallback ladder (plan DD-4 +
// Component 6) — so the drill drives the full chain the plan names, not a
// bare infra exit.

use crate::backend::{
    BackendError, BackendResult, RemoteBackend, RunHandle, RunSpec, RunStatus, Verdict,
};
use std::sync::atomic::{AtomicBool, Ordering};

/// The synthetic InfraFailure's reason string.
///
/// Rides the `[gantry] timeout:` / `[gantry] infra:` banner lines, the
/// flight-recorder bundle, and the ledger's timeout detail, so a drill's
/// degrade is always identifiable as a drill — never mistakable in a
/// transcript or a post-mortem for a real outage.
pub const DRILL_INFRA_REASON: &str = "drill: synthetic infra failure (gantry doctor --drill)";

/// The handle the drill backend hands out from `submit()`.
///
/// Fixed and drill-named for the same reason the reason string is: it shows
/// up in the `[gantry] submitted:` line, the `[gantry] timeout:` line, and
/// the ledger, and every one of those must read as a drill.
pub const DRILL_HANDLE: &str = "gantry-drill-synthetic";

/// The hidden management subcommand `gantry doctor --drill` spawns to run the
/// armed pipeline; the doctor side asserts the degrade chain from the child's
/// stderr and its ledger delta. Deliberately absent from `gantry help`: the
/// doctor flag is the only supported way in, and the subcommand exists so the
/// drill's exit code is a real process exit the parent can assert against the
/// verdict record.
pub const DRILL_RUN_SUBCOMMAND: &str = "__drill-run";

/// Whether the drill hook is armed in this process.
static ARMED: AtomicBool = AtomicBool::new(false);

/// RAII scope that arms the drill hook for its lifetime.
///
/// Produced only by [`arm`]. Dropping the scope disarms — including on
/// early return or panic — so arming can never outlive the drill that
/// requested it. Not `Clone`: the armed state is process-global and
/// single-scope by contract (the drill is a single-threaded pipeline run),
/// so a clonable scope would let one drop re-arm what another disarmed.
pub struct ArmScope;

impl Drop for ArmScope {
    fn drop(&mut self) {
        ARMED.store(false, Ordering::SeqCst);
    }
}

/// Arm the drill hook until the returned [`ArmScope`] drops.
///
/// Call only from drill code paths: the hidden `__drill-run` pipeline runner
/// and tests. Nothing else — no environment variable, no config key, no flag
/// — reaches this function, which is what makes the hook "never honored
/// outside a drill" (plan §"doctor --drill").
pub fn arm() -> ArmScope {
    ARMED.store(true, Ordering::SeqCst);
    ArmScope
}

/// Whether the drill hook is currently armed.
///
/// The read side of the contract: the pipeline consults this only through
/// [`inject`], so an unarmed process pays one atomic load per backend
/// construction and nothing else.
pub fn armed() -> bool {
    ARMED.load(Ordering::SeqCst)
}

/// The pipeline's single injection seam: hand over the backend the config
/// built, get back the one the run should use.
///
/// Unarmed (every real run, ever) this returns `backend` unchanged — the
/// atomic load is the hook's entire cost. Armed (a drill) it returns the
/// [`DrillBackend`], whose `wait` raises the synthetic deadline expiry that
/// routes the run through the real fallback ladder.
pub fn inject(backend: Box<dyn RemoteBackend>) -> Box<dyn RemoteBackend> {
    if armed() {
        Box::new(DrillBackend)
    } else {
        backend
    }
}

/// The drill's synthetic backend: accepts any submission, never completes.
///
/// Every method is hermetic by design — a fire drill must not touch the
/// operator's real cluster, CI, or git hosting, because the whole premise of
/// a drill is that infra may already be unavailable. `submit` answers with
/// the fixed drill handle without contacting anything; `wait` raises the
/// synthetic deadline expiry ([`DRILL_INFRA_REASON`]) immediately, which the
/// decision pipeline classifies as an `InfraFailure` (DD-4 — a drill
/// fabricates no verdict either) and routes through the fallback ladder.
struct DrillBackend;

impl RemoteBackend for DrillBackend {
    fn submit(&self, _spec: &RunSpec) -> BackendResult<RunHandle> {
        // No real submission: the drill's remote "run" is the failure the
        // hook is about to raise, and launching nothing is the point.
        Ok(RunHandle::new(DRILL_HANDLE))
    }

    fn wait(&self, _h: &RunHandle, _deadline: std::time::Instant) -> BackendResult<Verdict> {
        // The synthetic InfraFailure, in the deadline shape: this is the
        // error the pipeline's wait tail degrades on (DD-4, plan Component
        // 6), so the drill exercises banner, flight recorder, ledger,
        // semaphore, capped run, and exit code — the whole chain.
        Err(BackendError::deadline(DRILL_INFRA_REASON))
    }

    fn stream_logs(&self, _h: &RunHandle, _out: &mut dyn std::io::Write) -> BackendResult<()> {
        // Nothing was ever launched; there are no logs to stream. The real
        // pipeline treats this call as best-effort.
        Ok(())
    }

    fn describe(&self, _h: &RunHandle) -> String {
        format!("drill/{}", DRILL_HANDLE)
    }

    fn cancel(&self, _h: &RunHandle) -> BackendResult<()> {
        // Nothing to cancel — the synthetic run exists only as its failure.
        Ok(())
    }

    fn status(&self, _h: &RunHandle) -> BackendResult<RunStatus> {
        // "Unknown, not error": the pipeline's polling contract treats an
        // unanswerable status as no news rather than a failure.
        Ok(RunStatus::Unknown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real backend with behavior distinctive enough to prove [`inject`]
    /// returned it untouched when unarmed.
    struct MarkerBackend;

    impl RemoteBackend for MarkerBackend {
        fn submit(&self, _spec: &RunSpec) -> BackendResult<RunHandle> {
            Ok(RunHandle::new("marker-real-handle"))
        }

        fn wait(&self, _h: &RunHandle, _deadline: std::time::Instant) -> BackendResult<Verdict> {
            Ok(Verdict::Pass)
        }
    }

    /// Tests run in one process with a global flag, so arming is serialized
    /// behind a mutex — the same guard the suite's other global-state tests
    /// use. Each test takes the lock for its whole body.
    fn arm_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn hook_starts_disarmed() {
        let _lock = arm_lock();
        assert!(!armed(), "the drill hook must start disarmed");
    }

    #[test]
    fn arm_scope_arms_and_dropping_disarms() {
        let _lock = arm_lock();
        {
            let _scope = arm();
            assert!(armed(), "arm() must arm the hook");
        }
        assert!(!armed(), "dropping the scope must disarm the hook");
    }

    #[test]
    fn arm_scope_disarms_on_early_return() {
        let _lock = arm_lock();
        fn drill_body() -> bool {
            let _scope = arm();
            armed()
        }
        assert!(drill_body(), "the body runs armed");
        assert!(!armed(), "the scope's drop on return must disarm");
    }

    #[test]
    fn unarmed_inject_returns_the_real_backend() {
        let _lock = arm_lock();
        assert!(!armed());
        let backend = inject(Box::new(MarkerBackend));
        // Behavior, not type: the marker's contract survives untouched.
        let handle = backend
            .submit(&RunSpec::new("cargo", "test", vec![], "file:///r", "0", ""))
            .expect("unarmed backend submits for real");
        assert_eq!(handle.handle, "marker-real-handle");
        let verdict = backend
            .wait(
                &handle,
                std::time::Instant::now() + std::time::Duration::from_secs(1),
            )
            .expect("unarmed backend waits for real");
        assert_eq!(verdict, Verdict::Pass);
    }

    #[test]
    fn armed_inject_substitutes_the_drill_backend() {
        let _lock = arm_lock();
        let _scope = arm();
        let backend = inject(Box::new(MarkerBackend));
        // submit accepts anything and answers with the drill handle — no
        // real submission happens, and the marker's behavior is gone.
        let handle = backend
            .submit(&RunSpec::new("cargo", "test", vec![], "file:///r", "0", ""))
            .expect("the drill backend accepts the submission");
        assert_eq!(handle.handle, DRILL_HANDLE);

        // wait raises the synthetic failure in the deadline shape — the one
        // the pipeline's tail degrades through the ladder on.
        let err = backend
            .wait(
                &handle,
                std::time::Instant::now() + std::time::Duration::from_secs(1),
            )
            .expect_err("the drill backend must fail its wait");
        assert!(
            err.deadline_exceeded,
            "the synthetic failure is a deadline expiry"
        );
        assert_eq!(err.reason, DRILL_INFRA_REASON);
        assert!(err.run_url.is_none());

        // And the scope's contract holds through the assertions above.
        drop(_scope);
        assert!(!armed());
    }

    #[test]
    fn drill_backend_is_hermetic_on_every_method() {
        let _lock = arm_lock();
        let _scope = arm();
        let backend = inject(Box::new(MarkerBackend));
        let handle = backend
            .submit(&RunSpec::new("cargo", "test", vec![], "file:///r", "0", ""))
            .unwrap();

        // Log streaming: nothing was launched, so it is an empty success.
        let mut sink: Vec<u8> = Vec::new();
        backend
            .stream_logs(&handle, &mut sink)
            .expect("drill log streaming is an empty success");
        assert!(sink.is_empty());

        // describe names the drill, cancel succeeds (nothing to cancel),
        // and status answers Unknown rather than erroring — the pipeline's
        // polling contract for "no news".
        assert_eq!(backend.describe(&handle), format!("drill/{}", DRILL_HANDLE));
        backend.cancel(&handle).expect("nothing to cancel");
        assert!(matches!(backend.status(&handle), Ok(RunStatus::Unknown)));
    }
}
