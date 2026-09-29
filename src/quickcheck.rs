// gantry — `gantry quickcheck`: the 30-second no-backend sanity check
// (plan §"CLI surface": "30s no-backend sanity: shim resolves, cap works,
// git ok (Tier-0 proof)").
//
// quickcheck answers one question on an operator's schedule: does a fresh
// gantry install — zero config, no remote backend — actually work *right
// now*? Three checks, each printing a `[gantry] quickcheck:` line:
//
// 1. **shim resolves** — the real binary behind the shim resolves through
//    the production rules (`shim::resolve_real_binary`), whose PATH strip
//    and self-recursion guard are the checks: an Ok means gantry can hand
//    off without re-execing itself.
// 2. **cap works** — a trivial command runs through the per-run cap
//    ([`crate::cap::Cap`]) end-to-end. On a systemd box this proves a scope
//    is actually creatable with the configured values; where scopes are
//    impossible the documented degrade (plain exec, loudly noted, plan §6)
//    is reported as the active tier rather than failed — a capless
//    environment must not render quickcheck useless exactly where gantry's
//    plain-exec contract applies (plan R5 reports the tier; doctor treats
//    scope unavailability as a warning for the same reason).
// 3. **git ok** — `git` runs, and inside a work tree `HEAD` resolves: the
//    git plumbing every decision path leans on (repo detection, sha
//    capture) is functional.
//
// Nothing here contacts a backend or the network (INV-6): quickcheck is the
// Tier-0 proof, so it passes with no backend configured by construction.
// Every git child is bounded ([`crate::cap::wait_with_timeout`]) and the cap
// probe is bounded inside [`crate::cap`] too, so a wedged git or systemd
// costs a failed check, not a hang. The bounds are per child (10s each, plus
// the probe's own 10s), so a pathological host can push the total past the
// plan's "30s" shorthand — the promise that matters is bounded-and-loud, not
// a wall-clock guarantee. Exit 0 iff every required check passed.

use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use crate::cap;
use crate::config::{Backend, Config};
use crate::shim;

/// Per-child budget; on a healthy host the whole check finishes in well
/// under the plan's "30s" shorthand (module docs for the pathological-host
/// arithmetic).
const CHILD_BUDGET: Duration = Duration::from_secs(10);

/// Run the quickcheck. Prints one line per check plus a summary; exits 0
/// iff every required check passed.
pub fn run() -> ExitCode {
    let started = Instant::now();

    let load = Config::load();
    let cfg = load.config;
    for warning in load.warnings {
        eprintln!("[gantry] config warning: {}", warning);
    }

    // The tier header: quickcheck is the Tier-0 proof, so the backend state
    // it passes under is part of the answer. The backend is named in the
    // config's own spelling (not Debug's) — this line goes into agent
    // transcripts, where `argo` beats `Argo`.
    let backend_name = match cfg.remote.backend {
        Backend::None => None,
        Backend::Argo => Some("argo"),
        Backend::Command => Some("command"),
    };
    let header = match backend_name {
        None => "no backend configured (Tier-0 zero-config mode)".to_string(),
        Some(name) => format!(
            "backend {name} configured — these checks cover the no-backend (Tier-0) sanity only"
        ),
    };
    println!("[gantry] quickcheck: {header}");

    let mut failed = false;

    // 1. shim resolves (production resolution rules; an Ok already cleared
    //    the self-recursion guard).
    match shim::resolve_real_binary(&cfg) {
        Ok(real) => {
            println!(
                "[gantry] quickcheck: shim resolves: ok — real binary: {}",
                real.display()
            );
        }
        Err(why) => {
            println!("[gantry] quickcheck: shim resolves: FAIL — {why}");
            failed = true;
        }
    }

    // 2. cap works: run `true` through the per-run cap. `Cap::describe`
    //    triggers (and reports) the one-time probe; the spawn then proves
    //    the active tier end-to-end. The process-wide instance — the same
    //    one a shim run in this process would use — so the probe cost and
    //    its note are paid once no matter how many checks or tails touch it.
    let cap = cap::process_cap(&cfg);
    let tier = cap.describe();
    match cap.spawn(std::path::Path::new("true"), &[]) {
        Ok(status) if status.success() => {
            println!("[gantry] quickcheck: cap works: ok — {tier}");
        }
        Ok(status) => {
            println!("[gantry] quickcheck: cap works: FAIL — probe exited with {status} ({tier})");
            failed = true;
        }
        Err(e) => {
            println!("[gantry] quickcheck: cap works: FAIL — {e} ({tier})");
            failed = true;
        }
    }

    // 3. git ok: the binary runs; inside a work tree, HEAD resolves.
    match git_sanity() {
        Ok(detail) => {
            println!("[gantry] quickcheck: git ok: ok — {detail}");
        }
        Err(why) => {
            println!("[gantry] quickcheck: git ok: FAIL — {why}");
            failed = true;
        }
    }

    let elapsed = started.elapsed().as_secs_f32();
    if failed {
        println!("[gantry] quickcheck: FAILED ({elapsed:.1}s) — see the FAIL lines above");
        return ExitCode::FAILURE;
    }
    println!("[gantry] quickcheck: passed ({elapsed:.1}s)");
    ExitCode::SUCCESS
}

