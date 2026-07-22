# Naming Rationale

## Chosen: `gantry`

A **gantry crane** is the machine at a container port that lifts cargo containers off a ship and decides where they land — shore, truck, or another vessel. That is precisely this tool's job: it picks up `cargo` workloads and places them on the right executor (remote cluster or capped local run). The metaphor is exact, short, memorable, and lowercase-friendly for a CLI.

The working title during ideation was `cargo-remote-test`, rejected as descriptive-but-collision-prone and unpronounceable as a brand.

## Collision check (2026-07-21)

- **crates.io:** no `gantry` tool crate in active use. The only hits are `gantry-protocol` and `gantryclient` — components of the long-deprecated waSCC (pre-wasmCloud) actor registry, last published ~5 years ago. Whether the bare `gantry` crate name is registered/squatted must be verified at publish time; fallback publish names: `gantry-shim`, `cargo-gantry` (see plan.md open questions).
- **`cargo-remote` is taken** — [sgeisler/cargo-remote](https://github.com/sgeisler/cargo-remote) is an established crate with real usage (SSH+rsync build offload). This also motivated renaming the in-house script when extracting it.
- General software: "Gantry" is a WordPress/Joomla theme framework — different ecosystem entirely, no practical confusion for a Rust CLI.

## Alternatives considered

| Name | Metaphor | Rejected because |
|---|---|---|
| `stevedore` | dock worker who loads/unloads cargo | well-known Python/OpenStack plugin library of the same name |
| `longshore` | longshoremen handle cargo at docks | weaker as a verb/tool name; less obviously mechanical |
| `ferry` | ferries cargo to another vessel; Ferris pun | cute but ambiguous (file-transfer connotations); likely crate collisions |
| `derrick` / `hoist` | lifting equipment | generic lifting, loses the *container port* specificity |
| `transship` | moving cargo between vessels | clunky, hard to say |
| `cargo-remote-test` | literal | collides conceptually with `cargo-remote`, not a brand |

## House style

Consistent with the local naming culture (FORGE, NEEDLE, SIGIL, ARMOR, CLASP): a short, physical, single-word object whose real-world function mirrors the tool's. Repo and binary are lowercase `gantry`; prose may style it GANTRY.
