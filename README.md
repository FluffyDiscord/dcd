# dcd

Zero-downtime **red-black** Docker deploys from a YAML config, extensible with Lua.
One static binary, runs on the target server, talks to the local Docker socket.

It is a generalized, typed, tested replacement for the hand-rolled the original script-style
script: the red-black logic is written and tested once; each project supplies a
`dcd.yaml` (and optional Lua plugins).

## What it does

Builds the **black** container alongside the live **red** one, health-checks it, then
does an atomic nginx-upstream cutover and drains red — never dropping a request. It
also handles conditional managed-service recreation, two-phase migrations, dynamic
worker generation, graceful drain, rollback, crash recovery, and a concurrency lock.

```
dcd deploy [stage]            # the red-black deploy
dcd deploy --resume [stage]   # finish a deploy that died after cutover
dcd rollback [stage]          # re-point to the previous release (code only, no migrations)
dcd status [stage]            # current release + history
dcd tasks [stage]             # print the resolved plan
dcd deploy --dry-run          # print every action, execute nothing
dcd check [stage]             # validate the config
dcd init                      # scaffold a dcd.yaml
```

## Design

- **Runs on the server** against the local Docker socket; CI ships the binary + config.
- **Fixed recipe + hook slots** — the `docker-redblack` flow with `before_<step>` /
  `after_<step>` hooks. Not a general task DAG.
- **Shells out to `docker`** — every action is exactly a command you could run by hand;
  `--dry-run` prints them.
- **Effects seam** — all process/file/clock effects go through traits, so the whole
  pipeline is asserted against exact argv with zero Docker in unit tests.
- **Code-only rollback**; migrations are forward-only (expand-contract).

## Extensibility (Lua)

The common case needs **zero Lua** — `dcd.yaml` drives the recipe, and simple
project steps are YAML `hooks`. For real logic, plugins register tasks and hooks:

```lua
-- adjust the config before the deploy, based on runtime truths
configure(function(ctx)
  if ctx.env('CANARY') == '1' then ctx.set_config('retention.keep_releases', 5) end
end)

-- a custom step after a recipe stage
task('centrifugo', function(ctx)
  local cfg = ctx.json_decode(ctx.in_release('app config:dump --json'))
  ctx.write_file('.docker/centrifugo/config.json', ctx.json_encode(cfg))
  ctx.compose({'up', '-d', 'centrifugo'})
end)
after('healthcheck', 'centrifugo')
```

`ctx` gives engine-routed effects (`run`, `in_release`, `exec_in`, `docker`, `compose`,
`cp_*`, `read_file`, `write_file`, `env` — all dry-run-safe and secret-redacted), utilities
(`json`/`yaml` encode+decode, `log`, `warn`, `dump`, `inspect`), and data (`cfg`, `state`,
and a persistent `vars` scratch shared across hooks). Raw `os.execute`/`io` are sandboxed
out so every effect stays observable.

## Docs

- [Implementation spec](docs/implementation-spec.md) — the buildable contract.
- [Strategic blueprint](docs/strategic-blueprint.md) — the why.
- [RoadRunner example](docs/examples/roadrunner_app/) — a full deploy-script → `dcd.yaml` translation
  and the CI wiring.

## Build & test

```
cargo test                                   # unit + integration (no Docker)
DCD_E2E=1 cargo test --test e2e -- --test-threads=1   # real-Docker integration
cargo build --release --target x86_64-unknown-linux-musl   # static binary
```