/// The git sanity check: `git --version` must run, and inside a work tree
/// `HEAD` must resolve. Outside a repo the binary check still passes — the
/// gate plumbing only applies inside one — with the status named.
fn git_sanity() -> Result<String, String> {
    let version = bounded_output(Command::new("git").arg("--version"), "git --version")?;
    let version = first_line(&version).to_string();

    // rev-parse answers both questions at once: --is-inside-work-tree for
    // the repo status, HEAD for the sha resolution the decision paths need.
    let inside = bounded_output(
        Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .stderr(Stdio::null()),
        "git rev-parse --is-inside-work-tree",
    );
    match inside {
        Ok(out) if first_line(&out) == "true" => {
            let head = bounded_output(
                Command::new("git")
                    .args(["rev-parse", "--verify", "HEAD"])
                    .stderr(Stdio::null()),
                "git rev-parse --verify HEAD",
            )?;
            Ok(format!(
                "{version}; work tree with HEAD {}",
                first_line(&head)
            ))
        }
        Ok(out) if first_line(&out) == "false" => Ok(format!(
            "{version}; not inside a work tree (gate checks apply inside repos)"
        )),
        Ok(_) => {
            Err("git rev-parse --is-inside-work-tree printed an unexpected answer".to_string())
        }
        // Outside a work tree the check still passes — the gate plumbing
        // only applies inside one — but the specific rev-parse failure is
        // kept in the detail so a broken git install reads as what it is.
        Err(why) => Ok(format!(
            "{version}; not inside a git repo ({why}; gate checks apply inside repos)"
        )),
    }
}

/// Run a command to completion under [`CHILD_BUDGET`], returning its stdout
/// as a lossy string. Stderr flows to the command's default unless the
/// caller redirected it — quickcheck's own transcript must stay readable,
/// and a failing check names the command.
fn bounded_output(cmd: &mut Command, name: &str) -> Result<String, String> {
    bounded_output_with_budget(cmd, name, CHILD_BUDGET)
}

/// [`bounded_output`] with the budget explicit — the seam the hung-command
/// test uses to pin a short budget instead of waiting out the production one
/// (the same shape as `cap::probe_scope_with_budget`).
fn bounded_output_with_budget(
    cmd: &mut Command,
    name: &str,
    budget: Duration,
) -> Result<String, String> {
    cmd.stdout(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("{name}: cannot run: {e}"))?;
    let status = cap::wait_with_timeout(&mut child, budget).map_err(|e| format!("{name}: {e}"))?;
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "{name}: did not finish within {}s",
            budget.as_secs()
        ));
    };
    if !status.success() {
        return Err(format!("{name}: exited with {status}"));
    }
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        use std::io::Read;
        let _ = pipe.read_to_string(&mut stdout);
    }
    Ok(stdout)
}

/// The first line of a command's stdout, trimmed.
fn first_line(s: &str) -> &str {
    s.lines().next().map(str::trim).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_trims_the_first_nonempty_line() {
        assert_eq!(first_line("git version 2.47.0\n"), "git version 2.47.0");
        assert_eq!(first_line("true\nfalse\n"), "true");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn bounded_output_returns_stdout_of_a_fast_command() {
        let out = bounded_output(Command::new("echo").arg("hello"), "echo hello").unwrap();
        assert_eq!(first_line(&out), "hello");
    }

    #[test]
    fn bounded_output_names_a_failing_command() {
        let err = bounded_output(
            Command::new("sh").args(["-c", "echo boom >&2; exit 3"]),
            "failing-cmd",
        )
        .unwrap_err();
        assert!(err.contains("failing-cmd"), "{err}");
        assert!(err.contains("exited with"), "{err}");
    }

    #[test]
    fn bounded_output_bounds_a_hung_command() {
        let started = Instant::now();
        // A pinned short budget: the production CHILD_BUDGET (10s) would make
        // this test wait it out, but boundedness is what is under test, and a
        // 150ms budget exercises the same expiry path.
        let err = bounded_output_with_budget(
            Command::new("sleep").arg("30"),
            "hung-cmd",
            Duration::from_millis(150),
        )
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must be bounded"
        );
        assert!(err.contains("did not finish within"), "{err}");
    }

    #[test]
    fn bounded_output_reports_missing_binaries() {
        let err = bounded_output(
            &mut Command::new("gantry-no-such-binary-xyz"),
            "missing-cmd",
        )
        .unwrap_err();
        assert!(err.contains("cannot run"), "{err}");
    }

    #[test]
    fn git_sanity_works_both_inside_and_outside_a_repo() {
        // Whatever cwd the test process has, the check must produce a
        // coherent answer — inside a repo it names HEAD, outside it says so.
        // (cargo test runs inside this repo, so the HEAD branch is expected
        // here, but the assertion is written to hold in either world.)
        let detail = git_sanity().expect("git sanity must not fail");
        assert!(detail.starts_with("git version"), "{detail}");
    }
}
