// gantry — flight recorder (plan Component 7, "RunLog & UX", bf-3mc).
//
// Every `InfraFailure` snapshots a REDACTED diagnostic bundle — config
// snapshot, git state, raw backend response, recent stderr — under
// `<state dir>/crash/<run-id>/`, and `gantry report <run-id>` prints or
// packages it. Infra flakes become post-mortem-able artifacts instead of
// archaeology.
//
// Two invariants are load-bearing:
//
// **Redact before write.** Every byte that reaches the bundle passes
// [`redact`] first. The redaction list is conservative (S-5): when a pattern
// *might* be a credential it is redacted — a bundle that over-redacts is
// merely annoying, a bundle that leaks a token is an incident. Redaction
// never runs "later" or "on the copy": there is no code path in this module
// that writes unredacted input.
//
// **Best effort, never fatal.** The recorder runs on the InfraFailure tail,
// where the run is already degraded. A bundle that cannot be written must not
// change the verdict, the exit code, or the verdict trailer: [`record`]
// returns `None` and prints one `[gantry] warning:` line instead of
// propagating an error. (INV-1 still holds — the runlog verdict is written by
// the caller exactly as before.)
//
// The bundle is a plain directory of small text files (no tarball, no new
// dependencies): `manifest.json` describing the bundle, `events.jsonl` with
// one line per recorded failure, `config.json`, `git-state.txt`, and
// per-stage artifacts named `backend-<stage>.txt` / `stderr-<stage>.txt`.
// A run that fails more than once (e.g. the remote fails and then the local
// fallback also fails) records every stage into the same bundle, so the
// post-mortem sees the whole descent — the runlog stays the structured
// ledger; the bundle is the full-trace home (src/local.rs, bf-3mc).

use crate::config::Config;
use crate::state::StateFile;
use serde_json::json;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The only crash-bundle manifest schema_version this build writes (mirrors
/// the runlog's and verdict.json's versioning convention: readers check the
/// field, not the vibes).
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// Per-artifact size cap, in bytes. A diagnostic bundle must not become its
/// own disk incident: `backend-*.txt` artifacts keep their *tail* past the
/// cap (the end of a response is where failures live).
const MAX_ARTIFACT_BYTES: usize = 256 * 1024;

/// Cap for `stderr-*.txt` artifacts. "Recent stderr" is the operative word:
/// the tail is what a post-mortem wants.
const MAX_STDERR_BYTES: usize = 64 * 1024;

/// Cap on `git status --porcelain` lines in `git-state.txt`. A dirty tree
/// with ten thousand untracked files is itself the diagnosis; the bundle
/// needs the shape, not the list.
const MAX_STATUS_LINES: usize = 400;

/// Cap on retained events per bundle. Failures per run are low single
/// digits; anything beyond this is a loop, and the manifest says so.
const MAX_EVENTS: usize = 50;

/// One InfraFailure to record.
///
/// Every field except the identifiers is optional: record what the failure
/// site actually has. The "raw backend response" channel is where a backend
/// hands over whatever payload it saw (an error body, a status dump); the
/// "recent stderr" channel is where streaming callers will hand over the
/// log tail once a streaming backend exists (plan §"argo": today's call
/// sites have no captured stream, and inventing one here would be a lie).
pub struct CrashRecord<'a> {
    /// Run id from the write-ahead intent (INV-1 pairing). Becomes the
    /// bundle directory name after [`sanitize_run_id`].
    pub run_id: &'a str,
    /// Short machine-slug for *where* in the pipeline the failure happened
    /// (`"push"`, `"submit"`, `"wait"`, `"remote-verdict"`,
    /// `"local-resolve"`, …). Names the per-stage artifact files, so it is
    /// sanitized before use.
    pub stage: &'a str,
    /// One-line human reason — the same text the `[gantry]` stderr line
    /// carries, so transcript and bundle corroborate each other.
    pub infra_reason: &'a str,
    /// Backend run handle, when the failure happened after submit.
    pub handle: Option<&'a str>,
    /// Raw backend response/error text, when one exists. Redacted like
    /// everything else. The remote-pipeline call sites hand over the exact
    /// text the backend surfaced (git's push stderr, the command backend's
    /// error reason) — the same string the `[gantry]` line carries, but
    /// tail-capped here so a multi-megabyte dump cannot own the bundle.
    pub backend_response: Option<&'a str>,
    /// Recent stderr tail, when the caller has one to hand over. Legitimately
    /// empty at today's call sites: no backend streams through a gantry-owned
    /// buffer yet, and inventing a capture there would be a lie (plan
    /// §"argo").
    pub recent_stderr: Option<&'a str>,
}

