// gantry — `gantry explain` (plan §"CLI surface"): the dry run.
//
// "explain = dry run (gates, chosen backend, exact ref — no network)". The
// checks that consult git run locally (`rev-parse`, `status --porcelain`,
// `remote get-url` are filesystem operations); nothing pushes a ref and
// nothing contacts a backend. The ref is named through
// [`crate::refs::RefPusher::ref_name_for`] so the explanation cannot drift
// from what a real push would use, and the decision itself is a pure
// function ([`decide`]) over inputs gathered once ([`gather_inputs`]) — the
// same seam the pipeline modules use — so tests drive every branch without
// touching the process working directory or a state dir (hermeticity rule).
//
// Exit codes: 0 the explanation was produced (whatever it predicts — the
// prediction is in the document), 2 usage error.

use super::{backend_name, decision_name, gate_trace_line, GateJson, SCHEMA_VERSION};
use crate::config::{Backend, Config};
use crate::gate;
use crate::refs::RefPusher;
use crate::runlog::Decision;
use crate::state;
use serde::{Deserialize, Serialize};

/// The `gantry explain --json` document. Published schema: `docs/schemas/
/// explain-v1.json`, embedded as [`super::EXPLAIN_SCHEMA`] and validated
/// against by the tests below.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExplainDoc {
    /// Contract version ([`SCHEMA_VERSION`]); the schema pins it with
    /// `{"const": 1}`.
    pub schema_version: u32,
    /// First word of the explained command, e.g. "cargo".
    pub tool: String,
    /// Second word of the command; empty when absent.
    pub subcommand: String,
    /// The argv tail the shim would receive, subcommand first — the same
    /// shape a run record's `args` carries.
    pub args: Vec<String>,
    /// False means the shim would passthrough untouched; the gates were
    /// never consulted.
    pub intercepted: bool,
    /// Why gantry is disabled (`"GANTRY_LOCAL=1"`, `"state file: gantry
    /// off"`) when a kill switch decides the run; `None` when enabled.
    pub kill_switch: Option<String>,
    /// True when the zero-config cap-only mode (backend none) is what keeps
    /// the run local.
    pub tier0: bool,
    /// What a real run would do.
    pub decision: Decision,
    /// Why this decision; empty on the eligible-remote path (the pipeline
    /// records no reason there either).
    pub reason: String,
    /// The exact step to eligibility when the GitGate blocks the run.
    pub next_action: Option<String>,
    /// Backend a remote run would use ("argo" / "command"); "none" when the
    /// decision is local.
    pub backend: String,
    /// GitGate inputs as evaluated now; `None` when the decision was made
    /// before the gates (not intercepted, kill switch, tier0).
    pub gates: Option<GateJson>,
    /// HEAD detached (EC-02: eligible, just noted); `None` when the gates
    /// were not consulted.
    pub detached: Option<bool>,
    /// The exact ref a real run would push, when the decision is remote.
    #[serde(rename = "ref")]
    pub ref_name: Option<String>,
    /// The commit sha HEAD resolves to; `None` when it does not resolve.
    pub sha: Option<String>,
}

/// The environment-dependent inputs the dry run consults, gathered once so
/// [`decide`] stays pure.
pub struct ExplainInputs {
    /// The four GitGate booleans.
    pub gates: GateJson,
    /// The gate's verdict with reasons (AS-4).
    pub eligibility: gate::Eligibility,
    /// Whether HEAD is detached (EC-02).
    pub detached: bool,
    /// What HEAD resolves to, when it resolves.
    pub sha: Option<String>,
}

