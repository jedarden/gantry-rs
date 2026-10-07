// gantry — `gantry init --ssh <target>`: SSH-first onboarding (plan §8
// "doctor / installer": "Onboarding must be smooth and automatic: `gantry
// init --ssh user@host` performs the whole SSH-first setup — verifies
// git+cargo on the target, writes the command-template preset config, and
// finishes with `doctor --e2e`. Install to first remote run in ~10 minutes,
// no Kubernetes required").
//
// The flow is a one-way ladder of legs; every leg that can fail names itself
// in the message so the operator lands on the fix, and no config is written
// until the host has proven it can carry a run:
//
// 1. reach   — `printf '%s\n' "$HOME"` over ssh: the host answers, and the
//              executor paths become absolute. Absolute matters twice: the
//              wrapper's `exec` line and the preset's argv must survive
//              quoting verbatim (no tilde semantics), and the probe's cargo
//              answer is already absolute, so every path downstream is the
//              same kind of word.
// 2. git     — `git --version`: the executor fetches the epoch ref with
//              git; without it no run can ever start there. The version
//              invocation, not a mere `command -v`, is the proof: a git
//              that is present but cannot run fails the host exactly as a
//              missing one does.
// 3. cargo   — `command -v cargo`, falling back to `$HOME/.cargo/bin/cargo`,
//              then `cargo --version` on whichever was located: the rustup
//              install, which the non-interactive shell ssh hands the
//              command to frequently misses (no ~/.profile sourced), can
//              also be half-broken — located but unable to run — and the
//              executor would only find out mid-run. The probed absolute
//              path is pinned into the installed wrapper, so the executor's
//              own `cargo test` inherits the same answer the probe verified.
// 4. install — the embedded reference executor (contrib/gantry-exec.sh, the
//              very copy `doctor --e2e` runs — one source of truth) plus a
//              thin wrapper pinning `GANTRY_EXEC_CARGO`, written into
//              `<home>/.local/bin` on the host.
// 5. preset  — the user config (`~/.config/gantry/config.toml`) is written
//              with `backend = "command"` and submit/logs/wait templates
//              that run the installed wrapper over ssh. A pre-existing
//              config is backed up to `<config>.init-backup`, never
//              destroyed.
// 6. e2e     — the `doctor --e2e` canary reports its verdict and owns the
//              exit code. The canary always rides the loopback backend (see
//              [`crate::doctor::run_e2e_canary`]): the pipeline mechanics it
//              proves are the ones the preset then drives over ssh, while
//              the host itself was verified by legs 1–4 — together they are
//              the plan's "ends with doctor --e2e green".
//
// Structure is the pure/effect split the rest of the CLI uses: [`parse`],
// [`preset_config_toml`], and [`wrapper_script`] are pure; [`run`] does the
// leg work against explicitly injected inputs (ssh binary, config path,
// config contents) — the same shape that makes `uninstall::execute`
// testable — so the integration tests (tests/init_ssh_integration.rs) drive
// the whole ladder through a loopback ssh stand-in without touching the
// developer's real HOME.
//
// Exit codes follow the management-CLI convention (run/why/status): 0 the
// host is onboarded and the canary is green, 1 a leg failed (the message
// names the leg and the fix), 2 usage error.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::config::Config;
use crate::doctor::{sh_quote, E2eCanaryOptions, E2E_EXECUTOR_SCRIPT};

/// Where the executor lands on the target host, under the home the reach
/// leg resolved (`<home>/.local/bin` — the same per-user bin dir the plan's
/// install.sh uses for the gantry binary itself).
const REMOTE_BIN_SUBDIR: &str = ".local/bin";

/// The executor's file names on the host: the reference script, and the
/// wrapper that pins its cargo. The preset invokes the wrapper, never the
/// script directly — the pin is the point.
const REMOTE_EXECUTOR_NAME: &str = "gantry-exec.sh";
const REMOTE_WRAPPER_NAME: &str = "gantry-exec";