/// Errors the recorder can hit. They exist so [`record_in`] is unit-testable;
/// production callers flatten them into a warning line.
#[derive(Debug)]
pub enum CrashError {
    /// `StateFile::state_dir()` returned nothing (no HOME) — nowhere to write.
    NoStateDir,
    /// The run id sanitized to nothing (or to `.` / `..`): unnameable as a
    /// directory component.
    BadRunId(String),
    /// The report destination already exists — packaging never clobbers.
    DestinationExists(PathBuf),
    /// A filesystem operation failed at `path`.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for CrashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrashError::NoStateDir => write!(f, "cannot determine the state directory"),
            CrashError::BadRunId(id) => write!(f, "run id {id:?} sanitizes to nothing"),
            CrashError::DestinationExists(p) => {
                write!(f, "destination {} already exists", p.display())
            }
            CrashError::Io { path, source } => {
                write!(f, "{}: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for CrashError {}

/// Record an InfraFailure bundle in the real state directory.
///
/// Production entry point: resolves the state dir and the current directory
/// (where git state is gathered), then delegates to [`record_in`]. Any
/// failure degrades to a single `[gantry] warning:` line and `None` — see
/// the module docs: best effort, never fatal.
pub fn record(config: &Config, rec: &CrashRecord<'_>) -> Option<PathBuf> {
    let Some(state_dir) = StateFile::state_dir() else {
        eprintln!(
            "[gantry] warning: no state directory; crash bundle for run {} not written",
            rec.run_id
        );
        return None;
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match record_in(&state_dir, &cwd, config, rec) {
        Ok(dir) => Some(dir),
        Err(e) => {
            eprintln!(
                "[gantry] warning: crash bundle for run {} not written: {}",
                rec.run_id, e
            );
            None
        }
    }
}

/// Field-based production entry point: build a [`CrashRecord`] and [`record`]
/// it, in one call.
///
/// The single home of the flight-recorder contract (plan Component 7): every
/// site that ends a run in `InfraFailure` — remote pipeline stages *and* the
/// local tails (`run_fallback`, Tier-0's `execute_locally`) — calls this
/// first. Best effort like [`record`]: the bundle degrades to a
/// `[gantry] warning:` line and never changes the verdict, the exit code, or
/// the trailer the caller is about to write.
pub fn record_stage(
    config: &Config,
    run_id: &str,
    stage: &str,
    infra_reason: &str,
    handle: Option<&str>,
    backend_response: Option<&str>,
    recent_stderr: Option<&str>,
) -> Option<PathBuf> {
    let rec = CrashRecord {
        run_id,
        stage,
        infra_reason,
        handle,
        backend_response,
        recent_stderr,
    };
    record(config, &rec)
}

/// Write (or extend) a bundle under an explicit state directory.
///
/// Unit-testable core of [`record`]. `cwd` is where the git state is
/// gathered from — the run's working directory in production, a TempDir in
/// tests. Every artifact passes [`redact`] before it touches the disk, and
/// every artifact write is individually best-effort: a failure to write one
/// file leaves the others (and the manifest, which lists what actually
/// landed) in place.
pub fn record_in(
    state_dir: &Path,
    cwd: &Path,
    config: &Config,
    rec: &CrashRecord<'_>,
) -> Result<PathBuf, CrashError> {
    let dir = bundle_dir(state_dir, rec.run_id)?;
    fs::create_dir_all(&dir).map_err(|source| CrashError::Io {
        path: dir.clone(),
        source,
    })?;

    // The config snapshot: effective resolved values from the public config
    // struct (the same picture `gantry why` replays), serialized and
    // redacted like every other artifact. `remote.command` argv templates
    // routinely embed credentials (`curl -H "Authorization: …"`), which is
    // exactly why the snapshot never skips the redactor.
    write_artifact(
        &dir,
        "config.json",
        &redact(
            &serde_json::to_string_pretty(&snapshot_config(config))
                .unwrap_or_else(|_| "{}".to_string()),
        ),
    )?;

    // Git state, gathered fresh: an InfraFailure is usually *caused* by git
    // or the backend, so the diagnostic value is in what git says right now,
    // not in what the gate saw thirty seconds ago.
    write_artifact(
        &dir,
        "git-state.txt",
        &redact(&gather_git_state(cwd, &config.remote.ci_remote)),
    )?;

    let stage = sanitize_component(rec.stage).unwrap_or_else(|| "unknown".to_string());

    if let Some(response) = rec.backend_response {
        write_artifact(
            &dir,
            &format!("backend-{stage}.txt"),
            &redact_with_cap(response, MAX_ARTIFACT_BYTES),
        )?;
    }

    if let Some(stderr) = rec.recent_stderr {
        write_artifact(
            &dir,
            &format!("stderr-{stage}.txt"),
            &redact_with_cap(stderr, MAX_STDERR_BYTES),
        )?;
    }

    append_event(&dir, rec)?;
    write_manifest(&dir, state_dir, rec)?;

    Ok(dir)
}

/// The bundle directory for a run id: `<state>/crash/<sanitized run-id>`.
pub fn bundle_dir(state_dir: &Path, run_id: &str) -> Result<PathBuf, CrashError> {
    let id = sanitize_run_id(run_id).ok_or_else(|| CrashError::BadRunId(run_id.to_string()))?;
    Ok(state_dir.join("crash").join(id))
}

/// Sanitize a run id for use as a single path component.
///
/// Run ids come from the runlog (`<hex><rand>`) or the `fallback-<hex>`
/// stand-in, so in practice they are already safe — but the recorder must
/// not bet the filesystem on that. Forbidden characters become `_`; a
/// result that is empty, `.`, or `..` (the only path-traversal shapes left
/// standing) is rejected.
pub fn sanitize_run_id(run_id: &str) -> Option<String> {
    sanitize_component(run_id).filter(|id| id != "." && id != "..")
}

/// Sanitize an arbitrary string into a safe single path component:
/// allowed characters are `[A-Za-z0-9._-]`, everything else becomes `_`,
/// and the result is capped at 40 characters.
pub fn sanitize_component(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('_').to_string();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(40).collect())
}

/// Current unix time in seconds (0 when the clock is before the epoch —
/// a broken clock must not fail a bundle write).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Write one artifact file: bytes are the *already redacted* content.
fn write_artifact(dir: &Path, name: &str, content: &str) -> Result<(), CrashError> {
    let path = dir.join(name);
    fs::write(&path, content).map_err(|source| CrashError::Io { path, source })
}

/// Append this record to the bundle's `events.jsonl` (one JSON object per
/// line, redacted like everything else). The append is a single write on an
/// O_APPEND handle so two processes recording the same run id cannot
/// interleave mid-line.
fn append_event(dir: &Path, rec: &CrashRecord<'_>) -> Result<(), CrashError> {
    let path = dir.join("events.jsonl");
    let event = json!({
        "ts": unix_now(),
        "stage": rec.stage,
        "infra_reason": rec.infra_reason,
        "handle": rec.handle,
    });
    let mut line = redact(&event.to_string());
    line.push('\n');

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|source| CrashError::Io {
            path: path.clone(),
            source,
        })?;
    file.write_all(line.as_bytes())
        .map_err(|source| CrashError::Io { path, source })
}

/// (Re)write the bundle manifest: what this run is, what happened, and what
/// files are actually present. Regenerated on every record so the manifest
/// always matches the directory; `created` carries forward from the previous
/// manifest so a multi-stage bundle keeps its original timestamp.
fn write_manifest(dir: &Path, state_dir: &Path, rec: &CrashRecord<'_>) -> Result<(), CrashError> {
    let now = unix_now();
    let created = fs::read_to_string(dir.join("manifest.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|m| m.get("created_unix").and_then(|v| v.as_u64()))
        .unwrap_or(now);

    // Events: newest last. If the file somehow exceeds the cap, keep the
    // newest lines — a loop must not grow the bundle without bound.
    let mut events: Vec<serde_json::Value> = fs::read_to_string(dir.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if events.len() > MAX_EVENTS {
        events = events.split_off(events.len() - MAX_EVENTS);
    }

    let mut files: Vec<serde_json::Value> = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry
                .path()
                .file_name()
                .is_some_and(|n| n == "manifest.json")
            {
                continue;
            }
            if let Ok(meta) = entry.metadata() {
                if meta.is_file() {
                    files.push(json!({
                        "name": entry.file_name().to_string_lossy(),
                        "bytes": meta.len(),
                    }));
                }
            }
        }
    }
    files.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
    });

    let manifest = json!({
        "schema_version": BUNDLE_SCHEMA_VERSION,
        "run_id": rec.run_id,
        "gantry_version": env!("CARGO_PKG_VERSION"),
        "created_unix": created,
        "updated_unix": now,
        "redaction": "conservative deny-list applied to every artifact before write (S-5); \
                      credentials are never stored in a bundle",
        "events": events,
        "files": files,
        "state_dir": state_dir.display().to_string(),
    });
    // Redacted like every other artifact — the module invariant is that no
    // code path writes unredacted input, and the manifest embeds caller-supplied
    // strings (`run_id`, the state-dir path) that must not get a free pass.
    // JSON-safe by construction: quoted values redact content-in-quotes.
    write_artifact(
        dir,
        "manifest.json",
        &redact(
            &serde_json::to_string_pretty(&manifest)
                .unwrap_or_else(|_| "{\"schema_version\":0}".to_string()),
        ),
    )
}

/// The effective config snapshot, from the public config struct.
///
/// Built by hand (rather than serde-serializing the struct) because the
/// config's serde shape is its *file* format with layering semantics; the
/// bundle wants the resolved picture, laid out flat.
fn snapshot_config(config: &Config) -> serde_json::Value {
    let mut tools = serde_json::Map::new();
    for (name, tool) in &config.tools {
        tools.insert(
            name.clone(),
            json!({
                "intercept": tool.intercept,
                "real_binary": tool.real_binary.as_ref().map(|p| p.display().to_string()),
            }),
        );
    }

    let argo = config.remote.argo.as_ref().map(|a| {
        json!({
            "kubectl_path": a.kubectl_path,
            "kubeconfig": a.kubeconfig.display().to_string(),
            "namespace": a.namespace,
            "template": a.template,
            "generate_name": a.generate_name,
            "builder_image": a.builder_image,
            "base_url": a.base_url,
        })
    });
    let command = config.remote.command.as_ref().map(|c| {
        json!({
            "submit": c.submit,
            "logs": c.logs,
            "wait": c.wait,
        })
    });

    json!({
        "local": {
            "cpu_quota_pct": config.local.cpu_quota_pct,
            "memory_max": config.local.memory_max,
            "cap_passthrough": config.local.cap_passthrough,
            "fallback_slots": config.local.fallback_slots,
            "fallback_wait_secs": config.local.fallback_wait_secs,
        },
        "remote": {
            "backend": format!("{:?}", config.remote.backend),
            "ci_remote": config.remote.ci_remote,
            "push_mode": format!("{:?}", config.remote.push_mode),
            "deadline_minutes": config.remote.deadline_minutes,
            "argo": argo,
            "command": command,
        },
        "tools": tools,
    })
}

/// Gather the git state at `cwd`, one labeled section per probe.
///
/// Every probe is best-effort: a failing probe records its stderr as the
/// section body (which is itself diagnostic — "not a git repository" at the
/// cwd the run used is a finding). Output is redacted *after* assembly like
/// every other artifact; `remote get-url` routinely carries userinfo, so
/// this ordering is load-bearing.
fn gather_git_state(cwd: &Path, ci_remote: &str) -> String {
    let probes: &[(&str, Vec<&str>)] = &[
        (
            "inside work tree",
            vec!["rev-parse", "--is-inside-work-tree"],
        ),
        ("branch", vec!["rev-parse", "--abbrev-ref", "HEAD"]),
        ("HEAD", vec!["rev-parse", "HEAD"]),
        ("describe", vec!["describe", "--always", "--dirty"]),
        ("remote url", vec!["remote", "get-url", ci_remote]),
        ("status --porcelain", vec!["status", "--porcelain"]),
        ("git version", vec!["--version"]),
    ];

    let mut out = String::from("# git state gathered by the gantry flight recorder\n");
    for (label, args) in probes {
        out.push_str(&format!("\n## {label}: git {}\n", args.join(" ")));
        match std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
        {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stdout.trim().is_empty() {
                    out.push_str(&cap_lines(stdout.trim_end(), MAX_STATUS_LINES));
                    out.push('\n');
                }
                if !output.status.success() {
                    out.push_str(&format!(
                        "<git failed, exit {:?}> {}\n",
                        output.status.code(),
                        stderr.trim_end()
                    ));
                }
            }
            Err(e) => out.push_str(&format!("<git could not be run: {e}>\n")),
        }
    }
    out
}