/// Gather the real inputs by running the same local git checks the pipeline
/// runs — decision.rs captures these identically, `unwrap_or(false)`
/// included: a check that errors counts as failed, the same fail-closed the
/// real run applies.
///
/// The full [`gate::check_git_gate`] runs only inside a work tree: its
/// contract panics on unexpected git *errors* (a deliberate fail-loud in the
/// dispatch pipeline), and outside any repository its first check errors —
/// which would make `gantry explain` crash exactly where it is most needed
/// to explain. The dry run instead answers with the same ineligible verdict
/// the gate itself returns for that situation (`Ok(false)` branch), so the
/// explanation and the pipeline's answer stay word-for-word identical while
/// the diagnostics command never panics (EC-08: diagnostics degrade, they do
/// not crash).
pub fn gather_inputs(config: &Config) -> ExplainInputs {
    let gates = GateJson::from(&crate::runlog::GateInputs {
        worktree: gate::is_inside_work_tree().unwrap_or(false),
        head: gate::head_resolves().unwrap_or(false),
        remote: gate::remote_exists(&config.remote.ci_remote).unwrap_or(false),
        clean: gate::is_tree_clean().unwrap_or(false),
    });
    let (eligibility, detached, sha) = if gates.worktree {
        (
            gate::check_git_gate(&config.remote.ci_remote),
            gate::is_head_detached().unwrap_or(false),
            gate::head_sha().ok(),
        )
    } else {
        // The gate's own wording for a failed work-tree check.
        (
            gate::Eligibility {
                eligible: false,
                reason: "not inside a git work tree".to_string(),
                next_action: "run this command from within a git repository".to_string(),
                is_detached: false,
            },
            false,
            None,
        )
    };
    ExplainInputs {
        gates,
        eligibility,
        detached,
        sha,
    }
}

/// Compute what a real run would do. The branch order mirrors the pipeline
/// exactly ([`crate::decision::run_remote`] via main.rs): intercepted → kill
/// switch → tier0 → gates, so the dry run's answer is the pipeline's answer
/// by construction, not by convention.
pub fn decide(
    config: &Config,
    tool: &str,
    args: &[String],
    kill_switch: (bool, String),
    inputs: ExplainInputs,
) -> ExplainDoc {
    let subcommand = args.first().map(String::as_str).unwrap_or("");
    let base = ExplainDoc {
        schema_version: SCHEMA_VERSION,
        tool: tool.to_string(),
        subcommand: subcommand.to_string(),
        args: args.to_vec(),
        intercepted: config.intercepts(tool, subcommand),
        kill_switch: None,
        tier0: false,
        decision: Decision::Local,
        reason: String::new(),
        next_action: None,
        backend: "none".to_string(),
        gates: None,
        detached: None,
        ref_name: None,
        sha: None,
    };

    // 1. Not intercepted: the shim hands the argv to the real binary and the
    //    gates are never consulted — explaining them would imply they matter.
    if !base.intercepted {
        return ExplainDoc {
            reason: "not intercepted — passthrough to the real binary".to_string(),
            ..base
        };
    }

    // 2. Kill switch (GANTRY_LOCAL=1 / `gantry off`): the pipeline's first
    //    check. The run executes locally, recorded as backend "none".
    let (enabled, source) = kill_switch;
    if !enabled {
        return ExplainDoc {
            kill_switch: Some(source.clone()),
            reason: format!("kill switch: {source}"),
            ..base
        };
    }

    // 3. Tier-0 (zero-config default): interception still applies the cap and
    //    the ledger, nothing goes remote (plan §"Tier-0").
    if config.remote.backend == Backend::None {
        return ExplainDoc {
            tier0: true,
            reason: "tier0: no remote backend configured (cap-only mode)".to_string(),
            ..base
        };
    }

    // 4. The gates decide.
    let eligible = inputs.eligibility.eligible;
    let sha = inputs.sha.clone();
    let ref_name = if eligible {
        // The exact ref a real run would push, named through RefPusher (plan
        // §"CLI surface": "gates, backend, exact ref — no network").
        inputs
            .sha
            .as_ref()
            .map(|sha| RefPusher::ref_name_for(config, sha))
    } else {
        None
    };
    ExplainDoc {
        gates: Some(inputs.gates),
        detached: Some(inputs.detached),
        sha,
        decision: if eligible {
            Decision::Remote
        } else {
            Decision::Local
        },
        backend: if eligible {
            backend_name(config.remote.backend.clone()).to_string()
        } else {
            "none".to_string()
        },
        ref_name,
        reason: if eligible {
            String::new()
        } else {
            inputs.eligibility.reason.clone()
        },
        next_action: if eligible {
            None
        } else {
            Some(inputs.eligibility.next_action.clone())
        },
        ..base
    }
}

