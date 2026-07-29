# dcd

Zero-downtime **red-black** Docker deploys from a YAML file targeting Nginx as proxy. One static binary, runs on
the server, talks to the local Docker socket.

It builds the new ("black") container next to the live ("red") one, health-checks it,
flips nginx to it, and drains the old one — without dropping a request.

## Quickstart

```bash
dcd init                 # writes a starter dcd.yaml
$EDITOR dcd.yaml         # fill in your images / services / healthcheck
dcd check prod           # validate it
dcd deploy prod --dry-run   # see every action, run nothing
dcd deploy prod          # do it
```

## Commands

| Command | What it does |
|---------|--------------|
| `dcd deploy [stage]` | the red-black deploy |
| `dcd deploy --resume [stage]` | finish a deploy that died after the cutover |
| `dcd rollback [stage]` | re-point to the previous release (code only — no migrations) |
| `dcd status [stage]` | current release + history |
| `dcd tasks [stage]` | print the step plan |
| `dcd deploy --dry-run` | print every command, touch nothing |
| `dcd check [stage]` | validate the config |
| `dcd init` | scaffold a `dcd.yaml` (`--with-plugin` adds a Lua stub) |
| `dcd --version` | the built version (`-v`) |

Global flags: `--config <path>` · `--env-dir <path>` · `--env-file <path>` · `--env-stdin` ·
`--json` · `--image app=<tag>` (repeatable) · `--set path=value` (repeatable) · `--yes` ·
`--reason <text>` · `--version`.

Env comes from a Symfony-style dotenv chain next to `dcd.yaml` (`.env` → `.env.local` →
`.env.<stage>` → `.env.<stage>.local`, real env wins); every chain-defined key reaches the
containers via process-env passthrough — dcd writes no env file on the server.
`--env-file .env.deploy` rebases the whole chain onto another base name
(`.env.deploy` → `.env.deploy.local` → `.env.deploy.<stage>` → `.env.deploy.<stage>.local`),
so dcd's chain can live beside the app's own `.env` files without colliding.

## dcd.yaml

Drives the whole deploy — images, managed services, the app's healthcheck, migrations,
workers, cutover, retention, and simple `hooks`. The **common case needs no Lua**.
See the fully-worked [RoadRunner example](docs/examples/roadrunner_app/dcd.yaml).

## Lua plugins

Only when YAML isn't enough. List them under `plugins: [plugins/app.lua]`. A plugin file
registers tasks/hooks at the top level; each hook gets a `ctx`.

### Top-level functions

| Function | Does |
|----------|------|
| `task(name, fn)` | define a step you can hook onto (`fn` gets `ctx`) |
| `before(step, hook)` | run `hook` before a step — `hook` is a task name or `function(ctx)` |
| `after(step, hook)` | run `hook` after a step |
| `configure(fn)` | adjust `cfg` **once, before the deploy** (reads `state`/`env` to decide) |
| `set(k, v)` / `get(k)` | scratch vars (same store as `ctx.set/get`) |
| `cfg` / `state` | the live config / deploy state — **mutable**, same tables as `ctx.cfg`/`ctx.state` |

Hook steps you can target with `before_`/`after_`:
`preflight` · `ensure_upstream` · `pull` · `infra` · `migrate:before` · `start:black` ·
`healthcheck` · `cutover` · `drain:red` · `migrate:after` · `workers` · `finalize`
(plus the special `configure`).

### `ctx` — effects

Routed through the engine, so they're **dry-run-safe** (and observable in `--dry-run`).

| Call | Returns | Does |
|------|---------|------|
| `ctx.run(cmd)` | stdout | shell command on the server (in `deploy_root`) |
| `ctx.in_release(cmd)` | stdout | run **inside the new app container** |
| `ctx.exec_in(svc, cmd)` | stdout | run inside a managed service (e.g. `'nginx'`) |
| `ctx.docker({args})` | stdout | raw `docker …` |
| `ctx.compose({args})` | stdout | `docker compose …` (project + files already wired) |
| `ctx.cp_from_release(src, dst)` | — | copy a file **out of** the new container |
| `ctx.cp_to_release(src, dst)` | — | copy a file **into** the new container |
| `ctx.read_file(path)` | string | read a file (relative to `deploy_root`) |
| `ctx.write_file(path, s)` | — | write a file (skipped in `--dry-run`) |
| `ctx.file_exists(path)` | bool | |
| `ctx.env(name)` | string \| nil | read the resolved environment (process env over the dotenv chain) |

