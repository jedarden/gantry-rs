# `gantry init --ssh` onboarding — the leg ladder, exit contract, and what a green init actually proves

Operator-facing notes for the SSH-first onboarding flow (plan §8 "doctor /
installer": install to first remote run in ~10 minutes, no Kubernetes
required). **`src/cli/init.rs` is the single definition site**; the finishing
canary lives in `src/doctor.rs` (`run_e2e_canary`) and the reference executor
both install and canary share is `contrib/gantry-exec.sh`, embedded at
`src/doctor.rs:498` (`E2E_EXECUTOR_SCRIPT`) — one source of truth for the
remote contract. The whole ladder is driven end-to-end by
`tests/init_ssh_integration.rs` through a loopback ssh stand-in, so the
integration suite exercises the same legs this note describes without
touching a real host.

## The six-leg ladder

`gantry init --ssh <target>` runs a one-way ladder; every leg that can fail
names itself in the message so the operator lands on the fix. The module
header (`src/cli/init.rs:12`–`45`) is the in-code version of this table.

| # | Leg   | Proves | Reference |
|---|-------|--------|-----------|
| 1 | reach   | The host answers ssh, and its `$HOME` pins every later path as absolute. | `src/cli/init.rs:226`–`240` |
| 2 | git     | `git --version` *runs* on the host — the executor fetches the epoch ref with git, and a git that cannot run is no better than an absent one. | `src/cli/init.rs:242`–`255` |
| 3 | cargo   | A cargo is located (PATH, falling back to `$HOME/.cargo/bin/cargo`) **and** runs (`cargo --version`); the probed absolute path is pinned into the installed wrapper. | `src/cli/init.rs:257`–`264`, probe at `:89`–`95` |
| 4 | install | The reference executor (`gantry-exec.sh`) plus a thin wrapper (`gantry-exec`) pinning `GANTRY_EXEC_CARGO` are written into `<home>/.local/bin` and are executable (`test -x` both). | `src/cli/init.rs:266`–`304` |
| 5 | preset  | The user config is written with `backend = "command"` and submit/logs/wait ssh templates invoking the installed wrapper. A pre-existing config is backed up first (see [recovery](#recovery-from-a-red-canary)). | `src/cli/init.rs:306`–`321`, `preset_config_toml` at `:470`–`484` |
| 6 | e2e     | The `doctor --e2e` canary round-trips a known-good commit through the real pipeline. It owns the final verdict wording and the exit code. | `src/cli/init.rs:323`–`352`, `src/doctor.rs:571` |

What each leg's proof is *for*:

- **reach** — `printf '%s\n' "$HOME"` (`src/cli/init.rs:227`). Absolute
  matters twice: the wrapper's `exec` line and the preset's argv must
  survive `sh_quote` verbatim (no tilde semantics), and every downstream
  path — bin dir, executor, pinned cargo — is built from the same resolved
  home. An ssh transport failure (exit 255: bad host name, refused
  connection, unauthorized key) gets the connectivity checklist, including
  the pasteable `ssh <target> true` one-liner; any other answer gets a
  retry hint (`reach_failure`, `src/cli/init.rs:376`–`386`).
- **git** — the version *invocation*, not `command -v`: a git that is
  present but broken must fail onboarding, not the first run.
- **cargo** — the non-interactive shell ssh hands commands to frequently
  misses the rustup install (no `~/.profile` sourced), and a located cargo
  can still be half-broken. The probe runs `cargo --version` on whichever
  cargo it found (`CARGO_PROBE`, `src/cli/init.rs:89`–`95`) and prints the
  absolute path; `wrapper_script` (`src/cli/init.rs:455`–`464`) pins that
  exact string as `GANTRY_EXEC_CARGO`, so the executor's own `cargo test`
  inherits the same answer the probe verified — no `~/.profile` needed at
  run time either.
- **install** — `mkdir -p`, then `cat >` the embedded executor and the
  wrapper, then `chmod +x … && test -x` both (`src/cli/init.rs:272`–`294`).
  The preset invokes the *wrapper*, never the script directly — the pin is
  the point.
- **preset** — the generated TOML carries the command backend's
  placeholders (`{repo}`, `{rev}`, `{args_json}`, `{handle}`) intact for
  `crate::backend::command` to substitute per run, and no deadline key —
  the preset inherits the global budget (`src/cli/init.rs:470`–`484`). It
  is parsed through the real config loader in the unit tests
  (`src/cli/init.rs:560`–`602`).
- **e2e** — see the next section for what it does and does not prove.

Ordering guarantee: nothing is written anywhere until the host has passed
the verification legs. The config write is leg 5; legs 1–4 only touch the
target host, and the local config is untouched before they pass (usage text
at `src/cli/init.rs:168`–`169`). A failed install therefore leaves no
config behind (`src/cli/init.rs:266`–`268`).

Naming nuance: the ladder leg is called *preset*, but a failed config write
reports `leg 'config'` — the failure site is
`leg_failure("config", …)` (`src/cli/init.rs:320`).

## Exit-code contract

The management-CLI convention shared with run/why/status
(`src/cli/init.rs:55`–`57`; dispatch at `src/main.rs:302`):

| Exit | Meaning | Source |
|------|---------|--------|
| `0`  | Host onboarded and canary green. | `src/cli/init.rs:345` (canary path), `:358` (`run_e2e = false`, tests only) |
| `1`  | A named leg failed; the fix is in the message. | `leg_failure`, `src/cli/init.rs:363`–`371`; canary path `:347`–`350` |
| `2`  | Usage error — missing `--ssh`/value, a target with whitespace or control characters, or an unknown flag. Nothing is executed. | `parse`, `src/cli/init.rs:110`–`151`; `is_valid_target` at `:156`–`158` |

`--help`/`-h` exits `0` before the config directory is resolved
(`src/cli/init.rs:197`–`200`).

Exit-1 messages always lead with the leg (`gantry init: leg 'reach'
failed: …` — `leg_failure_message`, `src/cli/init.rs:363`–`365`), and each
leg's detail carries its own remedy: reach → connectivity checklist; git →
`apt/dnf install git` plus the re-run command; cargo → rustup plus the
re-run command; install → writability/space on `<target>:<bin_dir>`. The
target is a single whitespace-free word, so it is safe to embed verbatim in
every one of those lines (`src/cli/init.rs:153`–`158`).

Leg 6 failures come from the canary itself and name a *canary* leg —
`fixture`, `push`, `submit`, `wait`, or `verdict` — in the form
`E2E test failed: e2e canary failed at leg '<leg>': …`
(`src/cli/init.rs:347`–`350`; `e2e_failure`, `src/doctor.rs:804`–`806`).
Those remedies: `fixture` → disk space or temp-dir permissions;
`push` → git itself is broken on this box; `submit` → the command-template
presets are broken; `wait` → deadline or wait-command failure;
`verdict` → the known-good suite was misclassified, with a tail of the
executor's captured log attached (`e2e_failure_with_logs`,
`src/doctor.rs:811`).

## What a green init proves: loopback vs host

The design nuance worth internalizing: **the e2e canary always rides the
loopback backend, regardless of the configured backend**
(`run_e2e_canary`, `src/doctor.rs:563`–`591`). Init hands the canary the
config it loaded before writing the preset — legitimate, because the
canary reads none of the parts that would send work off-box: it builds its
own throwaway fixture — a bare remote under the system temp
dir, a known-good commit, and the *same embedded reference executor*
`contrib/gantry-exec.sh` the install leg placed on the host — and drives
push → clone → `cargo test` → verdict through the real `RefPusher` and
command-template backend machinery entirely on this box
(`src/doctor.rs:543`–`559`, round trip at `:724`–`800`). The config still
steers what it truthfully can: the fixture remote is added under the
configured `ci_remote` name, the epoch ref follows the configured
`push_mode`, and the wait deadline is the configured command deadline
(`src/doctor.rs:567`–`570`) — none of which the preset init writes touches
(`InitOptions::config` doc, `src/cli/init.rs:183`–`192`).

So the two halves of the guarantee are deliberately split:

- **Host-facing** (does that box qualify?): legs 1–4. The target answered,
  its git runs, its cargo both exists and runs, and the executor + wrapper
  landed there executable. That is the whole host claim.
- **Box-facing** (does the pipeline mechanics work here?): leg 6. A green
  canary proves the push/clone/verdict machinery on *this* machine — the
  same mechanics the preset then drives over ssh — and nothing about the
  target beyond what legs 1–4 already established.

Init says so in its own transcript on success: the scope note after the
canary verdict reads "green proves the pipeline mechanics on this box, not
\<target\> — the host itself was verified by the reach, git, and cargo legs
above" (`src/cli/init.rs:335`–`340`). Practical consequence: the first
actual end-to-end exercise of the wrapper *over ssh* is the first
intercepted `cargo test` that offloads to the new backend. The canary never
invokes the installed wrapper — a loopback run would prove nothing extra
about the host, and an argo-configured install would otherwise turn a
doctor check into a cluster submission (`src/doctor.rs:563`–`566`).

## Recovery from a red canary

Order of operations matters: **the config write (leg 5) precedes the
canary (leg 6)** (`src/cli/init.rs:306`–`321` then `:330`–`352`). A red
canary therefore exits 1 with the init preset already installed on disk
and the executor already on the host — the failure is on this box's
pipeline side, and every canary leg (`fixture`/`push`/`submit`/`wait`/
`verdict`) runs locally.

Before overwriting, `write_user_config` copies any pre-existing config
byte-exact to `<config>.init-backup` (appended, not an extension swap, so
the path stays a TOML path throughout;
`src/cli/init.rs:486`–`512`, test at `:623`–`645`). Nothing is silently
destroyed on re-init.

So recovery from exit 1 at leg 6 is:

1. Read the named canary leg's fix from the message (disk space, temp-dir
   permissions, broken git, toolchain misclassification — see the exit
   table above).
2. Fix the box.
3. Re-run **`gantry doctor --e2e`** — not a re-init. The host has not
   changed, the executor is installed, and the preset is live; the canary
   is the only leg that failed, and `doctor --e2e` is exactly that leg,
   standalone (`src/main.rs:226`–`236`).

A re-run of `gantry init --ssh <target>` is *safe* (the old config is
backed up again, never destroyed) but wasteful: it repeats the whole
ladder to re-prove legs the host already passed. To back out of an init
entirely, restore `<config>.init-backup` over
`~/.config/gantry/config.toml` by hand; `gantry uninstall` reverses this
box's gantry install (binary, shims, state, config — `src/uninstall.rs`)
but does not ssh to the target, so the host-side
`<home>/.local/bin/gantry-exec{,.sh}` pair is removed by hand if the
onboarding is being abandoned.