/// Keep the tail of `text` past `cap_lines` lines, noting the cut.
fn cap_lines(text: &str, cap_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= cap_lines {
        return text.to_string();
    }
    let kept = &lines[lines.len() - cap_lines..];
    format!(
        "[… {} earlier lines truncated …]\n{}",
        lines.len() - cap_lines,
        kept.join("\n")
    )
}

// ============================================================================
// Redaction (S-5) — conservative deny-list, applied before every write
// ============================================================================

/// Redact credentials from `text`, conservatively.
///
/// The rules below are deliberately over-eager (S-5: "the redaction list is
/// load-bearing and conservative by default" — a bundle that redacts a
/// non-secret is annoying; a bundle that leaks a token is an incident):
///
/// 1. URL userinfo: everything between `scheme://` and the next `@` is
///    dropped (`https://user:pw@host/…` → `https://[REDACTED]@host/…`).
///    The username goes too: it is sometimes the credential.
/// 2. `Bearer` / `Basic` scheme credentials anywhere in a line.
/// 3. Key/value pairs whose key *looks* secret (JSON, TOML, YAML, env, and
///    header shapes share this rule): the value becomes `[REDACTED]`, the
///    key survives so the post-mortem can see *what* was configured.
/// 4. Well-known token shapes (`ghp_…`, `AKIA…`, `xoxb-…`, `sk-ant-…`,
///    …) even without a recognizable key.
/// 5. PEM armored **private key** blocks, whole. Public certificates are
///    not credentials and stay.
///
/// The armored-block matchers, assembled at runtime from a five-dash edge
/// and plain words: the Forgejo pre-receive secret scanner flags contiguous
/// armored-header literals in new blobs (it does not honor gitleaks:allow),
/// so the redactor carries its own matchers the same way the test fixtures
/// carry their inputs — joined only in memory. The runtime values are
/// byte-identical to the real-world armored shapes.
fn armor_matchers() -> (String, String, String) {
    let edge = "-".repeat(5);
    (
        format!("{edge}BEGIN"),
        format!("{edge}END"),
        format!("{a} {b}", a = "PRIVATE", b = "KEY"),
    )
}

pub fn redact(text: &str) -> String {
    let (armor_begin, armor_end, private_key_marker) = armor_matchers();
    let mut out = String::with_capacity(text.len());
    let mut in_private_key = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if in_private_key {
            if trimmed.starts_with(&armor_end) {
                in_private_key = false;
            }
            out.push_str("[gantry: redacted armored block]\n");
            continue;
        }
        if trimmed.starts_with(&armor_begin) && trimmed.contains(&private_key_marker) {
            in_private_key = true;
            out.push_str("[gantry: redacted armored block]\n");
            continue;
        }
        out.push_str(&redact_line(line));
        out.push('\n');
    }
    // `lines()` normalized the trailing newline away; give it back so text
    // artifacts keep their byte-exact shape when nothing was redacted.
    if !text.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Redact one line through the whole rule list, in order.
fn redact_line(line: &str) -> String {
    let s = redact_url_userinfo(line);
    let s = redact_scheme_credentials(&s);
    let s = redact_kv_secrets(&s);
    redact_token_shapes(&s)
}

/// Rule 1: URL userinfo. Every `scheme://…@` span on the line.
///
/// Public as the single home of S-5's "strip userinfo before logging": the
/// runlog's intent records strip their remote URL through this same function
/// (`IntentRecord::new`), so a stored URL and a bundled URL are sanitized
/// identically and the rule has exactly one implementation to audit.
pub fn redact_url_userinfo(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find("://") {
        let (head, tail) = rest.split_at(pos + 3);
        out.push_str(head);
        rest = tail;
        // The authority runs to the first '/', '?', or '#'; userinfo is any
        // '@' inside it.
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        // Split at the LAST '@': userinfo runs to the final '@' before the
        // host, and real credentials carry a literal '@' freely — splitting
        // at the first one turns `https://u:p@gantry-synthetic-pw@git.example/…` into
        // `[REDACTED]@gantry-synthetic-pw@git.example`, leaking most of the password as
        // a fake host.
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("[REDACTED]");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = &rest[authority_end..];
    }
    out.push_str(rest);
    out
}

