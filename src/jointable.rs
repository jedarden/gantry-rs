// gantry — JoinTable: concurrent-run dedup (plan Component 9, v1.x).
//
// Two callers dispatching the *same* run at the same time — identical
// remote, sha, tool, and args — are the everyday shape of an agent fleet
// verifying one commit: twenty workers sharing one repo each invoke
// `cargo test`, and without dedup the cluster runs twenty copies of one
// suite. The JoinTable is the in-flight map between them: a state-dir file
// per key records which RunHandle owns the run, and a second identical
// invocation attaches to that handle (waits on it) instead of resubmitting —
// one workflow, both callers served the same verdict.
//
// Mechanics:
//
// - **Key** — [`JoinKey::new`] hashes the run-identity tuple
//   `(repo_url, sha, tool, subcommand, args)` into a fixed-length filename
//   (FNV-1a 64). The plan words the key `(remote_url, sha, tool, args)`; the
//   subcommand rides alongside the arg tail because an intercepted
//   `cargo test` and `gantry run -- cargo test` produce the identical
//   RunSpec (plan Q-2) and must land on one key. The repo identity is the
//   *remote URL* — worktree-invariant by construction, since both linked
//   worktrees of a repo resolve the same CI remote — which is EC-01's "two
//   worktrees of one repo dedup correctly" in its URL spelling. FNV-1a is
//   specified byte-for-byte, so the filename is stable across processes and
//   compiler versions without a hashing dependency; it is a filename
//   discriminator, not a security boundary — the inputs are the caller's own
//   argv, and a collision could only make two runs share a verdict. The
//   identity feeding the hash is worktree-invariant (plan EC-01):
//   [`resolve_repo_identity`] reads the remote URL — or the *common* git dir
//   when there is no remote — through the repo's shared config, never the
//   checkout's own path, so two worktrees of one repo compute one key.
//
// - **Claim** — [`claim`] decides originator-vs-joiner under an exclusive
//   `flock` on `<state>/join/<key>.lock`, so read-entry-or-write-claim is
//   atomic across processes (the same kernel-lock posture as the Component 6
//   semaphore: a SIGKILLed holder releases the lock with no gantry code
//   running).
//
// - **Entry** — `<state>/join/<key>.run` holds the claimed run's JSON:
//   handle (empty between claim and submit), run id, owner pid, backend,
//   start time. The originator records the handle once submit succeeds and
//   the entry dies with its guard ([`JoinEntry::drop`]) on *every* exit
//   path — a failed dispatch can never wedge the key shut. Entries are
//   written by temp-file + rename, so a concurrent reader sees the old or
//   the new document, never a torn one.
//
// - **Liveness** — an entry carrying a handle is always attachable: the
//   remote run is real regardless of whether its originator still lives,
//   and the key includes the sha, so its verdict is exactly the verdict an
//   attached caller would have produced with its own submission. An entry
//   *without* a handle means its owner sits between claim and submit, so
//   the pid decides: a dead owner's claim is taken over, a live owner's
//   claim is polled for the handle. Poll-budget exhaustion (a wedged owner)
//   degrades to [`JoinDecision::Unjoined`] — dedup is an optimization, never
//   a dependency: an unjoinable invocation submits its own run, exactly the
//   pre-JoinTable behavior.
//
// - **Cleanup** — the joiner removes the entry once its wait lands a
//   terminal verdict ([`AttachHandle::release`]), so a leaked entry (an
//   originator killed in the instant between verdict and guard drop) is
//   reclaimed by the first identical invocation after it rather than held
//   forever.
//
// - **Attachments** — every live watcher of a run is countable (plan
//   Component 10's supersede precondition: "zero live attachments —
//   originator gone, no JoinTable waiters"). The originator's attachment is
//   its entry, live while its pid is; each joiner registers a waiter file
//   (`<state>/join/<key>.att/<run_id>`, JSON `{pid, started_epoch_ms}`) at
//   claim time — under the same flock as the decision, so a concurrent count
//   never straddles a registration — and detaches by removing it
//   ([`AttachHandle::drop`]). [`attachment_count`] sums entry-plus-live-
//   waiters, pruning waiter files whose pid has died: a joiner killed
//   outright (no Drop runs) stops counting at the next count instead of
//   pinning the key against supersede forever. The joiner's stream+wait
//   tail itself lives in the dispatch pipelines ([`crate::decision`]): a
//   joiner streams the live run's output through the shared handle
//   (best-effort), then meets the originator in the shared wait — the same
//   tail, so the joiner's wait lands the exact handling an originator gets
//   — whose terminal arm reclaims the entry ([`AttachHandle::release`]).

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::backend::RunHandle;

/// Schema version of the on-disk entry document. A foreign schema is treated
/// as unreadable (never stolen): an entry this binary cannot understand may
/// describe a genuinely in-flight run from a different gantry version.
const ENTRY_SCHEMA: u32 = 1;

/// How long a joiner polls a live owner's claim for the handle to appear
/// before submitting its own run alongside. The normal window is
/// milliseconds (claim → push → submit); the budget only has to outlast a
/// slow remote submit, not a whole run — the handle appears when submit
/// returns.
const CLAIM_POLL_BUDGET: Duration = Duration::from_secs(60);

/// Pause between polls of a live owner's claim.
const CLAIM_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The on-disk entry document: one per claimed key, owned by the run that
/// won the claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EntryDoc {
    /// [`ENTRY_SCHEMA`] — foreign values are never understood away.
    schema: u32,
    /// The submitted run's backend handle; empty while the owner is between
    /// claim and submit.
    handle: String,
    /// The owner's runlog run id (the OPEN intent the owner wrote), so a
    /// joiner can name what it attached to.
    run_id: String,
    /// Owner pid, for the dead-owner takeover of a handle-less claim.
    pid: u32,
    /// Backend the handle belongs to. A joiner configured for a different
    /// backend cannot wait on a foreign handle — a mismatched entry is
    /// declined, not attached to.
    backend: String,
    /// Wall-clock claim time (diagnostics; `gantry why` material).
    started_epoch_ms: u64,
}

/// The dedup key: `(repo_url, sha, tool, subcommand, args)` hashed into a
/// filename-safe form (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinKey {
    name: String,
}

/// Fold one field into the FNV-1a 64 hash, then a field separator, so
/// `("ab", "c")` and `("a", "bc")` hash apart.
fn fnv1a_absorb(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash ^= u64::from(b);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    *hash ^= u64::from(0x1f_u8);
    *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
}

impl JoinKey {
    /// Hash the run-identity tuple into a dedup key (module docs).
    pub fn new(repo_url: &str, sha: &str, tool: &str, subcommand: &str, args: &[String]) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        fnv1a_absorb(&mut hash, repo_url.as_bytes());
        fnv1a_absorb(&mut hash, sha.as_bytes());
        fnv1a_absorb(&mut hash, tool.as_bytes());
        fnv1a_absorb(&mut hash, subcommand.as_bytes());
        for arg in args {
            fnv1a_absorb(&mut hash, arg.as_bytes());
        }
        JoinKey {
            name: format!("{hash:016x}"),
        }
    }
}