/// The ssh binary `cli` resolves via PATH. A variable, not a literal at the
/// spawn site, so the doc comment and the code cannot drift apart; the
/// integration tests stand a fixture in via PATH instead of this knob.
const SSH_BINARY: &str = "ssh";

/// The cargo probe run on the host: the PATH answer if the non-interactive
/// shell can see cargo, else the rustup install under `$HOME/.cargo/bin`.
/// The located cargo must also *run* — `cargo --version` is executed, so a
/// half-broken toolchain fails the host here rather than mid-run. Prints
/// the absolute path it found — that exact string is pinned into the
/// installed wrapper — or fails with a message naming where it looked.
const CARGO_PROBE: &str = "p=$(command -v cargo 2>/dev/null) || true; \
if [ -z \"$p\" ] && [ -x \"$HOME/.cargo/bin/cargo\" ]; then p=\"$HOME/.cargo/bin/cargo\"; fi; \
if [ -z \"$p\" ]; then \
echo 'cargo not found (looked in PATH and $HOME/.cargo/bin)' >&2; exit 127; fi; \
\"$p\" --version >/dev/null 2>&1 || { \
echo \"$p found but 'cargo --version' failed\" >&2; exit 126; }; \
printf '%s\\n' \"$p\"";

/// The parsed `gantry init` argument tail.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedInit {
    /// The ssh destination the preset points at (`user@host`, or anything
    /// else ssh accepts as `<destination>` — aliases in `~/.ssh/config`
    /// included). One word: whitespace would break every command line the
    /// preset and wrapper embed it in, so the parser rejects it up front.
    pub target: String,
}

/// Parse a `gantry init` argument tail (the leading `gantry init` already
/// stripped). `Err` carries the process exit code — 2 for every usage error
/// — with the message already on stderr, matching [`crate::cli::run::parse`].
pub fn parse(argv: &[String]) -> Result<ParsedInit, u8> {
    let mut target: Option<String> = None;
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--ssh" => {
                i += 1;
                match argv.get(i).map(String::as_str) {
                    Some(t) if is_valid_target(t) => target = Some(t.to_string()),
                    Some(_) => {
                        eprintln!(
                            "gantry init: --ssh needs an ssh destination without \
                             whitespace, e.g. gantry init --ssh user@host"
                        );
                        return Err(2);
                    }
                    None => {
                        eprintln!(
                            "gantry init: --ssh needs a destination, \
                             e.g. gantry init --ssh user@host"
                        );
                        return Err(2);
                    }
                }
            }
            other => {
                eprintln!("gantry init: unknown flag '{other}'");
                eprintln!("{USAGE}");
                return Err(2);
            }
        }
        i += 1;
    }
    match target {
        Some(target) => Ok(ParsedInit { target }),
        None => {
            eprintln!("gantry init: --ssh <target> is required");
            eprintln!("{USAGE}");
            Err(2)
        }
    }
}

/// A target must be one whitespace-free word: it is embedded verbatim in the
/// preset's argv, the wrapper's comment, and every leg's message — a
/// newline or space in it would inject across those lines.
fn is_valid_target(target: &str) -> bool {
    !target.is_empty() && !target.chars().any(|c| c.is_whitespace() || c.is_control())
}

const USAGE: &str = "usage: gantry init --ssh <target>   e.g. gantry init --ssh user@host";

fn print_usage() {
    println!("usage: gantry init --ssh <target>");
    println!();
    println!("SSH-first onboarding (plan §8): verifies git and cargo on the");
    println!("target host, installs the reference executor there, writes the");
    println!("command-template preset config, and finishes with the doctor");
    println!("--e2e canary. Exit 1 names the leg that failed; nothing is");
    println!("written until the host has passed verification.");
    println!();
    println!("  --ssh <target>  Destination to onboard (user@host, or an ssh");
    println!("                  config alias). Required.");
    println!("  --help, -h      This help.");
}