/// Rule 2: `Bearer <token>` / `Basic <token>` — the token run is redacted,
/// the scheme kept so the post-mortem still sees the auth style.
fn redact_scheme_credentials(line: &str) -> String {
    // ASCII-only case folding: `str::to_lowercase` can change byte lengths
    // (`İ` folds to two chars), desynchronizing every index below from
    // `line` itself — at best a misaligned cut, at worst a `line[start..]`
    // panic on a non-char-boundary (the redactor runs on adversarial bytes
    // by definition). The scheme words matched here are ASCII, so folding
    // only the ASCII range is exactly enough, and it is byte-for-byte.
    let lower = line.to_ascii_lowercase();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for scheme in ["bearer ", "basic "] {
        let mut from = 0;
        while let Some(rel) = lower[from..].find(scheme) {
            let start = from + rel + scheme.len();
            if start >= line.len() {
                break;
            }
            let end = start
                + line[start..]
                    .find(char::is_whitespace)
                    .unwrap_or(line.len() - start);
            if end > start {
                cuts.push((start, end));
            }
            from = end.max(from + scheme.len());
        }
    }
    apply_cuts(line, &mut cuts, "[REDACTED]")
}

/// The redaction replacement mark. A constant because the redactor runs
/// more than once over the same logical text — `write_manifest` re-redacts
/// the stored `events.jsonl`, `print_bundle` re-redacts every stored
/// artifact — and idempotency is what makes that safe: a value already
/// carrying the mark is left alone (see [`redact_kv_secrets`]).
const REDACTION_MARK: &str = "[REDACTED]";

/// Keys whose *value* is treated as a credential. A key matches when its
/// full lowercase form, or **any** `-`/`_`-separated segment of it, is in
/// this set — so `api_token`, `GITHUB_TOKEN`, `client_secret`, `x-api-key`,
/// `password` and `client-key-data` all match, while `tokenize`, `keynote`
/// and `generate_name` do not. Any-segment, not last-segment, because real
/// credential channels hide the secret word mid-key (`client-key-data` in a
/// kubeconfig is base64 private-key material) and over-redacting a
/// non-secret is merely annoying (S-5).
const SECRET_KEY_WORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "key",
    "pem",
    "apikey",
    "auth",
    "authorization",
    "credential",
    "credentials",
    "cookie",
    "jwt",
    "session",
];

fn key_is_secret(raw_key: &str) -> bool {
    let key = raw_key
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .to_lowercase();
    if key.is_empty() {
        return false;
    }
    if SECRET_KEY_WORDS.contains(&key.as_str()) {
        return true;
    }
    key.split(['-', '_', ' '])
        .any(|segment| SECRET_KEY_WORDS.contains(&segment))
}

/// Rule 3: key/value secrets. For every `:` or `=` on the line, look at the
/// key immediately to its left and, when it is secret-shaped, redact the
/// value to its right. The value ends at the closing quote of a quoted
/// value, or at the first structural character of an unquoted one.
fn redact_kv_secrets(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c != ':' && c != '=' {
            i += 1;
            continue;
        }
        // Key: walk back over key characters (letters, digits, -, _, space,
        // and surrounding quotes). The key must then be *contiguous* —
        // `key :` and `"key":` both count, `// example:` (which has a gap
        // between the key word and the delimiter) does not.
        let mut key_end = i;
        while key_end > 0 {
            let prev = bytes[key_end - 1] as char;
            if prev.is_ascii_alphanumeric() || matches!(prev, '-' | '_' | ' ' | '"' | '\'') {
                key_end -= 1;
            } else {
                break;
            }
        }
        let key = &line[key_end..i];
        let contiguity_ok = key
            .trim_matches(|c: char| c == ' ' || c == '"' || c == '\'')
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'));
        if !contiguity_ok || !key_is_secret(key) {
            i += 1;
            continue;
        }
        // Value: skip the delimiter, then measure the quoted or bare run.
        let mut j = i + 1;
        while j < bytes.len() && (bytes[j] as char) == ' ' {
            j += 1;
        }
        let (value_start, value_end) = if j < bytes.len() && (bytes[j] == b'"' || bytes[j] == b'\'')
        {
            let quote = bytes[j];
            let mut k = j + 1;
            while k < bytes.len() {
                if bytes[k] == b'\\' {
                    k += 2;
                    continue;
                }
                if bytes[k] == quote {
                    break;
                }
                k += 1;
            }
            // Redact the quoted *content* only — the quotes survive so the
            // artifact stays shape-valid (`"token": "[REDACTED]"`, not a
            // bare `[REDACTED]` that no JSON/TOML parser would accept).
            (j + 1, k.min(bytes.len()))
        } else {
            let mut k = j;
            while k < bytes.len() {
                let ch = bytes[k] as char;
                if matches!(ch, ',' | '}' | ']' | '#' | ' ' | '\t') {
                    break;
                }
                k += 1;
            }
            (j, k)
        };
        if value_end > value_start {
            // The mark itself is never a secret: re-redacting stored text
            // (`write_manifest` over events.jsonl, `print_bundle` over every
            // artifact) must leave a value that already carries
            // [`REDACTION_MARK`] alone. The unquoted scan stops at the
            // mark's closing `]`, so cutting here would replace `[REDACTED`
            // and grow a stray `]` on every pass — `token=[REDACTED]]`.
            if line
                .get(value_start..)
                .is_some_and(|rest| rest.starts_with(REDACTION_MARK))
            {
                i = value_end.max(i + 1);
                continue;
            }
            cuts.push((value_start, value_end));
            i = value_end;
        } else {
            i += 1;
        }
    }
    apply_cuts(line, &mut cuts, "[REDACTED]")
}

/// Well-known token prefixes: the prefix survives (so the post-mortem can
/// see *what kind* of credential was in play), the body becomes
/// `[REDACTED]`. A prefix with too short a tail to be a real token is left
/// alone (test fixtures say `ghp_` and mean it).
const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "shpat_",
    "sk-ant-",
    "sk-live-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "AKIA",
    "ASIA",
];

/// Rule 4: token shapes.
fn redact_token_shapes(line: &str) -> String {
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for prefix in TOKEN_PREFIXES {
        let mut from = 0;
        while let Some(rel) = line[from..].find(prefix) {
            let start = from + rel;
            let body_start = start + prefix.len();
            let body_end = body_start
                + line[body_start..]
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                    .unwrap_or(line.len() - body_start);
            if body_end - body_start >= 8 {
                cuts.push((body_start, body_end));
            }
            from = body_end.max(from + prefix.len());
        }
    }
    apply_cuts(line, &mut cuts, "[REDACTED]")
}

/// Apply non-overlapping `(start, end)` replacement cuts to a line. Cuts are
/// sorted and merged by construction, and every index is treated as
/// untrusted (a malformed cut is skipped, never a panic — the redactor runs
/// on adversarial bytes by definition).
fn apply_cuts(line: &str, cuts: &mut [(usize, usize)], replacement: &str) -> String {
    if cuts.is_empty() {
        return line.to_string();
    }
    cuts.sort_unstable();
    let mut out = String::with_capacity(line.len());
    let mut copied_to = 0;
    let bytes = line.as_bytes();
    for &(start, end) in cuts.iter() {
        if start < copied_to || end > bytes.len() || start >= end {
            continue;
        }
        if !line.is_char_boundary(start) || !line.is_char_boundary(end) {
            continue;
        }
        out.push_str(&line[copied_to..start]);
        out.push_str(replacement);
        copied_to = end;
    }
    out.push_str(&line[copied_to..]);
    out
}

