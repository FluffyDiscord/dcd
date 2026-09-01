# dcd — repo guide

A single static Rust binary: a zero-downtime **red-black** Docker deployer driven by a
`dcd.yaml`, extensible with sandboxed embedded-Lua plugins. It runs on the **deploying
machine** (CI runner or workstation) and drives the target over SSH; nothing is installed
there. The operator's compose file declares the containers, `dcd.yaml` the orchestration.

## Writing a `dcd.yaml`

Read **[AGENTS.md](AGENTS.md)** — the authoring + validation playbook (config helper, guide,
validator). Field reference: `docs/examples/all_in_one/dcd.yaml`. A real, lean config:
`docs/examples/roadrunner_app/`.

## Build · test · lint

Host, no Docker:

```bash
cargo build
cargo clippy --all-targets -- -D warnings        # must be clean
cargo test                                       # unit + integration, ZERO Docker
```

The Docker suites are `#[ignore]`d, so a plain `cargo test` reports them **ignored** rather
than passing — a gate that returns early reports green while asserting nothing. Opt in:

```bash
cargo test --test e2e         -- --test-threads=1 --include-ignored   # real Docker
cargo test --test ssh_shells  -- --test-threads=1 --include-ignored   # 9 real login shells
cargo test --test e2e_ssh     -- --test-threads=1 --include-ignored   # a deploy over a real sshd
```

`e2e_ssh` bind-mounts `/var/run/docker.sock` into the fixture, so it needs a socket-reachable
daemon and fails loudly against a TCP-only `DOCKER_HOST` (a dind CI service). CI runs the
first two; `e2e_ssh` is developer-machine-only.

## Where things live

- `src/config/` — typed schema (`Config`), load → merge stage → interpolate → `--set` → validate
- `src/compose.rs` — the resolved compose model (`docker compose config`), the source of every container fact
- `src/ssh.rs` — the ssh invocation (`-- sh`, script on stdin), POSIX quoting, option baseline, ControlMaster
- `src/engine.rs` — the fixed red-black recipe: 13 steps + `before_`/`after_` hook slots
- `src/docker.rs` — pure `docker`/`compose` argv builders (every command unit-asserted)
- `src/effects/` — the test seam (`CommandRunner`/`FileSystem`/`Clock`), local and ssh impls; the whole deploy runs with zero Docker
- `src/lua.rs` — the plugin host; `ctx.cfg` / `ctx.state` are live-mutable and read back into the engine
- `src/state.rs` — `dcd-state.json` + the release state machine
- `docs/implementation-spec.md` — the living spec · `docs/strategic-blueprint.md` — decisions/ADRs

## KEEP THESE IN SYNC

Four representations of the config schema must agree. **Change one → change all:**

1. `src/config/mod.rs` — the typed `Config` (the source of truth)
2. `docs/examples/all_in_one/dcd.yaml` — every field, described, with its default
3. `AGENTS.md` — §2 schema, §5 rules, §7 error→fix table
4. `src/cli.rs` — the `--help` text **and** the `SCAFFOLD` that `dcd init` writes

A user-visible change also touches `docs/examples/roadrunner_app/`, `docs/examples/fpm_app/`,
`UPGRADE.md` and `docs/implementation-spec.md`.

After any schema edit, re-run `dcd check` from inside each example directory (`check` resolves
the compose model, so each example ships a compose file) — **both** stages of `all_in_one`,
since a field reference that resolves for only one stage is half a field reference:

```bash
(cd docs/examples/all_in_one && dcd check prod && dcd check beta)      # no env needed
(cd docs/examples/roadrunner_app && DEPLOY_SSH=x DEPLOY_ROOT=/srv/a dcd check prod)
(cd docs/examples/fpm_app && DEPLOY_SSH=x DEPLOY_ROOT=/srv/a REGISTRY=r APP_TAG=v1 \
   ROUTER_TAG=r1 ROUTER_PORT=8080 dcd check prod)
```

## Conventions

- Side effects go through the effects seam only — never raw `std::process` / `std::fs` in the
  engine or recipe.
- One app container per release. There is no replica/scale knob yet.
- **No secret redaction** (intentionally removed). Env values are never printed or written:
  - Secrets come from the Symfony-style dotenv chain (`src/dotenv/`, spec §5.2) and reach
    containers as bare `-e KEY`, values riding a document on ssh **stdin**. dcd writes no env file.
  - Never print `docker compose config` stdout — it inlines resolved env values.
  - The one place a value could have escaped was the dotenv **parse error**, which quotes raw
    bytes around the cursor: an apostrophe in one value printed the next variable's secret.
    Two fixes, both kept — the **parser** treats a partnerless apostrophe inside a value as a
    literal byte, so `PASS=pa'ss` parses (spec §5.2.1); `mask_values` masks every value byte
    in the error window for the errors that remain, a deliberate divergence from Symfony's
    byte-exact snippet.
  - No path may print a value. Schema changes here also touch `UPGRADE.md`.
- **The dotenv oracle is upstream's own suite, vendored**:
  `tests/fixtures/symfony/DotenvTest.php` → `extract_cases.php` (run in a php container) →
  `dotenv_cases.json`, which `src/dotenv/tests.rs` executes case by case. Every upstream case
  must pass verbatim; the only allowed answers-differently list is the completed-`$(…)`
  refusals, named by exact input. Change the parser only in ways that keep that true.
