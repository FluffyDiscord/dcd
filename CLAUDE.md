# dcd — repo guide

`dcd` is a single static Rust binary: a zero-downtime **red-black** Docker deployer driven
by a `dcd.yaml`, extensible with sandboxed embedded-Lua plugins. It runs on the target
server against the local Docker socket.

## Helping someone write a `dcd.yaml`

Read **[AGENTS.md](AGENTS.md)** — the authoring + validation playbook (config helper, guide,
and validator). Field reference: `docs/examples/all_in_one/dcd.yaml`. A real, lean config:
`docs/examples/roadrunner_app/`.

## Build · test · lint (host, no Docker except e2e)

```bash
cargo build
cargo clippy --all-targets -- -D warnings        # must be clean
cargo test                                        # unit + integration, ZERO Docker
DCD_E2E=1 cargo test --test e2e -- --test-threads=1   # real Docker, needs a daemon
```

## Where things live

- `src/config/` — typed schema (`Config`), load → merge stage → interpolate → `--set` → validate
- `src/engine.rs` — the fixed red-black recipe: 12 steps + `before_`/`after_` hook slots
- `src/docker.rs` — pure `docker`/`compose` argv builders (every command is unit-asserted)
- `src/effects/` — the test seam (`CommandRunner`/`FileSystem`/`Clock`); the whole deploy runs with zero Docker
- `src/lua.rs` — the plugin host; `ctx.cfg` / `ctx.state` are live-mutable and read back into the engine
- `src/state.rs` — `dcd-state.json` + the release state machine
- `docs/implementation-spec.md` — the living spec · `docs/strategic-blueprint.md` — decisions/ADRs

## KEEP THESE IN SYNC

The config schema has four representations that must agree. **Change one → change all:**

1. `src/config/mod.rs` — the typed `Config` (the source of truth)
2. `docs/examples/all_in_one/dcd.yaml` — every field, described, with its default
3. `AGENTS.md` — the authoring guide (§2 schema, §5 rules, §7 error→fix table)
4. `src/cli.rs` — the `--help` text **and** the `SCAFFOLD` that `dcd init` writes

Also touch `docs/examples/roadrunner_app/dcd.yaml` and `docs/implementation-spec.md` when the change
is user-visible. After any schema edit, re-run `dcd check` on **both** example configs (the
all_in_one validates with no env set; acme needs `REGISTRY`/`DEPLOY_ROOT`/`MAXMIND_*`).

## Conventions

- Side effects go through the effects seam only — never raw `std::process` / `std::fs` in the engine or recipe.
- No secret redaction (intentionally removed) — instead, env **values are never printed or written**: secrets come
  from the Symfony-style dotenv chain (`src/dotenv/`, spec §5.2) and reach containers via process-env passthrough
  (bare `-e KEY`); dcd writes no env file. Schema changes here also touch UPGRADE.md.
- One app container per release — there is no replica/scale knob yet.