/// The injectable inputs [`run`] works against — everything the production
/// [`cli`] would otherwise read from the process environment.
pub struct InitOptions<'a> {
    /// The ssh binary to invoke (production: `ssh` from PATH).
    pub ssh: &'a Path,
    /// The user config file the preset is written to.
    pub config_path: &'a Path,
    /// The config the finishing canary rides. The preset init writes steers
    /// backend and templates; the canary itself reads only `ci_remote`,
    /// `push_mode`, and the command deadline, none of which init touches —
    /// so this may legitimately be loaded before the write, and an /etc or
    /// repo-layer deadline still applies.
    pub config: Config,
    /// Run the `doctor --e2e` canary as the finishing leg. Production is
    /// true; tests that only exercise the verification legs may skip the
    /// canary's compile cost.
    pub run_e2e: bool,
}

/// The management-CLI entry: parse, derive the real targets, run the ladder.
pub fn cli(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return 0;
    }
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(code) => return code as i32,
    };
    let Some(config_dir) = dirs::config_dir() else {
        eprintln!("gantry init: cannot determine the user config directory");
        return 1;
    };
    let config_path = config_dir.join("gantry/config.toml");
    let config = Config::load().config;
    run(
        &parsed.target,
        &InitOptions {
            ssh: Path::new(SSH_BINARY),
            config_path: &config_path,
            config,
            run_e2e: true,
        },
    )
}

/// The onboarding ladder itself. Every failure exits 1 through
/// [`leg_failure`] with the leg named first; the config write (leg 5) is
/// unreachable until both verification legs have passed.
pub fn run(target: &str, opts: &InitOptions) -> i32 {
    // Leg: reach — the host answers, and its home pins every later path.
    let home = match ssh_run(opts.ssh, target, "printf '%s\\n' \"$HOME\"", None) {
        Ok(home) if !home.is_empty() => home,
        Ok(_) => {
            return leg_failure(
                "reach",
                &format!(
                    "{target} answered but printed an empty $HOME — \
                          the executor needs a home directory to install into"
                ),
            )
        }
        Err(e) => return leg_failure("reach", &reach_failure(target, &e)),
    };
    println!("[gantry init] {target}: reachable (home {home})");

    // Leg: git — the executor fetches the epoch ref with git. The probe
    // runs `git --version`, not just `command -v`: the version invocation
    // proves the binary works, and a git that cannot run is no better than
    // a git that is absent.
    if let Err(e) = ssh_run(opts.ssh, target, "git --version >/dev/null 2>&1", None) {
        return leg_failure(
            "git",
            &format!(
                "{e} — install git on {target} (e.g. `apt install git` or \
                 `dnf install git`) and re-run gantry init --ssh {target}"
            ),
        );
    }
    println!("[gantry init] {target}: git ok");

    // Leg: cargo — PATH answer or the rustup fallback; the absolute path is
    // pinned into the wrapper so the executor sees the same toolchain.
    let cargo = match ssh_run(opts.ssh, target, CARGO_PROBE, None) {
        Ok(path) if !path.is_empty() => path,
        Ok(_) => return leg_failure("cargo", "the probe printed nothing"),
        Err(e) => return leg_failure("cargo", &cargo_failure(target, &e)),
    };
    println!("[gantry init] {target}: cargo ok ({cargo})");

    // Leg: install — reference executor plus cargo-pinning wrapper. Nothing
    // below can succeed without this, and nothing above it runs until both
    // verification legs passed, so a failed install has written no config.
    let bin_dir = format!("{home}/{REMOTE_BIN_SUBDIR}");
    let exec_sh = format!("{bin_dir}/{REMOTE_EXECUTOR_NAME}");
    let wrapper = format!("{bin_dir}/{REMOTE_WRAPPER_NAME}");
    let install = || -> Result<(), String> {
        ssh_run(
            opts.ssh,
            target,
            &format!("mkdir -p {}", sh_quote(&bin_dir)),
            None,
        )?;
        ssh_run(
            opts.ssh,
            target,
            &format!("cat > {}", sh_quote(&exec_sh)),
            Some(E2E_EXECUTOR_SCRIPT.as_bytes()),
        )?;
        let (w, s) = (sh_quote(&wrapper), sh_quote(&exec_sh));
        let script = wrapper_script(target, &exec_sh, &cargo);
        ssh_run(
            opts.ssh,
            target,
            &format!("cat > {w} && chmod +x {w} {s} && test -x {w} && test -x {s}"),
            Some(script.as_bytes()),
        )?;
        Ok(())
    };
    if let Err(e) = install() {
        return leg_failure(
            "install",
            &format!(
                "{e} — the executor could not be written to {target}:{bin_dir}; \
                 check the directory is writable and the host has room"
            ),
        );
    }
    println!("[gantry init] {target}: executor installed at {wrapper}");

    // Leg: preset — the config write, now that the host earned it.
    let contents = preset_config_toml(target, &wrapper);
    match write_user_config(opts.config_path, &contents) {
        Ok(Some(backup)) => println!(
            "[gantry init] preset config written to {} (previous config backed up to {})",
            opts.config_path.display(),
            backup.display()
        ),
        Ok(None) => {
            println!(
                "[gantry init] preset config written to {}",
                opts.config_path.display()
            )
        }
        Err(e) => return leg_failure("config", &e),
    }

    // Leg: e2e — the finishing canary owns the verdict wording and the exit
    // code, exactly as `gantry doctor --e2e` prints them, so an init
    // transcript and a doctor transcript read identically.
    if opts.run_e2e {
        println!("[gantry init] running the doctor --e2e canary…");
        return match crate::doctor::run_e2e_canary(&opts.config, &E2eCanaryOptions::default()) {
            Ok(msg) => {
                println!("\nE2E test: {msg}");
                println!(
                    "[gantry init] {target} is ready — intercepted `cargo test` \
                     now offloads there (backend: command)"
                );
                0
            }
            Err(e) => {
                eprintln!("\nE2E test failed: {e}");
                1
            }
        };
    }

    println!(
        "[gantry init] {target} is ready — intercepted `cargo test` \
         now offloads there (backend: command)"
    );
    0
}