/// Human rendering — a rendering of the JSON document, never a second source
/// of truth.
pub fn render_text(doc: &ExplainDoc) -> String {
    let mut lines = Vec::with_capacity(5);
    let argv_tail = if doc.args.is_empty() {
        String::new()
    } else {
        format!(" {}", doc.args.join(" "))
    };
    lines.push(format!("explain: {}{argv_tail}", doc.tool));
    lines.push(format!(
        "decision: {} · backend: {}",
        decision_name(doc.decision),
        doc.backend
    ));
    if let Some(gates) = &doc.gates {
        lines.push(gate_trace_line(gates));
        if doc.detached == Some(true) {
            lines.push("(detached HEAD — eligible per EC-02)".to_string());
        }
        if let Some(ref_name) = &doc.ref_name {
            lines.push(format!("ref: {ref_name}"));
        }
    }
    if !doc.reason.is_empty() {
        lines.push(format!("reason: {}", doc.reason));
    }
    if let Some(next_action) = &doc.next_action {
        lines.push(format!("next: {next_action}"));
    }
    lines.join("\n") + "\n"
}

/// Entry point from main.rs: `gantry explain [--json] -- <command> [args…]`
/// (the leading `gantry explain` already stripped from `argv`). Everything
/// after `--` is the command verbatim — including further `--` separators,
/// which belong to the explained tool, not to gantry.
pub fn cli(argv: &[String]) -> u8 {
    let mut json = false;
    let mut cmd: Vec<String> = Vec::new();
    let mut verbatim = false;
    for arg in argv {
        if verbatim {
            cmd.push(arg.clone());
        } else if arg == "--" {
            verbatim = true;
        } else if arg == "--json" {
            json = true;
        } else if arg.starts_with("--") {
            eprintln!("gantry explain: unknown flag '{arg}'");
            eprintln!("usage: gantry explain [--json] -- <command> [args…]");
            return 2;
        } else {
            // Tolerate the flag-less spelling too: `gantry explain cargo test`.
            cmd.push(arg.clone());
        }
    }
    if cmd.is_empty() {
        eprintln!("gantry explain: no command given");
        eprintln!("usage: gantry explain [--json] -- <command> [args…]");
        return 2;
    }

    let load = Config::load();
    for warning in &load.warnings {
        eprintln!("[gantry] config warning: {warning}");
    }
    let config = load.config;
    let doc = decide(
        &config,
        &cmd[0],
        &cmd[1..],
        state::check_enabled(),
        gather_inputs(&config),
    );

    if json {
        match serde_json::to_string_pretty(&doc) {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("gantry explain: cannot serialize document: {e}"),
        }
    } else {
        print!("{}", render_text(&doc));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{schema, EXPLAIN_SCHEMA};
    use crate::config::PushMode;
    use crate::runlog::GateInputs;

    const TEST_SHA: &str = "abc123";

    fn inputs(worktree: bool, head: bool, remote: bool, clean: bool) -> ExplainInputs {
        let all_pass = worktree && head && remote && clean;
        ExplainInputs {
            gates: GateJson::from(&GateInputs {
                worktree,
                head,
                remote,
                clean,
            }),
            eligibility: gate::Eligibility {
                eligible: all_pass,
                reason: if all_pass {
                    String::new()
                } else {
                    "working tree has uncommitted changes".to_string()
                },
                next_action: if all_pass {
                    String::new()
                } else {
                    "commit or stash, then retry".to_string()
                },
                is_detached: false,
            },
            detached: false,
            sha: if head {
                Some(TEST_SHA.to_string())
            } else {
                None
            },
        }
    }

    fn clean_inputs() -> ExplainInputs {
        inputs(true, true, true, true)
    }

    fn dirty_inputs() -> ExplainInputs {
        inputs(true, true, true, false)
    }

    /// The hardcoded config is Tier-0 (backend none); flip the backend to
    /// command to reach the gate-driven branches.
    fn remote_config() -> Config {
        let mut config = Config::hardcoded();
        config.remote.backend = Backend::Command;
        config
    }

    fn enabled() -> (bool, String) {
        (true, String::new())
    }

    /// Validate a document against the published explain schema.
    fn assert_valid(doc: &ExplainDoc) {
        let schema: serde_json::Value =
            serde_json::from_str(EXPLAIN_SCHEMA).expect("published schema parses");
        let doc = serde_json::to_value(doc).expect("document serializes");
        schema::validate(&schema, &doc)
            .unwrap_or_else(|errors| panic!("document violates its published schema: {errors:?}"));
    }

    #[test]
    fn a_non_intercepted_command_passes_through_without_consulting_the_gates() {
        let doc = decide(
            &remote_config(),
            "cargo",
            &["build".to_string()],
            enabled(),
            clean_inputs(),
        );

        assert!(!doc.intercepted);
        assert_eq!(doc.decision, Decision::Local);
        assert_eq!(doc.gates, None);
        assert_eq!(doc.ref_name, None);
        assert!(doc.reason.contains("passthrough"), "{}", doc.reason);
    }

    #[test]
    fn an_eligible_command_would_run_remote_and_names_the_exact_ref() {
        let doc = decide(
            &remote_config(),
            "cargo",
            &["test".to_string()],
            enabled(),
            clean_inputs(),
        );

        assert!(doc.intercepted);
        assert_eq!(doc.decision, Decision::Remote);
        assert_eq!(doc.backend, "command");
        assert_eq!(doc.gates.as_ref().map(|g| g.clean), Some(true));
        assert_eq!(doc.sha.as_deref(), Some(TEST_SHA));
        let ref_name = doc.ref_name.expect("eligible implies a ref");
        assert!(
            ref_name.starts_with("refs/gantry/") && ref_name.ends_with(&format!("-{TEST_SHA}")),
            "epoch ref shape: {ref_name}"
        );
        assert!(doc.reason.is_empty());
        assert_eq!(doc.next_action, None);
    }

    #[test]
    fn an_ineligible_command_falls_back_local_with_reason_and_next_action() {
        let doc = decide(
            &remote_config(),
            "cargo",
            &["test".to_string()],
            enabled(),
            dirty_inputs(),
        );

        assert_eq!(doc.decision, Decision::Local);
        assert_eq!(doc.backend, "none");
        assert_eq!(doc.ref_name, None);
        assert_eq!(doc.reason, "working tree has uncommitted changes");
        assert_eq!(
            doc.next_action.as_deref(),
            Some("commit or stash, then retry")
        );
        // The gates DID run — they are what blocked the run.
        assert_eq!(doc.gates.as_ref().map(|g| g.clean), Some(false));
    }

    #[test]
    fn a_kill_switch_decides_before_the_gates() {
        let doc = decide(
            &remote_config(),
            "cargo",
            &["test".to_string()],
            (false, "GANTRY_LOCAL=1".to_string()),
            clean_inputs(),
        );

        assert_eq!(doc.kill_switch.as_deref(), Some("GANTRY_LOCAL=1"));
        assert_eq!(doc.decision, Decision::Local);
        assert_eq!(doc.gates, None);
        assert!(!doc.tier0);
    }

    #[test]
    fn tier0_is_what_keeps_a_zero_config_run_local() {
        let doc = decide(
            &Config::hardcoded(), // backend none
            "cargo",
            &["test".to_string()],
            enabled(),
            clean_inputs(),
        );

        assert!(doc.tier0);
        assert_eq!(doc.decision, Decision::Local);
        assert_eq!(doc.backend, "none");
        assert_eq!(doc.gates, None);
    }

    #[test]
    fn branch_push_mode_names_a_branch_ref() {
        let mut config = remote_config();
        config.remote.push_mode = PushMode::Branch;
        let doc = decide(
            &config,
            "cargo",
            &["test".to_string()],
            enabled(),
            clean_inputs(),
        );
        assert_eq!(
            doc.ref_name.as_deref(),
            Some("refs/heads/gantry/abc123"),
            "the dry run must explain the push mode that is configured"
        );
    }

    #[test]
    fn outside_a_work_tree_the_dry_run_explains_instead_of_panicking() {
        // What gather_inputs produces outside any git repository: every
        // boolean check fails closed and the full gate is answered with the
        // ineligible verdict its own first check returns — never the panic
        // the gate reserves for unexpected git errors (a diagnostics command
        // explains; EC-08).
        let inputs = ExplainInputs {
            gates: GateJson::from(&GateInputs {
                worktree: false,
                head: false,
                remote: false,
                clean: false,
            }),
            eligibility: gate::Eligibility {
                eligible: false,
                reason: "not inside a git work tree".to_string(),
                next_action: "run this command from within a git repository".to_string(),
                is_detached: false,
            },
            detached: false,
            sha: None,
        };
        let doc = decide(
            &remote_config(),
            "cargo",
            &["test".to_string()],
            enabled(),
            inputs,
        );

        assert_eq!(doc.decision, Decision::Local);
        assert_eq!(doc.backend, "none");
        assert_eq!(doc.reason, "not inside a git work tree");
        assert_eq!(
            doc.next_action.as_deref(),
            Some("run this command from within a git repository")
        );
        assert_eq!(doc.ref_name, None);
        assert_eq!(doc.sha, None);
        assert_valid(&doc);

        let text = render_text(&doc);
        assert!(text.contains("work tree NO ← here"), "{text}");
        assert!(
            text.contains("run this command from within a git repository"),
            "{text}"
        );
    }

    #[test]
    fn explain_json_validates_against_its_published_schema() {
        // Every document shape the command can emit: eligible-remote,
        // gate-blocked local, kill-switch local, tier0, and not-intercepted.
        assert_valid(&decide(
            &remote_config(),
            "cargo",
            &["test".to_string()],
            enabled(),
            clean_inputs(),
        ));
        assert_valid(&decide(
            &remote_config(),
            "cargo",
            &["test".to_string()],
            enabled(),
            dirty_inputs(),
        ));
        assert_valid(&decide(
            &remote_config(),
            "cargo",
            &["test".to_string()],
            (false, "state file: gantry off".to_string()),
            clean_inputs(),
        ));
        assert_valid(&decide(
            &Config::hardcoded(),
            "cargo",
            &["test".to_string()],
            enabled(),
            clean_inputs(),
        ));
        assert_valid(&decide(
            &Config::hardcoded(),
            "cargo",
            &["build".to_string()],
            enabled(),
            clean_inputs(),
        ));
    }

    #[test]
    fn human_text_shows_the_gates_and_the_ref_when_remote() {
        let doc = decide(
            &remote_config(),
            "cargo",
            &["test".to_string()],
            enabled(),
            clean_inputs(),
        );
        let text = render_text(&doc);
        assert!(text.contains("clean tree YES"), "{text}");
        assert!(text.contains("ref: refs/gantry/"), "{text}");
    }

    #[test]
    fn human_text_carries_the_next_action_when_blocked() {
        let doc = decide(
            &remote_config(),
            "cargo",
            &["test".to_string()],
            enabled(),
            dirty_inputs(),
        );
        let text = render_text(&doc);
        assert!(
            text.contains("reason: working tree has uncommitted changes"),
            "{text}"
        );
        assert!(text.contains("next: commit or stash, then retry"), "{text}");
    }

    #[test]
    fn usage_errors_exit_two_before_any_config_or_git_is_touched() {
        let argv: Vec<String> = vec![];
        assert_eq!(cli(&argv), 2);

        let argv: Vec<String> = vec!["--json".to_string(), "--fancy".to_string()];
        assert_eq!(cli(&argv), 2);
    }
}
