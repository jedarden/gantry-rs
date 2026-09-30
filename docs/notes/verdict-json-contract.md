# verdict.json schema contract — version 1

The remote-side document that refines a run's classification within its
terminal workflow phase. **`src/verdict.rs` is the single definition site**
(`VerdictJson`, `FailureClass`, `CONTRACT_VERSION`); the reference producer is
`contrib/argo/gantry-verify-workflowtemplate.yml` (the `verdict` output
parameter), the consumer is `src/backend/argo.rs` (`wait()`). This note is the
human-readable contract those three must agree on; it closes out bf-2jnj.

Plan anchors: §Component 5 (`argo` bullet, "structured verdict.json output
parameter"), §"Verdict semantics", §"Versioning & compatibility" ("Remote
contract" — the `contract_version` handshake), §"Data format versioning"
("absent file = exit-code-only contract").

## The v1 document

A JSON object. Required fields: `schema_version`, `phase`, `exit_code`.
Everything else is optional with a clean default. Serialized example
(`failure_class` and `contract_version` are skipped when absent):

```json
{
  "schema_version": 1,
  "phase": "Failed",
  "exit_code": 1,
  "oom": false,
  "deadline_exceeded": false,
  "failure_class": "test-failure",
  "contract_version": "1"
}
```

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `schema_version` | integer | yes | — | Document schema version. This build parses exactly `1`; any other value is a loud parse error (never silently read as v1). |
| `phase` | string | yes | — | Workflow phase from `status.phase` (`Succeeded`, `Failed`, `Error`, …). Authoritative for workflow-level outcome: `"Error"` means no suite ever ran. |
| `exit_code` | integer | yes | — | **The client's ladder, never cargo's raw code:** `0` pass, `1` a real failure whatever its class, `≥2` infra. The raw cargo code (e.g. 101) stays in the run log and the container's own exit status; putting it here would make the client read a compile error as InfraFailure and re-run locally (INV-7). |
| `oom` | bool | no | `false` | The container was OOMKilled. InfraFailure signal: an infrastructure cap firing says nothing about the code. |
| `deadline_exceeded` | bool | no | `false` | The workflow exceeded its deadline. InfraFailure signal; set from outside the container (the pre-written default covers it in-process). |
| `failure_class` | string, optional | no | absent | Detailed classification derived from cargo's `--message-format json` stream: `compile-error` \| `test-failure` \| `doctest` \| `harness-panic` \| `gate-failure` (kebab-case). Lets agents branch without parsing logs. Absent or unrecognized ⇒ exit-code-only interpretation of the rest of the document. |
| `contract_version` | string, optional | no | absent | The verbatim echo of the `contract-version` parameter the client sent. See the handshake below. |

### Precedence ladder (`VerdictJson::to_verdict`)

1. **Contract drift** — a present `contract_version` echo this client does not
   speak ⇒ `InfraFailure`, whatever the other fields claim. A document whose
   own meaning is in question cannot vouch for anything.
2. **Infra signals** — `oom`, `deadline_exceeded`, or `phase == "Error"` ⇒
   `InfraFailure` (OOM outranks a later `failure_class` attribution; a capped
   run has no test result to attribute).
3. **Gate attribution** — `failure_class: "gate-failure"` ⇒ `GateFailure` even
   with a failing `exit_code` (the canonical shape: tests passed, gate failed,
   container exits 3 so the workflow reads `Failed` — agents must see red).
4. **Exit-code ladder** — `0` ⇒ `Pass`, `1` ⇒ `TestFailure`, anything else ⇒
   `InfraFailure`.

## Backward compatibility

The parser's failure posture is recorded as **DD-10**
(`design-decisions.md`): strict on structure and `schema_version`, lenient on
`failure_class` values, unknown fields ignored — the rule set below is the
wire-level restatement of that decision.

- **Unknown fields are ignored.** serde's default behavior (no
  `deny_unknown_fields`): a v2+ producer may add fields anywhere in the
  document; this consumer parses the fields it knows and the verdict is
  unchanged. Pinned by
  `verdict_json_unknown_fields_ignored` and the
  `property_unknown_fields_never_change_parse_or_verdict` sweep (every
  documented field combination × every unknown-field value shape).