/// The worktree-invariant half of a run identity, resolved from a repo
/// checkout (plan EC-01): the CI remote URL — falling back to the repo's
/// canonical common git dir when no origin is configured — plus the
/// requested revision's full sha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoIdentity {
    /// The remote URL, or the canonical common git dir when the repo has no
    /// origin. Worktree-invariant by construction: both spellings resolve
    /// from state every worktree of the repo shares (remote config, common
    /// dir), never from the checkout's own path.
    pub repo: String,
    /// The revision's full sha as `git rev-parse --verify` reports it.
    pub sha: String,
}

/// Resolve a repo checkout's run identity (plan EC-01): the remote URL from
/// the shared config and the revision's full sha, so two linked worktrees of
/// one repo resolve the *same* identity for the same revision and land on one
/// dedup key.
///
/// The revision resolves through the common object store — `git rev-parse`
/// runs in `repo_root`, whose gitfile chains to the common git dir — so
/// shas, branch names, and tags resolve identically from every worktree.
/// Per-worktree pseudo-refs resolve to the calling worktree's commit, which
/// is the correct key: two worktrees on *different* commits must not share a
/// verdict, and EC-01 keeps the GitGate's worktree-HEAD sha as the key's sha.
pub fn resolve_repo_identity(repo_root: &Path, rev: &str) -> Result<RepoIdentity, JoinError> {
    let sha = git_capture(repo_root, &["rev-parse", "--verify", rev])?;
    let repo = match git_capture(repo_root, &["remote", "get-url", "origin"]) {
        Ok(url) => url,
        Err(_) => {
            // No origin (a local-only repo): the common git dir is the
            // next-best worktree-invariant identity — EC-01's own spelling
            // (`git rev-parse --git-common-dir`). Canonicalized so both
            // worktrees hash the same string, not two spellings of one path.
            let common =
                crate::gate::git_common_dir_in(repo_root).map_err(|reason| JoinError::Git {
                    repo: repo_root.to_path_buf(),
                    reason,
                })?;
            std::fs::canonicalize(&common)
                .map_err(|source| JoinError::Io {
                    path: PathBuf::from(&common),
                    source,
                })?
                .to_string_lossy()
                .into_owned()
        }
    };
    Ok(RepoIdentity { repo, sha })
}

/// [`JoinKey`] for a repo checkout + revision + command shape: resolve the
/// identity through the common git dir ([`resolve_repo_identity`], plan
/// EC-01) and hash the same tuple [`JoinKey::new`] hashes, so a key computed
/// from worktree A equals the key computed from worktree B of one repo.
pub fn key_for_repo(
    repo_root: &Path,
    rev: &str,
    tool: &str,
    subcommand: &str,
    args: &[String],
) -> Result<JoinKey, JoinError> {
    let identity = resolve_repo_identity(repo_root, rev)?;
    Ok(JoinKey::new(
        &identity.repo,
        &identity.sha,
        tool,
        subcommand,
        args,
    ))
}