/// One leg's failure line: the leg named first, the fix in the same line
/// (plan §8's per-leg actionable message).
fn leg_failure_message(leg: &str, detail: &str) -> String {
    format!("gantry init: leg '{leg}' failed: {detail}")
}

/// One leg's failure: [`leg_failure_message`] on stderr. Always exit 1.
fn leg_failure(leg: &str, detail: &str) -> i32 {
    eprintln!("{}", leg_failure_message(leg, detail));
    1
}

/// The reach leg's message. ssh's own failures — bad host name, refused
/// connection, unauthorized key — all arrive as exit 255 and deserve the
/// connectivity checklist, not a toolchain hint.
fn reach_failure(target: &str, err: &str) -> String {
    if err.contains("exit 255") {
        format!(
            "{err} — cannot reach {target} over ssh; check the host name, \
             that sshd runs there, and that your key is authorized \
             (`ssh {target} true`)"
        )
    } else {
        format!("{err} — the target did not answer `$HOME`; re-run to retry")
    }
}

/// The cargo leg's message: the probe's own line — where it searched, or
/// which cargo it found that could not run — carries the diagnosis; add the
/// fix (install or repair the toolchain, rustup being the usual route).
fn cargo_failure(target: &str, err: &str) -> String {
    format!(
        "{e} — install a Rust toolchain on {t} (rustup is the usual route) \
         and re-run gantry init --ssh {t}",
        e = err,
        t = target
    )
}