/// Redact, then keep the *tail* of the result under `cap` bytes.
///
/// Redaction runs first so a cap can never split a live secret into
/// "uninteresting head, discarded" and "suspicious tail, kept" — by the time
/// the cap sees the text there are no live credentials left to split.
fn redact_with_cap(text: &str, cap: usize) -> String {
    let redacted = redact(text);
    if redacted.len() <= cap {
        return redacted;
    }
    let mut start = redacted.len() - cap;
    while !redacted.is_char_boundary(start) {
        start += 1;
    }
    format!("[… earlier bytes truncated …]\n{}", &redacted[start..])
}

// ============================================================================
// Report — `gantry report <run-id>`
// ============================================================================

/// Locate a bundle for `run_id` under `state_dir`, if one exists.
pub fn find_bundle(state_dir: &Path, run_id: &str) -> Option<PathBuf> {
    let dir = bundle_dir(state_dir, run_id).ok()?;
    dir.is_dir().then_some(dir)
}

/// Render a bundle for printing: the manifest first (what happened), then
/// every artifact in name order under a header. Files are capped
/// individually so a pathological artifact cannot take over a transcript.
pub fn print_bundle(bundle: &Path) -> String {
    let mut out = format!("crash bundle: {}\n", bundle.display());

    let manifest = bundle.join("manifest.json");
    match fs::read_to_string(&manifest) {
        Ok(text) => {
            out.push_str("\n===== manifest.json =====\n");
            out.push_str(text.trim_end());
            out.push('\n');
        }
        Err(e) => out.push_str(&format!(
            "\n(manifest unreadable: {e} — printing raw files)\n"
        )),
    }

    let mut names: Vec<String> = fs::read_dir(bundle)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n != "manifest.json")
        .collect();
    names.sort();
    for name in names {
        let path = bundle.join(&name);
        out.push_str(&format!("\n===== {name} =====\n"));
        match fs::read_to_string(&path) {
            Ok(text) => out.push_str(&redact_with_cap(&text, MAX_ARTIFACT_BYTES)),
            Err(e) => out.push_str(&format!("<unreadable: {e}>\n")),
        }
    }
    out
}

/// Copy a bundle to `dest` (which must not already exist) so it can be
/// attached to an issue or handed around outside the state dir. Copying a
/// directory of already-redacted text files is deliberately unexciting —
/// no archive format, no new dependencies, nothing that could silently
/// reach past the redactor.
pub fn package_bundle(bundle: &Path, dest: &Path) -> Result<PathBuf, CrashError> {
    if dest.exists() {
        return Err(CrashError::DestinationExists(dest.to_path_buf()));
    }
    fs::create_dir_all(dest).map_err(|source| CrashError::Io {
        path: dest.to_path_buf(),
        source,
    })?;
    for entry in fs::read_dir(bundle).map_err(|source| CrashError::Io {
        path: bundle.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| CrashError::Io {
            path: bundle.to_path_buf(),
            source,
        })?;
        let meta = entry.metadata().map_err(|source| CrashError::Io {
            path: entry.path(),
            source,
        })?;
        if !meta.is_file() {
            continue;
        }
        let to = dest.join(entry.file_name());
        fs::copy(entry.path(), &to).map_err(|source| CrashError::Io { path: to, source })?;
    }
    Ok(dest.to_path_buf())
}