/// One git command in `dir`, stdout trimmed; spawn failure and non-zero exit
/// fold into [`JoinError::Git`] (the same shells-out-to-system-git posture as
/// the GitGate — no libgit2, the user's git config and credential helpers
/// honored).
fn git_capture(dir: &Path, args: &[&str]) -> Result<String, JoinError> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| JoinError::Git {
            repo: dir.to_path_buf(),
            reason: format!("git {} failed: {e}", args.join(" ")),
        })?;
    if !output.status.success() {
        return Err(JoinError::Git {
            repo: dir.to_path_buf(),
            reason: format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// What [`claim`] decided for this invocation.
#[derive(Debug)]
pub enum JoinDecision {
    /// This invocation owns the run: dispatch normally, record the handle
    /// on the entry after submit ([`JoinEntry::record_handle`]), and let the
    /// guard clear the key on every exit path.
    Originator(JoinEntry),
    /// An identical run is already in flight: wait on its handle instead of
    /// submitting.
    Attach(AttachHandle),
    /// Dedup declined this time — no state dir, an unreadable entry, a
    /// wedged or foreign-backend claim. Submit normally; touch nothing.
    Unjoined,
}

/// The originator's claim guard. Dropping it removes the entry, closing the
/// key for the next identical invocation — on the success path, the push /
/// submit failure paths, and every fallback ladder alike.
#[derive(Debug)]
pub struct JoinEntry {
    entry_path: PathBuf,
    /// The claim this guard owns. The entry file can outlive the claim it
    /// was written for — a joiner's terminal-verdict release and a fresh
    /// invocation's re-claim hand the key to a new epoch while an old guard
    /// is still in scope — so both guard operations are scoped to it: they
    /// touch an entry only while it still belongs to this run.
    run_id: String,
}

impl JoinEntry {
    /// Record the submitted run's handle on the claim, opening the attach
    /// window: from here until the guard drops, identical invocations join
    /// this run.
    ///
    /// Best-effort by contract: a failed recording leaves a handle-less
    /// entry that joiners poll out on (dead-pid takeover once the owner
    /// exits, poll budget while it lives) — dedup degrades, dispatch never
    /// fails for the ledger's sake.
    pub fn record_handle(&self, handle: &RunHandle) {
        // Recording changes the same document that reclaim and guard-drop
        // remove. Keep the ownership check and replacement under the claim
        // lock, otherwise a fresh claim could be installed between the read
        // and write and receive a stale originator's handle.
        let lock_path = self.entry_path.with_extension("lock");
        let lock = match open_lock_file(&lock_path) {
            Ok(lock) => lock,
            Err(_) => return,
        };
        lock_exclusive_blocking(&lock);

        let updated = match read_entry(&self.entry_path) {
            Ok(Some(doc)) if doc.run_id == self.run_id => EntryDoc {
                handle: handle.handle.clone(),
                ..doc
            },
            // The entry vanished under a guard that still holds it — or the
            // key moved to a new claim epoch and the entry is someone else's
            // claim now. Nothing of ours to update; the guard's drop is
            // equally scoped and will not touch their claim.
            _ => return,
        };
        let _ = write_entry(&self.entry_path, &updated);
    }
}

impl Drop for JoinEntry {
    fn drop(&mut self) {
        // Best-effort, and only while the entry is still ours: a failed
        // removal leaves an attachable entry whose first terminal-verdict
        // joiner reclaims it (module docs, Cleanup); an entry re-claimed by
        // a fresh invocation after a joiner's release is that claimant's,
        // and this drop must not close their key.
        let _ = remove_entry_if_owner(&self.entry_path, &self.run_id);
    }
}

/// The joiner's side of an attach: the handle to wait on, plus the claim it
/// attached through.
#[derive(Debug)]
pub struct AttachHandle {
    /// The in-flight run's backend handle.
    pub handle: RunHandle,
    /// The originator's runlog run id, for the join line's provenance.
    pub originator_run_id: String,
    entry_path: PathBuf,
    /// This invocation's waiter registration (module docs, Attachments).
    /// Removed on drop — the detach that stops counting this joiner on every
    /// exit path, terminal or not.
    waiter_path: PathBuf,
}

impl AttachHandle {
    /// Clear the in-flight entry once the joined run reached a terminal
    /// verdict — the joiner-side reclaim that closes a key whose originator
    /// died before its guard could (module docs, Cleanup). Best-effort: the
    /// originator may have removed it first.
    ///
    /// Scoped like the originator's guard: the entry is removed only while
    /// it still belongs to the run this handle attached through. A release
    /// that lands after the key moved to a fresh claim epoch (a sibling
    /// joiner's release let a new invocation originate mid-wait) must leave
    /// that new claim alone.
    pub fn release(&self) {
        let _ = remove_entry_if_owner(&self.entry_path, &self.originator_run_id);
    }
}

impl Drop for AttachHandle {
    fn drop(&mut self) {
        // The detach: this invocation stops watching, so it stops counting —
        // terminal (release already reclaimed the entry) or not (an error or
        // Ctrl-C exit must not pin the key against supersede). Best-effort;
        // a skipped removal is corrected by the next count's dead-pid prune.
        let _ = fs::remove_file(&self.waiter_path);
    }
}

/// The on-disk waiter registration: one per attached joiner, named by the
/// joiner's own run id under `<state>/join/<key>.att/` (module docs,
/// Attachments).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WaiterDoc {
    /// Joiner pid — the liveness check that makes "live attachments" literal
    /// and lets a count prune registrations whose process died outright.
    pid: u32,
    /// Wall-clock attach time (diagnostics; `gantry why` material).
    started_epoch_ms: u64,
}

/// Decide [`JoinDecision::Attach`] for a readable, handle-bearing entry:
/// register the caller as a live waiter and hand back the attach handle.
/// Callers run it while still holding the key's claim lock, so a concurrent
/// [`attachment_count`] never straddles the registration.
fn attach_decision(
    entry_path: &Path,
    run_id: &str,
    doc: &EntryDoc,
) -> Result<JoinDecision, JoinError> {
    let waiter_path = register_waiter(entry_path, run_id)?;
    Ok(JoinDecision::Attach(AttachHandle {
        handle: RunHandle::new(&doc.handle),
        originator_run_id: doc.run_id.clone(),
        entry_path: entry_path.to_path_buf(),
        waiter_path,
    }))
}

/// Write this invocation's waiter registration: `<key>.att/<run_id>`, JSON
/// [`WaiterDoc`]. The name is the joiner's own run id (unique per
/// invocation), so the write needs no synchronization beyond the claim lock
/// its caller holds.
fn register_waiter(entry_path: &Path, run_id: &str) -> Result<PathBuf, JoinError> {
    let att_dir = entry_path.with_extension("att");
    fs::create_dir_all(&att_dir).map_err(|source| JoinError::Io {
        path: att_dir.clone(),
        source,
    })?;
    let waiter_path = att_dir.join(run_id);
    let json = serde_json::to_vec(&WaiterDoc {
        pid: std::process::id(),
        started_epoch_ms: now_epoch_ms(),
    })
    .map_err(|source| JoinError::Io {
        path: waiter_path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    })?;
    fs::write(&waiter_path, json).map_err(|source| JoinError::Io {
        path: waiter_path.clone(),
        source,
    })?;
    Ok(waiter_path)
}

/// Count the live attachments on a key (module docs, Attachments): one for
/// the originator while its entry stands and its pid lives, plus one per
/// waiter registration whose pid is still alive. Waiter files whose pid has
/// died are pruned on the way past — a joiner killed outright stops counting
/// here, and the directory self-heals. No entry means nothing in flight:
/// zero.
///
/// The count takes the key's claim lock, so it is a consistent snapshot
/// against claim decisions (a joiner mid-registration is never straddled).
/// Errors are reported, not folded into zero: a caller deciding whether a
/// run is watched must treat "could not count" as watched (nonzero), never
/// as the zero that would license a supersede.
pub fn attachment_count(state_dir: &Path, key: &JoinKey) -> Result<u32, JoinError> {
    let join_dir = state_dir.join("join");
    let entry_path = join_dir.join(format!("{}.run", key.name));
    let lock_path = join_dir.join(format!("{}.lock", key.name));

    let lock = open_lock_file(&lock_path).map_err(|source| JoinError::Io {
        path: lock_path,
        source,
    })?;
    lock_exclusive_blocking(&lock);

    let mut count = 0u32;
    if let Some(doc) = read_entry(&entry_path)? {
        if pid_alive(doc.pid) {
            count += 1;
        }
    }

    let att_dir = entry_path.with_extension("att");
    let entries = match fs::read_dir(&att_dir) {
        Ok(entries) => entries,
        // No registration directory: no joiner has ever attached.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(count),
        Err(source) => {
            return Err(JoinError::Io {
                path: att_dir,
                source,
            })
        }
    };
    for entry in entries {
        let path = match entry {
            Ok(e) => e.path(),
            Err(_) => continue,
        };
        match fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<WaiterDoc>(&bytes).ok())
        {
            // A live registration: one live attachment.
            Some(waiter) if pid_alive(waiter.pid) => count += 1,
            // A dead one is pruned — a joiner killed outright (no Drop runs)
            // stops counting here, and the directory self-heals.
            Some(_) => {
                let _ = fs::remove_file(&path);
            }
            // An unreadable one is left for diagnosis and simply not counted:
            // a file this binary cannot parse may be a newer schema's live
            // registration, and pruning it would uncount a real watcher.
            None => {}
        }
    }
    Ok(count)
}

/// Error type for join-table operations (the [`crate::state::StateError`]
/// shape: path-carrying io/parse variants).
#[derive(Debug)]
pub enum JoinError {
    /// The join directory could not be created.
    CannotCreateJoinDir {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A file operation on a join-table path failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// An entry document exists but is not readable JSON of this schema.
    Parse { path: PathBuf, reason: String },
    /// A git resolution step behind the run identity failed (the EC-01
    /// resolvers shell out to system git, and a non-repo or broken repo has
    /// no worktree-invariant identity to hash).
    Git { repo: PathBuf, reason: String },
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CannotCreateJoinDir { path, source } => {
                write!(f, "cannot create join dir {}: {}", path.display(), source)
            }
            Self::Io { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
            Self::Parse { path, reason } => {
                write!(f, "cannot parse entry {}: {}", path.display(), reason)
            }
            Self::Git { repo, reason } => {
                write!(
                    f,
                    "git identity resolution failed in {}: {}",
                    repo.display(),
                    reason
                )
            }
        }
    }
}

impl std::error::Error for JoinError {}

/// The gantry state directory (`~/.local/state/gantry`) — the same
/// resolution the runlog uses, so the JoinTable lives beside `runs.jsonl`
/// and one HOME redirect (tests, sandboxes) moves both.
pub fn default_state_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("gantry")
    })
}

/// Claim the key for this invocation, degrading to [`JoinDecision::Unjoined`]
/// on any join-table failure: dispatch never fails for dedup's sake.
///
/// `state_dir` is the gantry state dir ([`default_state_dir`]; `None` — no
/// HOME — means no table). `backend` is the configured backend's name: an
/// in-flight entry recorded under a different backend is declined, since a
/// handle only means something to the backend that issued it.
pub fn claim(state_dir: Option<&Path>, key: &JoinKey, run_id: &str, backend: &str) -> JoinDecision {
    let Some(dir) = state_dir else {
        return JoinDecision::Unjoined;
    };
    match claim_in_polling(
        dir,
        key,
        run_id,
        backend,
        CLAIM_POLL_BUDGET,
        CLAIM_POLL_INTERVAL,
    ) {
        Ok(decision) => decision,
        Err(e) => {
            eprintln!("[gantry] warning: join table unavailable, submitting without dedup: {e}");
            JoinDecision::Unjoined
        }
    }
}

