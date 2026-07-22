# Prior Art Survey

Surveyed 2026-07-21. The gap gantry fills: **no existing tool transparently intercepts the vanilla command and applies policy about where it runs.** Everything below either requires the caller to invoke something different, or solves an adjacent problem (caching, hermetic builds, safety).

## dcg — Destructive Command Guard (packaging model)

[Dicklesworthstone/destructive_command_guard](https://github.com/Dicklesworthstone/destructive_command_guard)

Blocks dangerous git/shell commands issued by AI agents. Opposite polarity to gantry — dcg intercepts to **block**, gantry intercepts to **relocate** — but the closest philosophical relative: the agent doesn't know it's there, and the tool is the policy layer between agent intent and execution.

What gantry borrows:
- Single static binary + curl-pipe `install.sh` that auto-detects the platform and configures everything.
- A `doctor`-style verification step so a broken install is diagnosed, not discovered in production.
- Clear block/redirect messages that explain *why* and *what happens instead* (dcg explains blocked commands; gantry explains offload/fallback decisions in `[gantry]` stderr lines).
- Docs that lead with the agent-fleet use case.

What gantry deliberately does not borrow:
- The per-agent integration matrix (Claude Code, Codex, Gemini CLI, Copilot, Cursor, OpenCode…). dcg must hook the agent because it needs to see every shell command; gantry only needs to own a few binary names, and PATH shimming covers every caller at once. See design-decisions DD-1.

## sgeisler/cargo-remote (name-collides with the in-house script)

[sgeisler/cargo-remote](https://github.com/sgeisler/cargo-remote) — `cargo remote test`: rsyncs the project to a build server over SSH, runs cargo there, optionally copies artifacts back.

- **Explicit invocation** — the caller must type `cargo remote …`; agents would need retraining/prompting. This is the key difference.
- Syncs the *working tree* (rsync), so dirty state works — but reproducibility is whatever was on disk, and there's no capped local fallback.
- Requires a persistent, toolchain-matched build server; no queue/CI semantics, no log-after-the-fact.
- Its existence takes the `cargo-remote` name on crates.io — one reason the extraction is renamed.

## sccache / sccache-dist (Mozilla)

Compiler cache with optional distributed compilation. Solves *compilation cost*, not *where the command runs*: tests still execute locally, link steps still eat local RAM, a fork-bombing proptest still kills the box. **Complementary** — the in-house remote executor already runs sccache against S3 (Garage) on the cluster side, which is what makes cold-clone remote builds tolerable. Gantry treats warm caching as a property of the backend, not of gantry.

## Bazel Remote Build Execution (and BuildBarn, BuildFarm, NativeLink)

The maximal version: hermetic action graph, content-addressed inputs, remote execution API. Requires adopting Bazel (or an RBE-speaking build tool) — a total build-system migration, unrealistic for ordinary cargo projects and hostile to agents that just know `cargo test`. Gantry's unit of offload is deliberately coarse ("commit + command line") to avoid this entire complexity class.

## Earthly / Dagger

Containerized CI-as-code, run locally or remote. Again explicit invocation (`earthly +test`) and a new build definition layer. Earthly's company also announced winding down commercial development (2025), a caution about the space. Not interception, not transparent.

## Nix remote builders

Genuinely transparent offload — but only for Nix derivations. If the project is flake-ified with `checkPhase`, Nix will farm it out; nobody's `cargo test` muscle memory or agent prompt survives that migration. The transparency precedent is encouraging; the adoption cost is disqualifying.

## rustup / volta / asdf / mise shims (mechanism precedent)

The shim-directory-plus-argv0-dispatch pattern is thoroughly proven at ecosystem scale: a directory of tool-named symlinks/binaries early in PATH, dispatching on `argv[0]`, resolving the "real" tool while avoiding self-recursion. rustup's `cargo` shim specifically demonstrates that shimming `cargo` itself is safe and universally compatible (gantry's shim will typically sit in front of rustup's shim and exec it as the "real" cargo — a two-layer chain that already works today with the bash predecessor).

Known sharp edges to inherit solutions for: PATH ordering drift (doctor check), recursion (strip own dir before resolution), scripts that hardcode `~/.cargo/bin/cargo` (documented limitation — they bypass gantry, which is fine).

## cargo's own extension points (why a shim is required at all)

- Custom subcommands (`cargo-foo`) **cannot override built-ins** like `test`.
- `[alias]` in `.cargo/config.toml` cannot shadow built-in subcommands either.
- `RUSTC_WRAPPER` hooks compilation, not test execution; `target.runner` hooks test *binary* invocation but runs after the local build already happened — too late, the expensive part is done.
- Therefore: transparent interception of `cargo test` is only achievable ahead of the binary, i.e. PATH. This is load-bearing for the whole design.

## cargo-nextest (future-relevant)

Next-gen test runner with clean machine-readable output and **test partitioning** (`--partition count:N/M`) — the natural primitive if gantry ever shards a suite across multiple remote pods. Excluded from v1 interception (the in-house builder image doesn't guarantee nextest), but the backend contract shouldn't preclude it.

## Argo Workflows as a generic remote executor

The in-house backend. Relevant properties for the client design: WorkflowTemplate + parameters as the submission contract; `status.phase` as the authoritative verdict; pod log streaming is best-effort only (`podGC: OnPodCompletion` deletes pods at completion — the predecessor's log tail is lossy by design and the verdict must never depend on it); workflow TTLs bound how long results are queryable (in-house: 30 min success / 2 h failure). All of these become explicit in the `argo` backend rather than incidental.

## Summary table

| Tool | Transparent to caller | Remote test execution | Safe local fallback | Adoption cost |
|---|---|---|---|---|
| **gantry (planned)** | ✅ PATH shim | ✅ pluggable backend | ✅ cgroup-capped | drop-in |
| dcg | ✅ (hooks) | — (safety guard) | n/a | installer |
| sgeisler/cargo-remote | ❌ `cargo remote` | ✅ SSH+rsync | ❌ | build server |
| sccache(-dist) | ✅ | ❌ compile only | n/a | low |
| Bazel RBE | ❌ | ✅ | ❌ | total migration |
| Earthly/Dagger | ❌ | ✅ | ❌ | new build layer |
| Nix remote builders | ✅ within Nix | ✅ | ❌ | Nix migration |