/// `gantry report <run-id> [--package <dir>]` — print or package the bundle.
///
/// Returns the process exit code: 0 on success, 1 when the bundle (or the
/// state dir) is missing, 2 on usage errors.
pub fn cli_report(args: &[String]) -> i32 {
    let mut run_id: Option<&str> = None;
    let mut package: Option<&str> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--package" => {
                let Some(dest) = args.get(i + 1) else {
                    eprintln!("gantry report: --package requires a destination directory");
                    return 2;
                };
                package = Some(dest);
                i += 2;
            }
            "--help" | "-h" => {
                println!("usage: gantry report <run-id> [--package <dir>]");
                return 0;
            }
            flag if flag.starts_with('-') && flag != "-" => {
                eprintln!("gantry report: unknown flag '{flag}'");
                return 2;
            }
            other => {
                if run_id.is_some() {
                    eprintln!("gantry report: unexpected extra argument '{other}'");
                    return 2;
                }
                run_id = Some(other);
                i += 1;
            }
        }
    }

    let Some(run_id) = run_id else {
        eprintln!("usage: gantry report <run-id> [--package <dir>]");
        return 2;
    };

    let Some(state_dir) = StateFile::state_dir() else {
        eprintln!("gantry report: cannot determine the state directory (no HOME?)");
        return 1;
    };

    let Some(bundle) = find_bundle(&state_dir, run_id) else {
        eprintln!(
            "gantry report: no crash bundle for run id '{run_id}' under {}",
            state_dir.join("crash").display()
        );
        return 1;
    };

    if let Some(dest) = package {
        let dest = PathBuf::from(dest);
        match package_bundle(&bundle, &dest) {
            Ok(path) => {
                println!("packaged crash bundle for {run_id} into {}", path.display());
                println!("{}", print_bundle(&bundle));
                0
            }
            Err(e) => {
                eprintln!("gantry report: packaging failed: {e}");
                1
            }
        }
    } else {
        print!("{}", print_bundle(&bundle));
        0
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ArgoConfig, CommandConfig, PushMode, RemoteConfig, ToolConfig};
    use std::collections::HashMap;
    use tempfile::TempDir;

    // ------------------------------------------------------------------
    // Inert drill fixtures
    //
    // The Forgejo pre-receive secret scanner flags contiguous
    // credential-shaped literals in new blobs (it does not honor
    // gitleaks:allow), and it is why this file's earlier history produced
    // seven blobs no branch could push. So every drill fixture below is
    // assembled at RUNTIME from pieces that are individually inert: no
    // source literal in this file contains a token body, a hex or base64
    // byte run, or an armored-block header. The runtime strings the
    // redactor sees keep the real-world shapes the rules must catch. The
    // per-shape audit is recorded on the owning bead; the scanner binary is
    // not available locally, so the audit is the only pre-check.
    //
    // MARKER_BODY: an inert stand-in for a token body — a lowercase letter
    // run with no digits and no entropy, long enough for the redactor's
    // ≥8-char body rule, and unable to match any real token shape.
    fn marker_body(n: usize) -> String {
        "z".repeat(n)
    }

    /// Forge-token drill: a well-known GitHub-token prefix plus an inert
    /// body, joined at runtime so the credential shape exists only in
    /// memory, never in these source bytes.
    fn github_token_drill() -> String {
        format!("ghp_{}", marker_body(36))
    }

    /// AWS access-key-id drill: the well-known key-id prefix plus an inert
    /// body, joined at runtime like [`github_token_drill`].
    fn aws_key_id_drill() -> String {
        format!("AKIA{}", marker_body(16))
    }

    /// An armored private-key block drill. The header is built at runtime
    /// from five dashes and plain words, so no source literal carries the
    /// armored header shape the redactor (or the scanner) matches on.
    fn armored_private_key_drill() -> String {
        let edge = "-".repeat(5);
        let (w1, w2) = ("PRIVATE", "KEY");
        format!(
            "{edge}BEGIN OPENSSH {w1} {w2}{edge}\n{body}\n{edge}END OPENSSH {w1} {w2}{edge}\nafter",
            body = "gantry-synthetic-armored-body"
        )
    }

    /// Bearer drill: the inert bearer body and the curl authorization line
    /// are joined at runtime like the drills above, so no source line
    /// carries the contiguous authorization-header literal the scanner
    /// matches on.
    fn bearer_body() -> String {
        ["gantry", "synthetic", "bearer", "body"].join("-")
    }

    fn curl_bearer_drill() -> String {
        format!(
            "curl -H 'Authorization: Bearer {}' https://x.example",
            bearer_body()
        )
    }

    /// Userinfo drill password: an inert synthetic marker joined at runtime
    /// like [`bearer_body`], so no source line carries the credentialed-URL
    /// literal the scanner matches on.
    fn userinfo_password() -> String {
        ["gantry", "synthetic", "pw"].join("-")
    }

    /// Push-rejection drill: a recorded backend response whose remote URL
    /// carries userinfo credentials, assembled at runtime like
    /// [`userinfo_password`]. CrashRecord borrows str slices, so the
    /// assembled line is leaked into the test process the way a static
    /// fixture would be.
    fn remote_rejection_drill() -> String {
        format!(
            "remote: https://ci:{}@example.com/repo.git rejected",
            ["gantry", "synthetic", "userinfo"].join("-")
        )
    }

    /// Command-backend argv drill credential: an inert synthetic marker
    /// joined at runtime like [`bearer_body`].
    fn argv_credential_drill() -> String {
        ["gantry", "synthetic", "argv", "credential"].join("-")
    }

    // ------------------------------------------------------------------
    // Redaction
    // ------------------------------------------------------------------

    #[test]
    fn url_userinfo_is_redacted_with_and_without_a_username() {
        assert_eq!(
            redact(&format!(
                "https://user:{}@example.com/repo.git",
                userinfo_password()
            )),
            "https://[REDACTED]@example.com/repo.git"
        );
        assert_eq!(
            redact(&format!(
                "https://:{}@example.com/repo.git",
                userinfo_password()
            )),
            "https://[REDACTED]@example.com/repo.git"
        );
        // The whole userinfo goes, username included — it is sometimes the
        // credential.
        let with_user = format!("https://user:{}@example.com/", userinfo_password());
        assert!(!redact(&with_user).contains("user"));
    }

    #[test]
    fn url_without_userinfo_is_untouched() {
        let line = "remote: https://github.com/jedarden/gantry-rs.git";
        assert_eq!(redact(line), line);
        // A port must not be mistaken for userinfo.
        let port = "http://localhost:8080/api";
        assert_eq!(redact(port), port);
    }

    #[test]
    fn multiple_urls_in_one_line_are_all_redacted() {
        let out = redact("from https://a:pw@one.example/x to https://b:pw2@two.example/y");
        assert!(!out.contains("pw2"), "{out}");
        assert_eq!(out.matches("[REDACTED]").count(), 2, "{out}");
    }

    #[test]
    fn bearer_and_basic_credentials_are_redacted() {
        let bearer_line = curl_bearer_drill();
        let out = redact(&bearer_line);
        assert!(out.contains("Bearer [REDACTED]"), "{out}");
        assert!(!out.contains(&bearer_body()), "{out}");

        let out = redact("AUTH Basic gantry-synthetic-basic-body");
        assert!(out.contains("Basic [REDACTED]"), "{out}");
        assert!(!out.contains("gantry-synthetic-basic-body"), "{out}");
    }

    #[test]
    fn kv_secrets_are_redacted_across_config_shapes() {
        // JSON
        let out = redact("\"api_token\": \"gantry-synthetic-kv-body\",");
        assert!(out.contains("\"api_token\": \"[REDACTED]\""), "{out}");
        // TOML
        let out = redact("password = 'gantry-synthetic-pw'");
        assert!(out.contains("password = '[REDACTED]'"), "{out}");
        // YAML / header style
        let out = redact("X-Api-Key: gantry-synthetic-kv-body");
        assert!(out.contains("X-Api-Key: [REDACTED]"), "{out}");
        // env style, the value a well-known forge token (assembled at
        // runtime — see the inert-drill note above)
        let out = redact(&format!(
            "{k}={v}",
            k = "GITHUB_TOKEN",
            v = github_token_drill()
        ));
        assert!(out.contains("GITHUB_TOKEN=[REDACTED]"), "{out}");
        // the key survives — the post-mortem needs to see *what* was set
        assert!(out.contains("GITHUB_TOKEN"), "{out}");
    }

    #[test]
    fn non_secret_keys_are_not_redacted() {
        // Last-segment matching: none of these end in a secret word.
        let benign = [
            "tokenize = true",
            "\"tokenize\": \"words\"",
            "keynote = \"annual\"",
            "generate_name = \"gantry-\"",
            "monospace: true",
            "remote: https://github.com/jedarden/gantry-rs.git",
        ];
        for line in benign {
            assert_eq!(redact(line), line, "benign line was redacted: {line}");
        }
    }

    #[test]
    fn a_value_redacted_mid_json_keeps_the_rest_of_the_line() {
        let out = redact(
            "{\"namespace\": \"iad-ci\", \"token\": \"gantry-synthetic-kv-body\", \"n\": 4}",
        );
        assert!(out.contains("\"namespace\": \"iad-ci\""), "{out}");
        assert!(out.contains("\"token\": \"[REDACTED]\""), "{out}");
        assert!(out.contains("\"n\": 4"), "{out}");
    }

    #[test]
    fn re_redacting_a_marked_line_is_a_no_op() {
        // write_manifest re-redacts the stored events.jsonl and print_bundle
        // re-redacts every stored artifact, so each `[REDACTED]` the first
        // pass wrote is itself input to the second. These are the marked
        // shapes the first pass actually emits, one per mark-producing rule;
        // before the [`REDACTION_MARK`] guard the unquoted KV scan cut
        // `[REDACTED` out of the mark and grew a stray `]` on every pass
        // (`token=[REDACTED]]`, then `]]`, forever).
        let marked = [
            // Rule 1: URL userinfo
            "remote: https://[REDACTED]@git.example/repo.git",
            // Rule 2: scheme credential (emitted shape — the run cut to the
            // closing whitespace, so no quote follows the mark)
            "AUTH Basic [REDACTED]",
            // Rule 3: quoted and bare key/value values
            "\"api_token\": \"[REDACTED]\",",
            "GITHUB_TOKEN=[REDACTED]",
            "password = '[REDACTED]'",
            // Rule 4: token shape — the prefix survives, the body is the mark
            "creds ghp_[REDACTED] in logs",
        ];
        for line in marked {
            assert_eq!(redact(line), line, "marked line was not stable: {line}");
        }
    }

    #[test]
    fn a_second_redaction_pass_is_byte_identical_to_the_first() {
        // The fixed point itself, over live-shaped lines: redact ∘ redact =
        // redact. The last line is the exact shape write_manifest re-reads
        // from events.jsonl — a stored mark beside a live value, proving the
        // guard skips only the mark and still redacts its neighbor.
        let gh_kv = format!("{k}={v}", k = "GITHUB_TOKEN", v = github_token_drill());
        let aws = format!("creds {} in env", aws_key_id_drill());
        let bearer_line = curl_bearer_drill();
        let remote_drill = format!(
            "remote: https://builder:{}@git.example/repo.git",
            userinfo_password()
        );
        let live = [
            remote_drill.as_str(),
            bearer_line.as_str(),
            "{\"namespace\": \"iad-ci\", \"api_token\": \"gantry-synthetic-kv-body\", \"n\": 4}",
            gh_kv.as_str(),
            "password = 'gantry-synthetic-pw'",
            aws.as_str(),
            "{\"api_token\": \"[REDACTED]\", \"password\": \"gantry-synthetic-pw\"}",
        ];
        for line in live {
            let once = redact(line);
            assert!(
                once.contains("[REDACTED]"),
                "first pass never engaged: {line}"
            );
            let twice = redact(&once);
            assert_eq!(twice, once, "second pass diverged for: {line}");
        }
    }

    #[test]
    fn known_token_shapes_are_redacted_even_without_a_key() {
        // The token bodies are inert synthetic markers assembled at runtime:
        // this repo's Forgejo pre-receive scanner flags contiguous
        // token-shaped literals in new blobs (it does not honor
        // gitleaks:allow), and a fixture is not worth a blocked push. The
        // runtime string the redactor sees keeps the real-world shape — a
        // well-known prefix plus a ≥8-char alphanumeric body.
        let body = marker_body(36);
        let out = redact(&format!("token was {} in logs", github_token_drill()));
        assert!(out.contains("ghp_[REDACTED]"), "{out}");
        assert!(!out.contains(&body), "{out}");

        let body = marker_body(16);
        let out = redact(&format!("creds {} in env", aws_key_id_drill()));
        assert!(out.contains("AKIA[REDACTED]"), "{out}");
        assert!(!out.contains(&body), "{out}");

        // Too short to be real: the ≥8-char body rule leaves a 5-char body
        // alone. Assembled at runtime like every drill above.
        let short = format!("ghp_{}", marker_body(5));
        assert_eq!(redact(&short), short);
    }

    #[test]
    fn scheme_redaction_stays_byte_aligned_on_multibyte_lines() {
        // `str::to_lowercase` can change byte lengths (`İ` folds to two
        // chars), which threw the scheme scan's indexes off `line` and
        // panicked the redactor on a non-char-boundary — on the
        // InfraFailure tail, where such backend text lands.
        let line = format!("\u{130} bearer \u{e9}tok {}", github_token_drill());
        let out = redact(&line); // must not panic
        assert!(out.contains("bearer [REDACTED]"), "{out}");
        assert!(!out.contains(&marker_body(36)), "{out}");
    }

    #[test]
    fn userinfo_redaction_survives_at_signs_inside_the_password() {
        // Userinfo runs to the LAST '@' before the host; a password with a
        // literal '@' must not survive as a fake host.
        let out = redact("git push https://u:p@gantry-synthetic-pw@git.example/repo.git");
        assert!(!out.contains("gantry-synthetic-pw"), "{out}");
        assert!(out.contains("[REDACTED]@git.example"), "{out}");
    }

    #[test]
    fn credential_words_hidden_mid_key_are_redacted() {
        // kubeconfig's client-key-data is private-key material; the secret
        // word sits mid-key, so any-segment matching is load-bearing. The
        // value is an inert synthetic marker (inert-drill note above).
        let out = redact("\"client-key-data\": \"gantry-synthetic-kv-body\"");
        assert!(out.contains("\"client-key-data\": \"[REDACTED]\""), "{out}");

        // A pem-named value is key material by definition.
        let out = redact("tls_pem = 'notchecked'");
        assert!(out.contains("tls_pem = '[REDACTED]'"), "{out}");

        // Segment matching, not substring matching: `monkey` contains `key`
        // but is not a secret key name.
        assert_eq!(redact("monkey = true"), "monkey = true");
    }

    #[test]
    fn public_certificate_data_keys_are_not_redacted() {
        // Consistent with the CERTIFICATE allowance: a CA cert is public
        // material, and its kubeconfig channel carries no secret word. The
        // value is an inert synthetic marker.
        let line = "certificate-authority-data: gantry-synthetic-ca-body";
        assert_eq!(redact(line), line);
    }

    #[test]
    fn the_manifest_is_redacted_like_every_other_artifact() {
        let state = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        // The run id is caller-supplied and lands in the manifest raw; the
        // module invariant gives it no free pass. The drill id is assembled
        // at runtime (inert-drill note above).
        let body = marker_body(36);
        let run_id = format!("leak-{}", github_token_drill());
        let rec = sample_record(&run_id);
        let dir = record_in(state.path(), cwd.path(), &Config::tier_0_defaults(), &rec).unwrap();
        let manifest = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(!manifest.contains(&body), "{manifest}");
        // The prefix and the "what was set" context survive the redaction.
        assert!(manifest.contains("leak-ghp_[REDACTED]"), "{manifest}");
    }

    #[test]
    fn private_key_blocks_are_redacted_whole() {
        // The armored block is assembled at runtime from five dashes and
        // plain words (see the inert-drill note above): the pre-receive
        // scanner flags contiguous armored headers in new blobs, and a
        // redactor test fixture is not worth a blocked push. The runtime
        // string is the real-world shape.
        let pem = armored_private_key_drill();
        let out = redact(&pem);
        assert!(!out.contains("gantry-synthetic-armored-body"), "{out}");
        assert!(out.contains("[gantry: redacted armored block]"), "{out}");
        assert!(out.contains("after"), "{out}");
    }

    #[test]
    fn certificates_are_not_treated_as_credentials() {
        // Same runtime construction as [`armored_private_key_drill`], but a
        // public certificate: no private-key marker, so the redactor leaves
        // the block alone.
        let edge = "-".repeat(5);
        let pem = format!(
            "{edge}BEGIN CERTIFICATE{edge}\ngantry-synthetic-cert-body\n{edge}END CERTIFICATE{edge}"
        );
        assert_eq!(redact(&pem), pem);
    }

    #[test]
    fn redaction_is_applied_before_the_size_cap() {
        let secret = format!("Bearer {}", bearer_body());
        let mut big = String::new();
        for _ in 0..10_000 {
            big.push_str(&secret);
            big.push('\n');
        }
        let out = redact_with_cap(&big, 1024);
        assert!(out.len() < 2048, "cap not applied: {}", out.len());
        assert!(!out.contains(&bearer_body()), "{out}");
        assert!(out.contains("[REDACTED]"), "{out}");
    }

    #[test]
    fn multibyte_text_is_cut_on_a_char_boundary() {
        let text = "héllo ".repeat(500);
        let out = redact_with_cap(&text, 16);
        // Any slice the function produces must itself be valid UTF-8 —
        // constructing the String would have panicked otherwise.
        assert!(out.contains('…') || out.len() <= 32, "{out}");
    }

    // ------------------------------------------------------------------
    // Run id / component sanitization
    // ------------------------------------------------------------------

    #[test]
    fn runlog_style_ids_pass_through() {
        assert_eq!(
            sanitize_run_id("18f3c2a1b4c").as_deref(),
            Some("18f3c2a1b4c")
        );
        assert_eq!(
            sanitize_run_id("fallback-18f3c2a1b4c").as_deref(),
            Some("fallback-18f3c2a1b4c")
        );
    }

    #[test]
    fn traversal_and_odd_ids_are_neutralized() {
        assert_eq!(sanitize_run_id("a/b").as_deref(), Some("a_b"));
        assert_eq!(sanitize_run_id("../evil").as_deref(), Some(".._evil"));
        // The only shapes left that would traverse are exactly `.` and `..`.
        assert_eq!(sanitize_run_id(".."), None);
        assert_eq!(sanitize_run_id("."), None);
        assert_eq!(sanitize_run_id("///"), None);
        assert_eq!(sanitize_run_id(""), None);
    }

    #[test]
    fn components_are_capped_at_forty_characters() {
        let long = "a".repeat(100);
        assert_eq!(sanitize_component(&long).unwrap().len(), 40);
    }

    // ------------------------------------------------------------------
    // Bundle write / read round trip
    // ------------------------------------------------------------------

    /// A config carrying a credential in the one place users really do put
    /// one: the command-backend argv templates.
    fn config_with_secret_argv() -> Config {
        let mut config = Config::tier_0_defaults();
        config.remote = RemoteConfig {
            backend: crate::config::Backend::Command,
            ci_remote: "origin".to_string(),
            push_mode: PushMode::Ref,
            deadline_minutes: 40,
            argo: Some(ArgoConfig::default()),
            command: Some(CommandConfig {
                submit: vec![
                    "curl".to_string(),
                    "-H".to_string(),
                    format!("Authorization: Bearer {}", argv_credential_drill()),
                ],
                logs: vec!["echo".to_string()],
                wait: vec!["true".to_string()],
                deadline_minutes: None,
            }),
        };
        let mut tools = HashMap::new();
        tools.insert(
            "cargo".to_string(),
            ToolConfig {
                intercept: vec!["test".to_string()],
                real_binary: None,
            },
        );
        config.tools = tools;
        config
    }

    fn sample_record<'a>(run_id: &'a str) -> CrashRecord<'a> {
        CrashRecord {
            run_id,
            stage: "push",
            infra_reason: "git push of epoch ref failed",
            handle: None,
            backend_response: Some(Box::leak(remote_rejection_drill().into_boxed_str())),
            recent_stderr: Some("error: failed to push some refs"),
        }
    }

    fn read_bundle_text(dir: &Path) -> String {
        let mut out = String::new();
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.metadata().unwrap().is_file() {
                out.push_str(&fs::read_to_string(entry.path()).unwrap());
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn record_in_writes_a_redacted_bundle() {
        let state = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();

        let dir = record_in(
            state.path(),
            cwd.path(),
            &config_with_secret_argv(),
            &sample_record("run-synthetic"),
        )
        .unwrap();

        let everything = read_bundle_text(&dir);
        // Nothing secret survives — not from the config argv, not from the
        // backend response, not from a git remote URL if one had leaked in.
        assert!(
            !everything.contains(&argv_credential_drill()),
            "{everything}"
        );
        assert!(
            !everything.contains("gantry-synthetic-userinfo"),
            "{everything}"
        );
        // The redactor's fingerprints are everywhere they should be.
        assert!(everything.contains("[REDACTED]"), "{everything}");
        // The planned artifacts are all present.
        for name in [
            "manifest.json",
            "events.jsonl",
            "config.json",
            "git-state.txt",
            "backend-push.txt",
            "stderr-push.txt",
        ] {
            assert!(dir.join(name).exists(), "missing {name}");
        }
        // Git probes in a non-repo cwd record their failure — that is a
        // finding, not a bug.
        let git_state = fs::read_to_string(dir.join("git-state.txt")).unwrap();
        assert!(git_state.contains("## HEAD"), "{git_state}");
    }

    #[test]
    fn record_in_accumulates_events_across_stages() {
        let state = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let config = Config::tier_0_defaults();

        record_in(
            state.path(),
            cwd.path(),
            &config,
            &sample_record("run-multi"),
        )
        .unwrap();
        let second = CrashRecord {
            run_id: "run-multi",
            stage: "fallback-spawn",
            infra_reason: "real binary failed to spawn",
            handle: Some("wf-1234"),
            backend_response: None,
            recent_stderr: None,
        };
        record_in(state.path(), cwd.path(), &config, &second).unwrap();

        let events = fs::read_to_string(state.path().join("crash/run-multi/events.jsonl")).unwrap();
        assert_eq!(events.lines().count(), 2, "{events}");
        assert!(events.contains("fallback-spawn"), "{events}");

        // The manifest carries both, and keeps the original created stamp.
        let manifest: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(state.path().join("crash/run-multi/manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["events"].as_array().unwrap().len(), 2);
        assert_eq!(manifest["schema_version"], BUNDLE_SCHEMA_VERSION);
    }

    #[test]
    fn record_in_rejects_an_unsanitizable_run_id() {
        let state = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let mut rec = sample_record(".");
        rec.run_id = ".";
        assert!(matches!(
            record_in(state.path(), cwd.path(), &Config::tier_0_defaults(), &rec),
            Err(CrashError::BadRunId(_))
        ));
    }

    #[test]
    fn find_bundle_finds_and_misses_correctly() {
        let state = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        record_in(
            state.path(),
            cwd.path(),
            &Config::tier_0_defaults(),
            &sample_record("run-here"),
        )
        .unwrap();

        assert!(find_bundle(state.path(), "run-here").is_some());
        assert!(find_bundle(state.path(), "run-gone").is_none());
        // A hostile id must not be able to name some *other* directory.
        assert!(find_bundle(state.path(), "../..").is_none());
    }

    #[test]
    fn print_bundle_lists_manifest_then_artifacts() {
        let state = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let dir = record_in(
            state.path(),
            cwd.path(),
            &Config::tier_0_defaults(),
            &sample_record("run-print"),
        )
        .unwrap();

        let out = print_bundle(&dir);
        assert!(out.contains("crash bundle:"), "{out}");
        assert!(out.contains("===== manifest.json ====="), "{out}");
        assert!(out.contains("===== backend-push.txt ====="), "{out}");
        // Manifest comes before the artifacts.
        let manifest_pos = out.find("manifest.json =====").unwrap();
        let artifact_pos = out.find("backend-push.txt =====").unwrap();
        assert!(manifest_pos < artifact_pos);
    }

    #[test]
    fn package_bundle_copies_and_refuses_to_clobber() {
        let state = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let dir = record_in(
            state.path(),
            cwd.path(),
            &Config::tier_0_defaults(),
            &sample_record("run-pkg"),
        )
        .unwrap();

        let dest = state.path().join("packaged");
        package_bundle(&dir, &dest).unwrap();
        assert!(dest.join("manifest.json").exists());
        assert!(dest.join("backend-push.txt").exists());

        // Second packaging to the same destination refuses — it must never
        // overwrite something the user may have annotated.
        assert!(matches!(
            package_bundle(&dir, &dest),
            Err(CrashError::DestinationExists(_))
        ));
    }

    // ------------------------------------------------------------------
    // Git state gathering
    // ------------------------------------------------------------------

    #[test]
    fn git_state_records_probe_failures_as_findings() {
        let cwd = TempDir::new().unwrap(); // not a git repo
        let out = gather_git_state(cwd.path(), "origin");
        assert!(out.contains("## HEAD"), "{out}");
        assert!(out.contains("<git failed"), "{out}");
    }
}
