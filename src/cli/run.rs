// gantry — `gantry run`: explicit offload of an arbitrary command (plan
// §"CLI surface": `gantry run [--backend B] -- <cmd…>`; features.md v1.x:
// "explicit offload of an arbitrary command without shimming").
//
// The shim profile offloads only intercepted subcommands; `run` is the same
// pipeline on demand: everything after `--` is the wrapped argv, routed
// through GitGate → RefPusher → backend exactly as an intercepted cargo
// invocation ([`crate::decision::run_explicit`]), with the same capped-local
// fallback ladder and RunLog treatment. Exit-code fidelity is the wrapped
// command's, not gantry's: a local run passes the child's exit code through
// byte-exact, a remote run lands on the verdict ladder (0 pass, 1 failure,
// 2 infra) — the same contract the intercepted path gives `cargo test`.
//
// Exit codes (the management-CLI convention): 0 the wrapped command ran and
// its outcome is reported, 1 the run failed, 2 usage error.

use crate::config::{Backend, Config};

/// The usage line printed alongside every usage error.
const USAGE: &str = "usage: gantry run [--backend none|argo|command] -- <cmd> [args…]";

/// One parsed `gantry run` invocation: the backend override, if any, and the
/// wrapped argv (everything after `--`, program first).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRun {
    /// `--backend` override for this run only (`None` = use the configured
    /// backend). Spelled like the config: none / argo / command.
    pub backend_override: Option<Backend>,
    /// The wrapped command, program first — `["make", "-j4"]` for
    /// `gantry run -- make -j4`.
    pub cmd: Vec<String>,
}

/// Map a `--backend` value to the config enum — the same spellings
/// [`crate::cli::backend_name`] prints, so `gantry explain`'s output can be
/// pasted back as `gantry run --backend <that>`.
fn parse_backend(value: &str) -> Option<Backend> {
    match value {
        "none" => Some(Backend::None),
        "argo" => Some(Backend::Argo),
        "command" => Some(Backend::Command),
        _ => None,
    }
}

/// Parse a `gantry run` argument tail (the leading `gantry run` already
/// stripped). `Err` carries the process exit code: 2 for every usage error,
/// with the message already on stderr.
///
/// Flags are accepted only before `--` (`--backend`, in the two GNU spellings);
/// everything after `--` is the wrapped command verbatim — including further
/// `--` separators and dash-leading words, which belong to the wrapped tool,
/// never to gantry. The `--` itself is required, not optional: a wrapped
/// command's own flags (`gantry run make --version`) must never be
/// misread as gantry flags.
pub fn parse(argv: &[String]) -> Result<ParsedRun, u8> {
    let mut parsed = ParsedRun {
        backend_override: None,
        cmd: Vec::new(),
    };
    let mut verbatim = false;
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if verbatim {
            parsed.cmd.push(arg.clone());
        } else {
            match arg.as_str() {
                "--" => verbatim = true,
                "--backend" => {
                    i += 1;
                    match argv.get(i).map(String::as_str) {
                        Some(value) => match parse_backend(value) {
                            Some(backend) => parsed.backend_override = Some(backend),
                            None => {
                                eprintln!("gantry run: unknown backend '{value}'");
                                eprintln!("{USAGE}");
                                return Err(2);
                            }
                        },
                        None => {
                            eprintln!("gantry run: --backend needs a value");
                            eprintln!("{USAGE}");
                            return Err(2);
                        }
                    }
                }
                other => {
                    if let Some(value) = other.strip_prefix("--backend=") {
                        match parse_backend(value) {
                            Some(backend) => parsed.backend_override = Some(backend),
                            None => {
                                eprintln!("gantry run: unknown backend '{value}'");
                                eprintln!("{USAGE}");
                                return Err(2);
                            }
                        }
                    } else if other.starts_with('-') {
                        eprintln!("gantry run: unknown flag '{other}'");
                        eprintln!("{USAGE}");
                        return Err(2);
                    } else {
                        // A bare word before `--` is almost certainly a
                        // missing separator. Error rather than guess: the
                        // words after it belong to the wrapped command, and
                        // quieting one of them into a flag would be worse.
                        eprintln!("gantry run: expected `--` before the command (got '{other}')");
                        eprintln!("{USAGE}");
                        return Err(2);
                    }
                }
            }
        }
        i += 1;
    }
    if parsed.cmd.is_empty() {
        eprintln!("gantry run: no command given");
        eprintln!("{USAGE}");
        return Err(2);
    }
    Ok(parsed)
}

