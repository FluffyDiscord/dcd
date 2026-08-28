# dcd — repo guide

`dcd` is a single static Rust binary: a zero-downtime **red-black** Docker deployer driven
by a `dcd.yaml`, extensible with sandboxed embedded-Lua plugins. It runs on the **deploying
machine** (CI runner or workstation) and drives the target over SSH; nothing is installed
there. The operator's compose file declares the containers, `dcd.yaml` the orchestration.

## Helping someone write a `dcd.yaml`

Read **[AGENTS.md](AGENTS.md)** — the authoring + validation playbook (config helper, guide,
and validator). Field reference: `docs/examples/all_in_one/dcd.yaml`. A real, lean config:
`docs/examples/roadrunner_app/`.

## Build · test · lint (host, no Docker except e2e)

```bash
cargo build
cargo clippy --all-targets -- -D warnings        # must be clean
cargo test                                        # unit + integration, ZERO Docker
```

The Docker suites are `#[ignore]`d, so a plain `cargo test` reports them **ignored**
rather than passing — a gate that returns early reports green while asserting nothing.
Opt in explicitly:

```bash
cargo test --test e2e         -- --test-threads=1 --include-ignored   # real Docker
cargo test --test ssh_shells  -- --test-threads=1 --include-ignored   # 9 real login shells
cargo test --test e2e_ssh     -- --test-threads=1 --include-ignored   # a deploy over a real sshd
```

`e2e_ssh` bind-mounts `/var/run/docker.sock` into the fixture, so it needs a
socket-reachable daemon and fails loudly against a TCP-only `DOCKER_HOST` (a dind
CI service). CI runs the first two; `e2e_ssh` is developer-machine-only.

## Where things live

- `src/config/` — typed schema (`Config`), load → merge stage → interpolate → `--set` → validate
- `src/compose.rs` — the resolved compose model (`docker compose config`), the source of every container fact
- `src/ssh.rs` — the ssh invocation (`-- sh`, script on stdin), POSIX quoting, option baseline, ControlMaster
- `src/engine.rs` — the fixed red-black recipe: 13 steps + `before_`/`after_` hook slots
- `src/docker.rs` — pure `docker`/`compose` argv builders (every command is unit-asserted)
- `src/effects/` — the test seam (`CommandRunner`/`FileSystem`/`Clock`), local and ssh impls; the whole deploy runs with zero Docker
- `src/lua.rs` — the plugin host; `ctx.cfg` / `ctx.state` are live-mutable and read back into the engine
- `src/state.rs` — `dcd-state.json` + the release state machine
- `docs/implementation-spec.md` — the living spec · `docs/strategic-blueprint.md` — decisions/ADRs

## KEEP THESE IN SYNC

The config schema has four representations that must agree. **Change one → change all:**

1. `src/config/mod.rs` — the typed `Config` (the source of truth)
2. `docs/examples/all_in_one/dcd.yaml` — every field, described, with its default
3. `AGENTS.md` — the authoring guide (§2 schema, §5 rules, §7 error→fix table)
4. `src/cli.rs` — the `--help` text **and** the `SCAFFOLD` that `dcd init` writes

Also touch `docs/examples/roadrunner_app/`, `docs/examples/fpm_app/`, `UPGRADE.md` and
`docs/implementation-spec.md` when the change is user-visible. After any schema edit, re-run
`dcd check` on **all three** example configs, from inside each directory
(`check` resolves the compose model, so each example ships a compose file):

```bash
(cd docs/examples/all_in_one && dcd check prod && dcd check beta)      # no env needed
(cd docs/examples/roadrunner_app && DEPLOY_SSH=x DEPLOY_ROOT=/srv/a dcd check prod)
(cd docs/examples/fpm_app && DEPLOY_SSH=x DEPLOY_ROOT=/srv/a REGISTRY=r APP_TAG=v1 \
   ROUTER_TAG=r1 ROUTER_PORT=8080 dcd check prod)
```

**Both** stages of `all_in_one`, not just `prod` — half a field reference that only
resolves for one stage is half a field reference.

## Conventions

- Side effects go through the effects seam only — never raw `std::process` / `std::fs` in the engine or recipe.
- No secret redaction (intentionally removed) — instead, env **values are never printed or written**: secrets come
  from the Symfony-style dotenv chain (`src/dotenv/`, spec §5.2) and reach containers as bare `-e KEY`, with the
  values riding a document on ssh **stdin**; dcd writes no env file. Never print `docker compose config` stdout —
  it inlines resolved env values. Schema changes here also touch UPGRADE.md.
  The one place a value could have escaped was the dotenv **parse error**, which quotes raw
  bytes around the cursor — an apostrophe in one value printed the next variable's secret.
  `mask_values` now masks every value byte in that window, a deliberate divergence from
  Symfony's byte-exact snippet. Keep it that way: no path may print a value.
- One app container per release — there is no replica/scale knob yet.