/// [`claim`] against an explicit state directory — the test seam that keeps
/// environment redirection out of the test process (a unit test cannot
/// export HOME; same explicit-path convention as `check_git_gate_in` and
/// `RefPusher::push_in`), with the production poll cadence.
pub fn claim_in(
    state_dir: &Path,
    key: &JoinKey,
    run_id: &str,
    backend: &str,
) -> Result<JoinDecision, JoinError> {
    claim_in_polling(
        state_dir,
        key,
        run_id,
        backend,
        CLAIM_POLL_BUDGET,
        CLAIM_POLL_INTERVAL,
    )
}

/// [`claim_in`] with the poll cadence injected — the unit tests exercise the
/// wedged-owner and mid-submit timeouts in milliseconds instead of waiting
/// out the production budget.
fn claim_in_polling(
    state_dir: &Path,
    key: &JoinKey,
    run_id: &str,
    backend: &str,
    poll_budget: Duration,
    poll_interval: Duration,
) -> Result<JoinDecision, JoinError> {
    let join_dir = state_dir.join("join");
    fs::create_dir_all(&join_dir).map_err(|source| JoinError::CannotCreateJoinDir {
        path: join_dir.clone(),
        source,
    })?;
    let entry_path = join_dir.join(format!("{}.run", key.name));
    let lock_path = join_dir.join(format!("{}.lock", key.name));

    // The claim decision holds the exclusive lock: entry-read, takeover, and
    // claim-write are one atomic step across processes. The lock is released
    // on return (the File guard drops) — nothing is held across a run.
    let lock = open_lock_file(&lock_path).map_err(|source| JoinError::Io {
        path: lock_path.clone(),
        source,
    })?;
    lock_exclusive_blocking(&lock);

    match read_entry(&entry_path)? {
        Some(doc) if !doc.handle.is_empty() => {
            // A handle-bearing entry is attachable regardless of its owner's
            // pid: the remote run is real, and same key means same suite
            // means same verdict (module docs, Liveness).
            if doc.backend != backend {
                eprintln!(
                    "[gantry] join skipped: in-flight run {} uses backend {}, configured is {backend}",
                    doc.handle, doc.backend
                );
                return Ok(JoinDecision::Unjoined);
            }
            return attach_decision(&entry_path, run_id, &doc);
        }
        // Handle-less: the owner is between claim and submit. A live owner
        // gets polled (below); a dead one took its claim to the grave, so
        // fall through and take the key over.
        Some(doc) if pid_alive(doc.pid) => {
            drop(lock);
            return poll_for_handle(
                &entry_path,
                &lock_path,
                run_id,
                backend,
                &doc,
                poll_budget,
                poll_interval,
            );
        }
        // No entry, or a handle-less claim whose owner is dead.
        _ => {}
    }

    // No live claim: this invocation owns the run.
    write_entry(&entry_path, &fresh_claim(run_id, backend))?;
    Ok(JoinDecision::Originator(JoinEntry {
        entry_path,
        run_id: run_id.to_string(),
    }))
}

/// A new owner's entry document: handle empty until its submit returns.
fn fresh_claim(run_id: &str, backend: &str) -> EntryDoc {
    EntryDoc {
        schema: ENTRY_SCHEMA,
        handle: String::new(),
        run_id: run_id.to_string(),
        pid: std::process::id(),
        backend: backend.to_string(),
        started_epoch_ms: now_epoch_ms(),
    }
}

/// Poll a live owner's handle-less claim until the handle appears (attach),
/// the entry clears (the run completed mid-poll — nothing left to join), the
/// owner dies (take over the key under our own run id), or the budget runs
/// out (submit alongside).
fn poll_for_handle(
    entry_path: &Path,
    lock_path: &Path,
    run_id: &str,
    backend: &str,
    first_seen: &EntryDoc,
    poll_budget: Duration,
    poll_interval: Duration,
) -> Result<JoinDecision, JoinError> {
    let deadline = Instant::now() + poll_budget;

    loop {
        if Instant::now() >= deadline {
            eprintln!(
                "[gantry] join skipped: in-flight run {} never reported a handle; submitting alongside",
                first_seen.run_id
            );
            return Ok(JoinDecision::Unjoined);
        }
        std::thread::sleep(poll_interval);

        let lock = open_lock_file(lock_path).map_err(|source| JoinError::Io {
            path: lock_path.to_path_buf(),
            source,
        })?;
        lock_exclusive_blocking(&lock);

        match read_entry(entry_path)? {
            // The entry cleared while we polled: the run completed and its
            // owner closed the key — the in-flight window is shut, so a
            // fresh submission is the correct (only) move.
            None => return Ok(JoinDecision::Unjoined),
            Some(doc) => {
                if !doc.handle.is_empty() {
                    if doc.backend != backend {
                        eprintln!(
                            "[gantry] join skipped: in-flight run {} uses backend {}, configured is {backend}",
                            doc.handle, doc.backend
                        );
                        return Ok(JoinDecision::Unjoined);
                    }
                    return attach_decision(entry_path, run_id, &doc);
                }
                if !pid_alive(doc.pid) {
                    // The owner died wedged between claim and submit: its
                    // flock died with it, we hold the lock, the key is ours.
                    write_entry(entry_path, &fresh_claim(run_id, backend))?;
                    return Ok(JoinDecision::Originator(JoinEntry {
                        entry_path: entry_path.to_path_buf(),
                        run_id: run_id.to_string(),
                    }));
                }
            }
        }
        // Still submitting. The lock drops here; sleep and look again.
    }
}

// ============================================================================
// Entry and lock plumbing
// ============================================================================

/// Remove an entry only while it still belongs to `run_id`.
///
/// The ownership check and removal share the claim lock. Without that
/// critical section, an old originator guard or joiner release could read its
/// own document, lose the lock to a fresh claim, and then remove the fresh
/// epoch's entry. Cleanup is best-effort at its call sites, but it must never
/// cross an epoch boundary when the filesystem is healthy.
fn remove_entry_if_owner(entry_path: &Path, run_id: &str) -> Result<(), JoinError> {
    let lock_path = entry_path.with_extension("lock");
    let lock = open_lock_file(&lock_path).map_err(|source| JoinError::Io {
        path: lock_path,
        source,
    })?;
    lock_exclusive_blocking(&lock);

    if read_entry(entry_path)?
        .as_ref()
        .is_some_and(|doc| doc.run_id == run_id)
    {
        match fs::remove_file(entry_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(JoinError::Io {
                    path: entry_path.to_path_buf(),
                    source,
                })
            }
        }
    }
    Ok(())
}

/// Read the entry document at `path`; `None` when there is none. A document
/// that is not JSON of [`ENTRY_SCHEMA`] — malformed, or well-formed from a
/// different schema version — is a parse error, never a stealable claim: it
/// may describe a genuinely in-flight run this binary cannot understand.
fn read_entry(path: &Path) -> Result<Option<EntryDoc>, JoinError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|source| JoinError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let parse_error = |reason: String| JoinError::Parse {
        path: path.to_path_buf(),
        reason,
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| parse_error(format!("not JSON: {e}")))?;
    if value.get("schema").and_then(|s| s.as_u64()) != Some(u64::from(ENTRY_SCHEMA)) {
        return Err(parse_error(format!(
            "foreign or missing schema (expected {ENTRY_SCHEMA})"
        )));
    }
    let doc: EntryDoc =
        serde_json::from_slice(&bytes).map_err(|e| parse_error(format!("wrong shape: {e}")))?;
    Ok(Some(doc))
}