/// Run `ssh <target> <remote_cmd>` and return trimmed stdout; `stdin`, when
/// given, is streamed to the remote command (the executor upload's `cat`).
/// Stdout and stderr are captured, never inherited. Any non-zero exit
/// carries the remote stderr so the failing leg's message is actionable.
fn ssh_run(
    ssh: &Path,
    target: &str,
    remote_cmd: &str,
    stdin: Option<&[u8]>,
) -> Result<String, String> {
    let mut command = Command::new(ssh);
    command.arg(target).arg(remote_cmd);
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    // The remote stdio must be piped, not inherited: `wait_with_output`
    // only collects what was piped, so an inherited stdout would hand
    // every probe an empty answer (and leak the remote output past the
    // leg-failure message) on an otherwise healthy host.
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", ssh.display()))?;
    if let Some(bytes) = stdin {
        // Write and close the pipe before waiting: a `cat` on the far end
        // blocks on EOF, and waiting first would deadlock on a full pipe.
        let mut pipe = child.stdin.take().expect("stdin piped above");
        if let Err(e) = pipe.write_all(bytes) {
            let _ = child.wait(); // reap; the failure below is the useful one
            return Err(format!("cannot stream the script to {target}: {e}"));
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("ssh {target}: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "ssh {target} failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The wrapper installed next to the executor: pins `GANTRY_EXEC_CARGO` to
/// the exact path the probe verified, so the non-interactive shell ssh hands
/// the command to needs no `~/.profile` to find the toolchain. Paths are
/// absolute (the reach leg resolved `$HOME`), so [`sh_quote`] round-trips
/// them byte-exact.
fn wrapper_script(target: &str, exec_sh: &str, cargo: &str) -> String {
    format!(
        "#!/usr/bin/env sh\n\
         # Generated by `gantry init --ssh {target}`: pins the reference\n\
         # executor's cargo to the toolchain the init probes verified.\n\
         GANTRY_EXEC_CARGO={} exec {} \"$@\"\n",
        sh_quote(cargo),
        sh_quote(exec_sh)
    )
}

/// The preset config text: backend `command`, templates running the
/// installed wrapper over ssh with the command backend's placeholders
/// (`{repo}`, `{rev}`, `{args_json}`, `{handle}`) intact for
/// [`crate::backend::command`] to substitute per run.
fn preset_config_toml(target: &str, wrapper: &str) -> String {
    format!(
        "# Generated by `gantry init --ssh {target}`.\n\
         # The executor installed at {wrapper} runs the remote contract\n\
         # (contrib/gantry-exec.sh) on that host; intercepted `cargo test`\n\
         # runs offload there through the command-template backend.\n\
         [remote]\n\
         backend = \"command\"\n\
         \n\
         [remote.command]\n\
         submit = [\"ssh\", \"{target}\", \"{wrapper}\", \"submit\", \"{{repo}}\", \"{{rev}}\", \"{{args_json}}\"]\n\
         logs = [\"ssh\", \"{target}\", \"{wrapper}\", \"logs\", \"{{handle}}\"]\n\
         wait = [\"ssh\", \"{target}\", \"{wrapper}\", \"wait\", \"{{handle}}\"]\n"
    )
}

/// Write the preset to the user config, backing up any existing file to
/// `<config>.init-backup` first — init re-runs are expected (re-onboarding
/// after a host change) and must be safe, but a previous config (local caps,
/// intercept narrowing) must never be silently destroyed. `Ok(None)` means
/// there was nothing to back up.
fn write_user_config(config_path: &Path, contents: &str) -> Result<Option<PathBuf>, String> {
    let backup = backup_path(config_path);
    if config_path.exists() {
        fs::copy(config_path, &backup)
            .map_err(|e| format!("cannot back up {}: {e}", config_path.display()))?;
    }
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    fs::write(config_path, contents)
        .map_err(|e| format!("cannot write {}: {e}", config_path.display()))?;
    Ok(if backup.exists() { Some(backup) } else { None })
}

/// `<config>.init-backup` — appended, not an extension swap, so
/// `config.toml` stays a TOML path all the way through.
fn backup_path(config_path: &Path) -> PathBuf {
    let mut name = config_path.as_os_str().to_os_string();
    name.push(".init-backup");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Backend;
    use tempfile::TempDir;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_accepts_an_ssh_target() {
        let parsed = parse(&argv(&["--ssh", "ops@buildbox"])).unwrap();
        assert_eq!(parsed.target, "ops@buildbox");

        // Any ssh destination word is fine — a config alias included.
        let parsed = parse(&argv(&["--ssh", "buildbox"])).unwrap();
        assert_eq!(parsed.target, "buildbox");
    }

    #[test]
    fn parse_rejects_every_usage_error_with_a_two() {
        // No arguments at all.
        assert_eq!(parse(&[]), Err(2));
        // --ssh without its value, and with an empty one.
        assert_eq!(parse(&argv(&["--ssh"])), Err(2));
        assert_eq!(parse(&argv(&["--ssh", ""])), Err(2));
        // A target with whitespace would inject across every line that
        // embeds it (preset argv, wrapper comment, messages).
        assert_eq!(parse(&argv(&["--ssh", "bad target"])), Err(2));
        assert_eq!(parse(&argv(&["--ssh", "host\nrm -rf /"])), Err(2));
        // Unknown flags.
        assert_eq!(parse(&argv(&["--bogus"])), Err(2));
        assert_eq!(parse(&argv(&["--ssh", "h", "extra"])), Err(2));
    }

    #[test]
    fn cli_help_and_usage_errors_exit_before_touching_the_world() {
        assert_eq!(cli(&argv(&["--help"])), 0);
        assert_eq!(cli(&argv(&["-h"])), 0);
        // Usage errors return before the config directory is resolved.
        assert_eq!(cli(&[]), 2);
        assert_eq!(cli(&argv(&["--ssh"])), 2);
    }

    #[test]
    fn preset_config_toml_parses_and_carries_the_host_preset() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.toml");
        let wrapper = "/home/ops/.local/bin/gantry-exec";
        fs::write(&path, preset_config_toml("ops@buildbox", wrapper)).unwrap();

        // The written file must parse through the real config loader — a
        // preset that cannot load is worse than no preset.
        let loaded = Config::load_layers(None, Some(&path), None).unwrap();
        assert!(
            loaded.warnings.is_empty(),
            "warnings: {:?}",
            loaded.warnings
        );
        assert_eq!(loaded.config.remote.backend, Backend::Command);

        let command = loaded.config.remote.command.expect("command table");
        fn argv_of(v: &[String]) -> Vec<&str> {
            v.iter().map(String::as_str).collect()
        }
        assert_eq!(
            argv_of(&command.submit),
            vec![
                "ssh",
                "ops@buildbox",
                wrapper,
                "submit",
                "{repo}",
                "{rev}",
                "{args_json}",
            ]
        );
        assert_eq!(
            argv_of(&command.logs),
            vec!["ssh", "ops@buildbox", wrapper, "logs", "{handle}"]
        );
        assert_eq!(
            argv_of(&command.wait),
            vec!["ssh", "ops@buildbox", wrapper, "wait", "{handle}"]
        );
        // No deadline key: the preset inherits the global budget.
        assert_eq!(command.deadline_minutes, None);
    }

    #[test]
    fn wrapper_script_pins_the_verified_cargo_quoted() {
        let script = wrapper_script(
            "ops@buildbox",
            "/home/ops/.local/bin/gantry-exec.sh",
            "/home/ops/.cargo/bin/cargo",
        );
        assert!(script.starts_with("#!/usr/bin/env sh"), "{script}");
        assert!(
            script.contains("GANTRY_EXEC_CARGO='/home/ops/.cargo/bin/cargo'"),
            "the probed path must be pinned, single-quoted: {script}"
        );
        assert!(
            script.contains("exec '/home/ops/.local/bin/gantry-exec.sh' \"$@\""),
            "the wrapper must exec the reference script: {script}"
        );
    }

    #[test]
    fn write_user_config_backs_up_and_replaces() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("gantry/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[local]\ncpu_quota_pct = 150\n").unwrap();

        let backup = write_user_config(&path, "preset").unwrap().expect("backup");
        assert!(backup
            .as_os_str()
            .to_string_lossy()
            .ends_with(".init-backup"));
        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            "[local]\ncpu_quota_pct = 150\n",
            "the previous config must survive byte-exact"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "preset");

        // A fresh write (no prior config) has nothing to back up.
        let fresh = temp.path().join("fresh/config.toml");
        assert!(write_user_config(&fresh, "x").unwrap().is_none());
        assert_eq!(fs::read_to_string(&fresh).unwrap(), "x");
    }

    #[test]
    fn leg_failure_message_names_the_leg_first() {
        // The whole contract of the per-leg line: the leg is the first word
        // after the prefix, the detail rides unchanged.
        assert_eq!(
            leg_failure_message("reach", "ssh ops@buildbox failed (exit 255): refused"),
            "gantry init: leg 'reach' failed: ssh ops@buildbox failed (exit 255): refused"
        );
        // And the exit is 1 no matter which leg failed.
        assert_eq!(leg_failure("reach", "x"), 1);
        assert_eq!(leg_failure("cargo", "x"), 1);
    }

    #[test]
    fn reach_failure_routes_ssh_transport_failures_to_the_connectivity_checklist() {
        // ssh's own exit 255: the message must read as connectivity — host
        // name, sshd, key — and carry the one-liner the operator can paste.
        let msg = reach_failure(
            "ops@buildbox",
            "ssh ops@buildbox failed (exit 255): refused",
        );
        assert!(msg.contains("cannot reach ops@buildbox over ssh"), "{msg}");
        assert!(msg.contains("that sshd runs there"), "{msg}");
        assert!(msg.contains("`ssh ops@buildbox true`"), "{msg}");
        assert!(!msg.contains("toolchain"), "{msg}");

        // Any other reach failure: the host answered but misbehaved — a
        // retry, not the connectivity lecture.
        let msg = reach_failure("ops@buildbox", "ssh ops@buildbox failed (exit 1): kaboom");
        assert!(!msg.contains("cannot reach"), "{msg}");
        assert!(msg.contains("re-run to retry"), "{msg}");
    }

    #[test]
    fn cargo_failure_names_the_target_and_the_rustup_remedy() {
        // Missing cargo: the probe's own where-it-looked line survives, and
        // the fix names the target plus the re-run command.
        let msg = cargo_failure(
            "ops@buildbox",
            "ssh ops@buildbox failed (exit 127): \
             cargo not found (looked in PATH and $HOME/.cargo/bin)",
        );
        assert!(msg.contains("looked in PATH and $HOME/.cargo/bin"), "{msg}");
        assert!(
            msg.contains("install a Rust toolchain on ops@buildbox"),
            "{msg}"
        );
        assert!(msg.contains("rustup"), "{msg}");
        assert!(
            msg.contains("re-run gantry init --ssh ops@buildbox"),
            "{msg}"
        );

        // Located but unusable cargo gets the same remedy — the toolchain
        // there needs (re)installing either way.
        let broken = cargo_failure(
            "ops@buildbox",
            "ssh ops@buildbox failed (exit 126): \
             /home/ops/.cargo/bin/cargo found but 'cargo --version' failed",
        );
        assert!(
            broken.contains("install a Rust toolchain on ops@buildbox"),
            "{broken}"
        );
        assert!(broken.contains("cargo --version' failed"), "{broken}");

        // Composed the way `run` emits it, the line leads with the leg —
        // the operator reads "leg 'cargo'" before any probe detail.
        let composed = leg_failure_message(
            "cargo",
            &cargo_failure(
                "ops@buildbox",
                "ssh ops@buildbox failed (exit 127): cargo not found",
            ),
        );
        assert!(
            composed.starts_with("gantry init: leg 'cargo' failed: "),
            "{composed}"
        );
    }
}