### `ctx` — utilities & debug

| Call | Returns | Does |
|------|---------|------|
| `ctx.json_decode(s)` / `ctx.json_encode(v)` | value / string | JSON |
| `ctx.yaml_decode(s)` / `ctx.yaml_encode(v)` | value / string | YAML |
| `ctx.log(msg)` / `ctx.warn(msg)` | — | print to the deploy output |
| `ctx.inspect(v)` | string | pretty-print a value as YAML |
| `ctx.dump(v)` | — | log `v` as YAML; **`ctx.dump()` with no arg = `cfg` + `state`** |

### `ctx` — data

`cfg` and `state` are **live**: assign to them with plain Lua (`ctx.cfg.retention.keep_releases = 1`)
and the engine reads the change back before the next step — there is no setter function. The
deploy then honors it: config for steps not yet run, state for what gets persisted. (Structural
fields fixed at deploy start — `images`, the container name, `deploy_root` — are snapshots.)

| Field | Is | Mutable |
|-------|-----|---------|
| `ctx.cfg` | the parsed config | **yes** — change for later steps (retention, healthcheck, cutover, drain, workers…) |
| `ctx.state` | current stage: `{ current, releases = [ { id, container, status, images, ran_migrations, reason } ] }` | **yes** — read back into deploy state; persisted once past cutover |
| `ctx.vars` | scratch table shared across all hooks in a run | yes (not part of cfg/state) |
| `ctx.set(k, v)` / `ctx.get(k)` | the same scratch store, by key | — |
| `ctx.container` | the new (black) container name | no |
| `ctx.stage` | the stage, e.g. `'prod'` | no |

> `ctx.state` is full power: you can rewrite `releases`/`current`/`status`. That also means you
> can break rollback/resume (≤1 `cutover_pending`, etc.) — the engine trusts what you write.

> Raw `os.execute` / `io.open` / `io.popen` are sandboxed out — use `ctx.run(...)` so the
> action shows up in `--dry-run`. `ctx.run('jq …')`,
> `ctx.run('bash script.sh')`, etc. are all fair game.

### Examples

```lua
-- bump retention on a canary deploy, before anything runs (plain assignment — no setter)
configure(function(ctx)
  if ctx.env('CANARY') == '1' then ctx.cfg.retention.keep_releases = 5 end
end)

-- generate centrifugo config from the running app, after health passes
task('centrifugo', function(ctx)
  ctx.in_release('php bin/console app:realtime:config --output=/tmp/c.json')
  ctx.cp_from_release('/tmp/c.json', '.docker/centrifugo/config.json')
  ctx.compose({'up', '-d', 'centrifugo'})
end)
after('healthcheck', 'centrifugo')

-- print cfg + state to the deploy log for debugging
after('cutover', function(ctx) ctx.dump() end)
```

## Container images

Every release publishes the binary as `linux/amd64` + `linux/arm64` images on GHCR, in a
Debian-slim and an Alpine flavour. The binary is statically linked, so either flavour can be
copied into any base image:

```dockerfile
COPY --from=ghcr.io/fluffydiscord/dcd:0.3.0 /usr/local/bin/dcd /usr/local/bin/dcd
```

| Tag | What it points at |
|-----|-------------------|
| `0.3.0`, `0.3`, `latest` | Debian-slim, the release `v0.3.0` |
| `0.3.0-alpine`, `0.3-alpine`, `latest-alpine` | Alpine, the same release |
| `edge`, `edge-alpine` | the current `master` |

Tag a release to publish one:

```bash
git tag v0.3.0 && git push origin v0.3.0
```

## Build & test

```bash
cargo test                                                  # unit + integration, no Docker
DCD_E2E=1 cargo test --test e2e -- --test-threads=1         # real-Docker integration
cargo build --release --target x86_64-unknown-linux-musl    # static binary
```

## Docs

- [Implementation spec](docs/implementation-spec.md) — the buildable contract.
- [Strategic blueprint](docs/strategic-blueprint.md) — decisions + ADRs.
- [RoadRunner example](docs/examples/roadrunner_app/) — a full deploy-script → `dcd.yaml` translation + CI wiring.