- **Unknown `failure_class` strings read as absent**, not as a parse error —
  a class coined by a newer producer costs the document only its class, never
  its `oom`/`deadline_exceeded`/`exit_code` signals (`unknown_failure_class_keeps_infra_signals`).
  A `failure_class` that is not a JSON string at all is a malformed document,
  not an unknown class: serde rejects it and the caller degrades
  (`invalid_failure_class_value_is_rejected_by_serde`).
- **Unknown `schema_version` is NOT tolerated silently.** Additive field
  evolution is free; a *new schema version* claims a contract this parser does
  not know, so `VerdictJson::parse` returns `Err` naming the version and the
  caller degrades (below) rather than guessing
  (`property_unsupported_schema_versions_are_rejected`).
- **Adding optional fields is free; removing or retyping a required field is a
  major bump** — same rule as the published client-side schemas
  (`docs/schemas/*-v1.json`), and the reason the version field exists.

### The `contract_version` handshake

The client sends `contract_version` as a Workflow parameter; the template
echoes it back verbatim.

- **Matching echo** — handshake confirmed, ladder decides normally.
- **Absent echo** — *not* drift: the field arrived by additive evolution, so a
  schema-1 producer predating the handshake omits it and this client knows the
  schema-1 document completely (`absent_echo_is_a_pre_handshake_producer_not_drift`).
- **Different echo** (or a present echo in a shape this client cannot read —
  it reads as `""`, which never matches) — contract drift ⇒ `InfraFailure`
  with an explicit `[gantry] contract drift:` message, never a misread verdict
  (`property_mismatching_echo_is_infra_failure_across_the_matrix`,
  `garbled_echo_shape_is_drift_not_parse_error`).

## Degradation: missing or invalid file ⇒ exit-code-only mode

The explicit rule: **a run with no usable verdict.json is classified from the
exit code alone, and that is not an error.** Three shapes of "no usable
document", one degradation path:

1. The `verdict` output parameter is **absent entirely** (e.g. the command
   backend, plan §Component 5 `command` bullet — plain exit-code contract).
2. The document is **malformed JSON** (truncated output, wrong shape).
3. The document's **`schema_version` is one this parser does not know** —
   `VerdictJson::parse` returns `Err`, never a panic or a guess.

Interpretation then falls to the exit-code ladder:

- `Verdict::interpret(exit_code, verdict_json: Option<&str>)`
  (`src/backend/mod.rs`) — the phase-less entry point: `None` or unparseable
  ⇒ `Verdict::from_exit_code` (0 pass, 1 test failure, ≥2 infra); a parseable
  document is authoritative even against the caller-side exit code.
- `VerdictJson::from_exit_code(phase, exit_code)` (`src/verdict.rs`) — the
  phase-aware entry point the Argo backend uses when a terminal phase carries
  no usable `verdict` parameter: the workflow's own phase joins the ladder, so
  `Error` ⇒ `InfraFailure` no matter what the exit code claims. Pinned by
  `from_exit_code_is_the_minimal_document_ladder` to be structurally the same
  ladder a parsed minimal document goes through — the fallback and the parse
  path share one implementation.

No infra signal and no gate attribution is ever *assumed* in degradation,
because none is known. Absence degrades; it never fabricates.

## Verification mapping (bf-2jnj acceptance)

| bf-2jnj acceptance bullet | Evidence |
|---|---|
| "verdict parsing compiles" | `src/verdict.rs` is the shipped module, re-exported through `crate::backend`; full `cargo test` green |
| "reads versioned format" | `parse_accepts_complete_schema_one_payload`, `schema_version_zero_and_two_are_rejected_with_a_clear_error`, `property_unsupported_schema_versions_are_rejected` |
| "handles optional failure_class" | `failure_class_strings_parse_to_variants`, `null_failure_class_reads_as_absent`, `unknown_failure_class_degrades_to_absent_not_error`, `absent_optional_fields_default_cleanly` |
| "defaults to exit-code-only when absent" | `interpret_absent_document_uses_exit_code_only`, `interpret_malformed_or_mismatched_document_degrades_to_exit_code`, `from_exit_code_*` suite |

Note: bf-2jnj's description says the version field is a top-level `version`;
the shipped field is `schema_version` (matching every versioned record in the
plan and the `docs/schemas/*-v1.json` convention). The description's
`src/verdict.rs` path, optional-`failure_class`, and exit-code-only
acceptance all match what shipped; only that field name is superseded — same
situation as the retired names in `backend-type-naming.md`.