/// Entry point from main.rs: `gantry run [--backend B] -- <cmd> [args…]` (the
/// leading `gantry run` already stripped from `argv`). `repo_url` and `sha`
/// arrive from the caller so the run addresses the same repo identity the
/// intercepted pipeline records (main.rs's resolution rules, applied
/// identically to `explain` and the shim profile).
pub fn cli(argv: &[String], repo_url: &str, sha: &str) -> u8 {
    let parsed = match parse(argv) {
        Ok(parsed) => parsed,
        Err(code) => return code as u8,
    };

    let load = Config::load();
    for warning in &load.warnings {
        eprintln!("[gantry] config warning: {warning}");
    }
    let mut config = load.config;
    if let Some(backend) = parsed.backend_override {
        config.remote.backend = backend;
    }

    // The pipeline's return is the wrapped command's exit code (INV-3): clamp
    // into u8 exactly as main.rs's exit_code_from does — negative (signal on
    // a platform without the convention) degrades to 0, the rest pass verbatim.
    let code = crate::decision::run_explicit(&config, repo_url, sha, &parsed.cmd);
    if code < 0 {
        0
    } else {
        (code & 0xFF) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn double_dash_hands_the_rest_over_verbatim() {
        let parsed = parse(&argv(&["--", "make", "-j4"])).expect("parses");
        assert_eq!(parsed.backend_override, None);
        assert_eq!(parsed.cmd, argv(&["make", "-j4"]));
    }

    #[test]
    fn flags_after_double_dash_belong_to_the_wrapped_command() {
        let parsed = parse(&argv(&["--", "cargo", "test", "--", "--nocapture"])).expect("parses");
        assert_eq!(parsed.cmd, argv(&["cargo", "test", "--", "--nocapture"]));
    }

    #[test]
    fn backend_override_takes_both_spellings() {
        let parsed =
            parse(&argv(&["--backend", "none", "--", "sh", "-c", "exit 0"])).expect("parses");
        assert_eq!(parsed.backend_override, Some(Backend::None));

        let parsed = parse(&argv(&["--backend=command", "--", "true"])).expect("parses");
        assert_eq!(parsed.backend_override, Some(Backend::Command));

        let parsed = parse(&argv(&["--backend", "argo", "--", "true"])).expect("parses");
        assert_eq!(parsed.backend_override, Some(Backend::Argo));
    }

    #[test]
    fn backend_override_rejects_unknown_values() {
        assert_eq!(parse(&argv(&["--backend", "ssh", "--", "true"])), Err(2));
        assert_eq!(parse(&argv(&["--backend=latest", "--", "true"])), Err(2));
    }

    #[test]
    fn dangling_backend_flag_is_a_usage_error() {
        assert_eq!(parse(&argv(&["--backend"])), Err(2));
    }

    #[test]
    fn unknown_flag_is_a_usage_error() {
        assert_eq!(parse(&argv(&["--json", "--", "true"])), Err(2));
    }

    #[test]
    fn bare_word_without_double_dash_is_a_usage_error() {
        assert_eq!(parse(&argv(&["make", "-j4"])), Err(2));
    }

    #[test]
    fn missing_command_is_a_usage_error() {
        assert_eq!(parse(&argv(&[])), Err(2));
        assert_eq!(parse(&argv(&["--"])), Err(2));
        assert_eq!(parse(&argv(&["--backend", "none"])), Err(2));
    }

    #[test]
    fn parse_backend_uses_the_config_spellings() {
        assert_eq!(parse_backend("none"), Some(Backend::None));
        assert_eq!(parse_backend("argo"), Some(Backend::Argo));
        assert_eq!(parse_backend("command"), Some(Backend::Command));
        assert_eq!(parse_backend("None"), None);
        assert_eq!(parse_backend(""), None);
    }
}