/// Write the entry document atomically: temp file + rename, so a concurrent
/// reader sees the old or the new document, never a torn one. The temp name
/// is key-derived, and writers of one key are serialized by its claim lock.
fn write_entry(path: &Path, doc: &EntryDoc) -> Result<(), JoinError> {
    let json = serde_json::to_vec(doc).map_err(|source| JoinError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    })?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, json).map_err(|source| JoinError::Io {
        path: tmp.clone(),
        source,
    })?;
    fs::rename(&tmp, path).map_err(|source| JoinError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Open (or create) a claim-lock file for flock use.
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Take `LOCK_EX` blocking, giving up only on a persistent non-EINTR error
/// (the same microseconds-long-critical-section posture as the Component 6
/// queue lock: the decision under it is two stat calls and a rename).
#[cfg(unix)]
fn lock_exclusive_blocking(file: &File) {
    use std::os::unix::io::AsRawFd;
    loop {
        // SAFETY: flock(2) on a valid fd; it touches no memory and fails
        // cleanly with a non-zero return when the lock cannot be taken.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

/// Non-Unix degrade: no flock exists, so claim decisions are unsynchronized
/// there (the same posture as the Component 6 semaphore's non-Unix arms).
#[cfg(not(unix))]
fn lock_exclusive_blocking(_file: &File) {}

/// Whether `pid` names a live process: `kill(pid, 0)` succeeds (alive, ours)
/// or fails with EPERM (alive, not ours — possible under a shared state
/// dir). Only ESRCH means gone.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill(2) with signal 0 performs a permission check and touches
    // no memory; the errno read below is the documented error channel.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Non-Unix degrade: every claim looks alive, so the poll budget (not the
/// pid check) bounds a wedged owner's hold on the key.
#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed key world so every test hashes the same tuple shape.
    fn key(repo_url: &str, sha: &str, sub: &str, args: &[&str]) -> JoinKey {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        JoinKey::new(repo_url, sha, "cargo", sub, &owned)
    }

    /// Forge an entry document directly (claims from tests write through
    /// [`write_entry`] so the on-disk shape stays the production shape).
    fn forge(dir: &Path, k: &JoinKey, handle: &str, run_id: &str, pid: u32) {
        let path = dir.join("join").join(format!("{}.run", k.name));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_entry(
            &path,
            &EntryDoc {
                schema: ENTRY_SCHEMA,
                handle: handle.to_string(),
                run_id: run_id.to_string(),
                pid,
                backend: "command".to_string(),
                started_epoch_ms: now_epoch_ms(),
            },
        )
        .unwrap();
    }

    fn entry_path(dir: &Path, k: &JoinKey) -> PathBuf {
        dir.join("join").join(format!("{}.run", k.name))
    }

    // ------------------------------------------------------------------
    // Key hashing
    // ------------------------------------------------------------------

    #[test]
    fn join_key_is_stable_and_field_separated() {
        let a = key("file:///r", "sha1", "test", &["--lib"]);
        let b = key("file:///r", "sha1", "test", &["--lib"]);
        assert_eq!(a, b, "identical tuples must hash identically");

        // Field separation: the same bytes regrouped across two fields must
        // not collide ("ab"+"c" vs "a"+"bc").
        assert_ne!(
            key("ab", "c", "", &[]),
            key("a", "bc", "", &[]),
            "field separator must keep regrouped bytes apart"
        );

        // Every tuple component participates.
        assert_ne!(key("u", "s", "t", &["x"]), key("u2", "s", "t", &["x"]));
        assert_ne!(key("u", "s", "t", &["x"]), key("u", "s2", "t", &["x"]));
        assert_ne!(key("u", "s", "t", &["x"]), key("u", "s", "t2", &["x"]));
        assert_ne!(key("u", "s", "t", &["x"]), key("u", "s", "t", &["y"]));
        assert_ne!(key("u", "s", "t", &["x"]), key("u", "s", "t", &["x", "y"]));

        // Filename-safe: hex, fixed length.
        let name = a.name.clone();
        assert_eq!(name.len(), 16, "FNV-1a 64 renders as 16 hex chars");
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ------------------------------------------------------------------
    // Claim lifecycle: originate → record → attach → drop → re-originate
    // ------------------------------------------------------------------

    #[test]
    fn first_claim_originates_second_attaches_third_reoriginates() {
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);

        // First invocation owns the run.
        let owner = claim_in(dir.path(), &k, "run-A", "command").unwrap();
        let entry = match owner {
            JoinDecision::Originator(e) => e,
            other => panic!("first claim must originate, got {other:?}"),
        };
        assert!(
            entry_path(dir.path(), &k).exists(),
            "claim writes the entry"
        );

        // Handle recorded → the attach window opens.
        entry.record_handle(&RunHandle::new("run-A-handle"));

        let joiner = claim_in(dir.path(), &k, "run-B", "command").unwrap();
        match joiner {
            JoinDecision::Attach(a) => {
                assert_eq!(a.handle.handle, "run-A-handle");
                assert_eq!(a.originator_run_id, "run-A");
                // The joiner's terminal-verdict reclaim: the joined run is
                // over, so the entry clears even under the originator's live
                // guard — a key whose originator died before its guard could
                // is reclaimed exactly this way.
                a.release();
            }
            other => panic!("second claim must attach, got {other:?}"),
        }
        assert!(
            !entry_path(dir.path(), &k).exists(),
            "the joiner's release reclaims the entry after the terminal verdict"
        );

        // Originator exits → guard drops → remove of an already-gone file is
        // a no-op, and the key stays closed.
        drop(entry);
        assert!(
            !entry_path(dir.path(), &k).exists(),
            "guard drop removes the entry"
        );

        let next = claim_in(dir.path(), &k, "run-C", "command").unwrap();
        assert!(
            matches!(next, JoinDecision::Originator(_)),
            "after the entry clears, a fresh invocation originates"
        );
    }

    #[test]
    fn claim_survives_a_vanished_entry_under_record_handle() {
        // record_handle on an entry someone else removed must be a silent
        // no-op (the guard's drop removes nothing), never a panic.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        let entry = match claim_in(dir.path(), &k, "run-A", "command").unwrap() {
            JoinDecision::Originator(e) => e,
            other => panic!("must originate, got {other:?}"),
        };
        fs::remove_file(entry_path(dir.path(), &k)).unwrap();
        entry.record_handle(&RunHandle::new("unused"));
        drop(entry); // remove of an already-gone file is a no-op
    }

    #[test]
    fn a_superseded_guard_never_touches_the_new_epoch() {
        // The lifecycle hazard the run_id scoping exists for: the owner's
        // guard outlives its own claim — a joiner's terminal-verdict release
        // cleared the entry and a fresh invocation re-claimed the key before
        // the owner's process got around to dropping its guard — and the
        // stale guard's operations (record, drop) must leave the new epoch's
        // claim exactly as they found it.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        let stale = match claim_in(dir.path(), &k, "run-A", "command").unwrap() {
            JoinDecision::Originator(e) => e,
            other => panic!("must originate, got {other:?}"),
        };
        // The run ends; the key moves on to a new claim epoch.
        fs::remove_file(entry_path(dir.path(), &k)).unwrap();
        let fresh = match claim_in(dir.path(), &k, "run-B", "command").unwrap() {
            JoinDecision::Originator(e) => e,
            other => panic!("the re-claim must originate, got {other:?}"),
        };
        // The stale guard fires both of its operations across the boundary.
        stale.record_handle(&RunHandle::new("run-A-handle"));
        drop(stale);

        let doc: EntryDoc =
            serde_json::from_slice(&fs::read(entry_path(dir.path(), &k)).unwrap()).unwrap();
        assert_eq!(doc.run_id, "run-B", "the new claim's identity is untouched");
        assert_eq!(doc.handle, "", "no stale handle was stamped onto it");
        assert_eq!(
            doc.pid,
            std::process::id(),
            "the new claim still owns the pid"
        );

        drop(fresh);
        assert!(
            !entry_path(dir.path(), &k).exists(),
            "the new epoch's guard still closes its own key"
        );
    }

    // ------------------------------------------------------------------
    // Liveness rules
    // ------------------------------------------------------------------

    #[test]
    fn handle_bearing_entry_attaches_even_with_a_dead_owner() {
        // A dead pid only matters while the owner is between claim and
        // submit; once the handle exists, the remote run is real regardless.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "run-X-handle", "run-X", dead_pid());
        match claim_in(dir.path(), &k, "run-B", "command").unwrap() {
            JoinDecision::Attach(a) => {
                assert_eq!(a.handle.handle, "run-X-handle");
                assert_eq!(a.originator_run_id, "run-X");
            }
            other => panic!("handle-bearing entry must attach, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn joiner_release_reclaims_a_handle_bearing_dead_originator() {
        // A SIGKILLed originator cannot run JoinEntry::drop. Once a joiner
        // waits out the real handle, its terminal release must close that
        // orphaned claim so the next identical invocation can originate.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "run-dead-handle", "run-dead", dead_pid());

        let attach = match claim_in(dir.path(), &k, "run-B", "command").unwrap() {
            JoinDecision::Attach(attach) => attach,
            other => panic!("dead handle-bearing originator must attach, got {other:?}"),
        };
        assert_eq!(
            attachment_count(dir.path(), &k).unwrap(),
            1,
            "only the live joiner counts after the originator dies"
        );

        attach.release();
        drop(attach);
        assert_eq!(attachment_count(dir.path(), &k).unwrap(), 0);
        assert!(
            matches!(
                claim_in(dir.path(), &k, "run-C", "command").unwrap(),
                JoinDecision::Originator(_)
            ),
            "a terminal joiner release must reopen the key"
        );
    }

    #[test]
    #[cfg(unix)]
    fn dead_owner_submitting_claim_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "", "run-dead", dead_pid());

        let decided = claim_in(dir.path(), &k, "run-B", "command").unwrap();
        match decided {
            JoinDecision::Originator(_) => {}
            other => panic!("dead owner's claim must be taken over, got {other:?}"),
        }
        // The takeover wrote OUR run id into the entry.
        let doc: EntryDoc =
            serde_json::from_slice(&fs::read(entry_path(dir.path(), &k)).unwrap()).unwrap();
        assert_eq!(doc.run_id, "run-B");
        assert_eq!(doc.pid, std::process::id());
    }

    #[test]
    #[cfg(unix)]
    fn live_owner_claim_polls_until_the_handle_appears() {
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "", "run-A", std::process::id()); // self = alive

        // The owner "submits": the handle appears mid-poll.
        let path = entry_path(dir.path(), &k);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let doc: EntryDoc = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            write_entry(
                &path,
                &EntryDoc {
                    handle: "run-A-handle".to_string(),
                    ..doc
                },
            )
            .unwrap();
        });

        let decided = claim_in_polling(
            dir.path(),
            &k,
            "run-B",
            "command",
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .unwrap();
        writer.join().unwrap();
        match decided {
            JoinDecision::Attach(a) => {
                assert_eq!(a.handle.handle, "run-A-handle");
                assert_eq!(a.originator_run_id, "run-A");
            }
            other => panic!("poll must land on the handle, got {other:?}"),
        }
    }

    #[test]
    fn wedged_live_owner_claim_times_out_unjoined() {
        // A live owner that never records a handle exhausts the poll budget;
        // the invocation submits alongside (Unjoined) and touches nothing.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "", "run-A", std::process::id());

        let decided = claim_in_polling(
            dir.path(),
            &k,
            "run-B",
            "command",
            Duration::from_millis(50),
            Duration::from_millis(10),
        )
        .unwrap();
        assert!(matches!(decided, JoinDecision::Unjoined), "got {decided:?}");
        // The forged entry is untouched: nobody but its owner writes it.
        let doc: EntryDoc =
            serde_json::from_slice(&fs::read(entry_path(dir.path(), &k)).unwrap()).unwrap();
        assert_eq!(doc.run_id, "run-A");
    }

    #[test]
    fn entry_cleared_mid_poll_yields_unjoined() {
        // The run completed (entry removed) between claim-read and the poll:
        // the in-flight window shut, so the caller submits fresh.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "", "run-A", std::process::id());

        let path = entry_path(dir.path(), &k);
        let remover = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            fs::remove_file(&path).unwrap();
        });

        let decided = claim_in_polling(
            dir.path(),
            &k,
            "run-B",
            "command",
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .unwrap();
        remover.join().unwrap();
        assert!(matches!(decided, JoinDecision::Unjoined), "got {decided:?}");
    }

    #[test]
    fn foreign_backend_entry_is_declined_unjoined() {
        // A handle only means something to the backend that issued it; a
        // joiner configured for another backend must not wait on it.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "run-X-handle", "run-X", std::process::id());

        let decided = claim_in(dir.path(), &k, "run-B", "argo").unwrap();
        assert!(matches!(decided, JoinDecision::Unjoined), "got {decided:?}");
        // The declined entry stays put for its own backend's joiners.
        assert!(entry_path(dir.path(), &k).exists());
    }

    #[test]
    fn unreadable_entry_degrades_to_a_parse_error() {
        // A corrupt entry must never be stolen (it may describe a live run
        // from a foreign version): the claim errors, and `claim` degrades to
        // Unjoined on the caller's behalf.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        let path = entry_path(dir.path(), &k);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{not json").unwrap();

        let err = claim_in(dir.path(), &k, "run-B", "command").unwrap_err();
        assert!(matches!(err, JoinError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn foreign_schema_entry_is_never_stolen() {
        // Same rule for a well-formed document from a different schema
        // version: unparseable-as-ours, so the claim declines.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        let path = entry_path(dir.path(), &k);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, br#"{"schema": 999, "handle": "h"}"#).unwrap();

        let err = claim_in(dir.path(), &k, "run-B", "command").unwrap_err();
        assert!(matches!(err, JoinError::Parse { .. }), "got {err:?}");
    }

    // ------------------------------------------------------------------
    // Attachments: the handle-bearing claim, waiter lifecycle, live counting
    // ------------------------------------------------------------------

    #[test]
    fn claim_against_handle_bearing_entry_yields_originators_run_handle() {
        // The store contract the joiner skip builds on (gantry-db9df1c6): a
        // claim against a handle-bearing in-flight entry resolves to Attach
        // carrying the *originator's* RunHandle — the handle a joiner waits
        // on instead of submitting — and the claim registers that joiner as
        // a live watcher while it holds the attach.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "run-A-handle", "run-A", std::process::id());

        match claim_in(dir.path(), &k, "run-B", "command").unwrap() {
            JoinDecision::Attach(a) => {
                assert_eq!(
                    a.handle.handle, "run-A-handle",
                    "the originator's RunHandle rides the attach, not a fresh one"
                );
                assert_eq!(a.originator_run_id, "run-A");
                assert_eq!(
                    attachment_count(dir.path(), &k).unwrap(),
                    2,
                    "originator entry + this joiner's live registration"
                );
            }
            other => panic!("handle-bearing entry must attach, got {other:?}"),
        }
    }

    #[test]
    fn claim_with_no_entry_still_originates_fresh() {
        // The other half of the contract: with nothing in flight, the claim
        // is a fresh originator — the caller submits its own run — and no
        // watcher state exists beyond the originator's own entry.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        let att_dir = dir.path().join("join").join(format!("{}.att", k.name));

        let entry = match claim_in(dir.path(), &k, "run-A", "command").unwrap() {
            JoinDecision::Originator(e) => e,
            other => panic!("empty key must originate, got {other:?}"),
        };
        assert_eq!(
            attachment_count(dir.path(), &k).unwrap(),
            1,
            "only the originator's entry is watching"
        );
        assert!(!att_dir.exists(), "an unattached claim creates no waiters");

        drop(entry);
        assert_eq!(
            attachment_count(dir.path(), &k).unwrap(),
            0,
            "the guard's drop closes the key"
        );
    }

    #[test]
    fn dropped_attach_detaches_its_waiter() {
        // The detach (Drop): a joiner that exits without a terminal verdict
        // — an error path, a Ctrl-C — stops counting when its AttachHandle
        // drops, instead of pinning the key against supersede forever.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "run-A-handle", "run-A", std::process::id());

        let attach = match claim_in(dir.path(), &k, "run-B", "command").unwrap() {
            JoinDecision::Attach(a) => a,
            other => panic!("must attach, got {other:?}"),
        };
        assert_eq!(attachment_count(dir.path(), &k).unwrap(), 2);

        drop(attach);
        assert_eq!(
            attachment_count(dir.path(), &k).unwrap(),
            1,
            "the waiter registration went with the drop; the originator remains"
        );
        let att_dir = dir.path().join("join").join(format!("{}.att", k.name));
        assert!(
            fs::read_dir(&att_dir).unwrap().next().is_none(),
            "the registration directory is empty again"
        );
    }

    #[test]
    #[cfg(unix)]
    fn dead_joiners_waiter_is_pruned_not_counted() {
        // A joiner killed outright runs no Drop, so its registration stays;
        // the next count prunes it (dead pid) instead of pinning the key.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "run-A-handle", "run-A", std::process::id());
        let att_dir = dir.path().join("join").join(format!("{}.att", k.name));
        fs::create_dir_all(&att_dir).unwrap();
        fs::write(
            att_dir.join("run-dead"),
            serde_json::to_vec(&WaiterDoc {
                pid: dead_pid(),
                started_epoch_ms: now_epoch_ms(),
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            attachment_count(dir.path(), &k).unwrap(),
            1,
            "only the originator counts; the dead joiner was pruned"
        );
        assert!(
            !att_dir.join("run-dead").exists(),
            "the dead registration was removed on the way past"
        );
    }

    #[test]
    fn a_stale_attach_release_never_touches_the_new_epoch() {
        // release() is scoped like the guards: a joiner's terminal release
        // that lands after the key moved to a fresh claim epoch (its own
        // release let a new invocation originate mid-wait) leaves that new
        // claim exactly as it found it.
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        forge(dir.path(), &k, "run-A-handle", "run-A", std::process::id());
        let attach = match claim_in(dir.path(), &k, "run-B", "command").unwrap() {
            JoinDecision::Attach(a) => a,
            other => panic!("must attach, got {other:?}"),
        };

        // The run ends and the key moves on to a new claim epoch before the
        // joiner's release fires.
        fs::remove_file(entry_path(dir.path(), &k)).unwrap();
        let fresh = match claim_in(dir.path(), &k, "run-C", "command").unwrap() {
            JoinDecision::Originator(e) => e,
            other => panic!("the re-claim must originate, got {other:?}"),
        };

        attach.release();
        let doc: EntryDoc =
            serde_json::from_slice(&fs::read(entry_path(dir.path(), &k)).unwrap()).unwrap();
        assert_eq!(doc.run_id, "run-C", "the new claim's identity is untouched");
        assert_eq!(doc.handle, "", "the new claim is still handle-less");

        // The stale attach still detaches only itself.
        drop(attach);
        assert_eq!(
            attachment_count(dir.path(), &k).unwrap(),
            1,
            "only the new epoch's originator counts"
        );
        drop(fresh);
    }

    // ------------------------------------------------------------------
    // Fixtures
    // ------------------------------------------------------------------

    /// Run one git command in `dir`, asserting success (the same fixture
    /// posture as the GitGate tests: shell out to system git, explicit dir,
    /// stderr in the panic message).
    fn git_cmd(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A minimal repo with one commit and an `origin` remote pointing at a
    /// bare repo (the same fixture shape the GitGate tests use). Returns
    /// (repo, remote); both removed on drop.
    fn fixture_repo_with_remote() -> (tempfile::TempDir, tempfile::TempDir) {
        let repo = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        git_cmd(
            repo.path(),
            &["init", "--bare", remote.path().to_str().unwrap()],
        );
        git_cmd(repo.path(), &["init"]);
        git_cmd(repo.path(), &["config", "user.name", "JoinTable Tests"]);
        git_cmd(repo.path(), &["config", "user.email", "jointable@test"]);
        git_cmd(
            repo.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        std::fs::write(repo.path().join("file.txt"), "content").unwrap();
        git_cmd(repo.path(), &["add", "."]);
        git_cmd(repo.path(), &["commit", "-m", "initial"]);
        (repo, remote)
    }

    /// A linked worktree of `repo` on a fresh branch at HEAD (EC-01's
    /// two-worktrees-of-one-repo world). Removed with the repo on drop.
    fn add_worktree(repo: &Path) -> tempfile::TempDir {
        let wt = tempfile::tempdir().unwrap();
        git_cmd(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                "jointable-side",
                wt.path().to_str().unwrap(),
            ],
        );
        wt
    }

    /// A pid that is guaranteed dead: a reaped child. (Linux recycles pids,
    /// but a just-reaped pid is about as dead as a pid gets.)
    #[cfg(unix)]
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("reap child");
        pid
    }

    // ------------------------------------------------------------------
    // Worktree-invariant identity (plan EC-01)
    // ------------------------------------------------------------------

    #[test]
    fn identity_and_key_are_stable_across_two_worktrees_of_one_repo() {
        let (repo, _remote) = fixture_repo_with_remote();
        let wt = add_worktree(repo.path());

        // One identity from both worktrees: same remote URL (the shared
        // config), same sha (the shared commit) — EC-01's dedup property.
        let id_main = resolve_repo_identity(repo.path(), "HEAD").unwrap();
        let id_wt = resolve_repo_identity(wt.path(), "HEAD").unwrap();
        assert_eq!(id_main, id_wt, "one repo, one commit, one identity");
        assert_eq!(
            id_main.repo,
            _remote.path().to_str().unwrap(),
            "identity is the remote URL, never a checkout path"
        );

        // The key follows: identical from both worktrees.
        let k_main = key_for_repo(repo.path(), "HEAD", "cargo", "test", &[]).unwrap();
        let k_wt = key_for_repo(wt.path(), "HEAD", "cargo", "test", &[]).unwrap();
        assert_eq!(k_main, k_wt, "two worktrees of one repo compute one key");

        // Diverge the worktree onto its own commit: same remote, different
        // sha — the keys must come apart (different runs, no false join).
        std::fs::write(wt.path().join("side.txt"), "side").unwrap();
        git_cmd(wt.path(), &["add", "."]);
        git_cmd(wt.path(), &["commit", "-m", "side commit"]);
        let id_diverged = resolve_repo_identity(wt.path(), "HEAD").unwrap();
        assert_eq!(id_diverged.repo, id_main.repo, "the remote did not move");
        assert_ne!(id_diverged.sha, id_main.sha, "the commit did");
        assert_ne!(
            key_for_repo(wt.path(), "HEAD", "cargo", "test", &[]).unwrap(),
            k_main,
            "different commits must not share a key"
        );
    }

    #[test]
    fn identity_falls_back_to_the_common_git_dir_without_a_remote() {
        // EC-01's own spelling: with no remote to name the repo, the common
        // git dir does — and it is the same for every worktree, unlike the
        // per-worktree `--git-dir`.
        let repo = tempfile::tempdir().unwrap();
        git_cmd(repo.path(), &["init"]);
        git_cmd(repo.path(), &["config", "user.name", "JoinTable Tests"]);
        git_cmd(repo.path(), &["config", "user.email", "jointable@test"]);
        std::fs::write(repo.path().join("file.txt"), "content").unwrap();
        git_cmd(repo.path(), &["add", "."]);
        git_cmd(repo.path(), &["commit", "-m", "initial"]);
        let wt = add_worktree(repo.path());

        let id_main = resolve_repo_identity(repo.path(), "HEAD").unwrap();
        let id_wt = resolve_repo_identity(wt.path(), "HEAD").unwrap();
        assert_eq!(id_main, id_wt, "the common dir is shared across worktrees");
        let common = crate::gate::git_common_dir_in(repo.path()).unwrap();
        let expected = std::fs::canonicalize(&common).unwrap();
        assert_eq!(
            PathBuf::from(&id_main.repo),
            expected,
            "the fallback identity is the canonical common git dir"
        );
    }

    // ------------------------------------------------------------------
    // Contention: exclusive create under concurrent claims
    // ------------------------------------------------------------------

    #[test]
    fn concurrent_claims_admit_exactly_one_originator() {
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        const CALLERS: usize = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(CALLERS));
        let mut threads = Vec::new();
        for i in 0..CALLERS {
            let barrier = std::sync::Arc::clone(&barrier);
            let dir = dir.path().to_path_buf();
            let k = k.clone();
            threads.push(std::thread::spawn(move || {
                // The claim decision holds the exclusive lock end to end, so
                // exactly one thread can read "no entry" and write its claim;
                // every other thread reads that entry and joins or bows out.
                // Short poll budget: no originator records a handle here, so
                // losers exhaust the budget and submit alongside (Unjoined).
                barrier.wait();
                claim_in_polling(
                    &dir,
                    &k,
                    &format!("run-{i}"),
                    "command",
                    Duration::from_millis(200),
                    Duration::from_millis(5),
                )
            }));
        }
        // Collect every decision before dropping any guard, so no originator
        // closes its key while a late claimer is still deciding.
        let decisions: Vec<JoinDecision> = threads
            .into_iter()
            .map(|t| t.join().unwrap().unwrap())
            .collect();
        let originators = decisions
            .iter()
            .filter(|d| matches!(d, JoinDecision::Originator(_)))
            .count();
        assert_eq!(
            originators, 1,
            "exactly one concurrent claim may originate, got {decisions:?}"
        );
    }

    // ------------------------------------------------------------------
    // Stale reclaim after the lock holder dies (real cross-process flock)
    // ------------------------------------------------------------------

    /// Non-blocking probe: true iff the exclusive lock was taken. Test-only —
    /// production claims block by design.
    #[cfg(unix)]
    fn try_lock_exclusive(file: &File) -> bool {
        use std::os::unix::io::AsRawFd;
        // SAFETY: flock(2) on a valid fd; it touches no memory and fails
        // cleanly with a non-zero return when the lock is held elsewhere.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    }

    /// Entry point for [`killed_lock_holders_stale_entry_is_reclaimed`]'s
    /// spawn: hold the lock file named by `GANTRY_JOIN_LOCK_HOLDER`
    /// exclusively, write a handle-less claim under it, and park until
    /// killed. A no-op outside a spawn (env unset), so the suite's own run
    /// of this ignored test passes through instantly.
    #[test]
    #[ignore = "entry point for cross-process lock-holder spawns, driven by its spawning test"]
    fn jointable_lock_holder_child() {
        let Some(path) = std::env::var_os("GANTRY_JOIN_LOCK_HOLDER") else {
            return;
        };
        let lock_path = PathBuf::from(path);
        let lock = open_lock_file(&lock_path).expect("child opens the lock file");
        lock_exclusive_blocking(&lock);
        // `{key}.lock` with the extension swapped is `{key}.run` — the entry
        // this lock guards. Written under the held lock, tmp+rename atomic.
        let entry_path = lock_path.with_extension("run");
        write_entry(&entry_path, &fresh_claim("child-run", "command"))
            .expect("child writes its claim");
        // Park until the parent's SIGKILL. The lock fd stays open and held;
        // the kernel releases it when the process dies.
        loop {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    #[cfg(unix)]
    fn killed_lock_holders_stale_entry_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let k = key("file:///r", "sha1", "test", &[]);
        let join_dir = dir.path().join("join");
        std::fs::create_dir_all(&join_dir).unwrap();
        let lock_path = join_dir.join(format!("{}.lock", k.name));
        let entry_path = join_dir.join(format!("{}.run", k.name));

        // A real second process holds the claim lock: flock contends per open
        // file description, so the child's lock blocks this process too.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "jointable::tests::jointable_lock_holder_child",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("GANTRY_JOIN_LOCK_HOLDER", &lock_path)
            .spawn()
            .expect("spawn lock-holder child");

        // Ready when the child's claim is on disk AND its lock blocks a
        // fresh probe (each probe opens its own description and drops it, so
        // a probe that wins cannot starve the child's blocking acquire).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("lock-holder child never became ready");
            }
            let child_holds = {
                let probe = open_lock_file(&lock_path).expect("probe opens the lock file");
                !try_lock_exclusive(&probe)
            };
            if child_holds && entry_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        // The holder dies. The kernel drops its flock with the process — the
        // same release that makes a SIGKILLed originator's claim reclaimable.
        child.kill().expect("kill lock-holder child");
        child.wait().expect("reap lock-holder child");

        // The stale entry (dead pid, no handle) is taken over — and the claim
        // must not block on the dead holder's lock, which is already gone.
        let takeover = match claim_in(dir.path(), &k, "run-parent", "command").unwrap() {
            JoinDecision::Originator(entry) => entry,
            other => panic!("dead holder's claim must be taken over, got {other:?}"),
        };
        let doc: EntryDoc = serde_json::from_slice(&fs::read(&entry_path).unwrap()).unwrap();
        assert_eq!(doc.run_id, "run-parent", "the takeover rewrote the claim");
        assert_eq!(doc.pid, std::process::id(), "the takeover owns the pid");
        // The takeover's guard closes the key on drop, like any originator's.
        drop(takeover);
        assert!(
            !entry_path.exists(),
            "the takeover guard closes the key on drop"
        );
    }
}
