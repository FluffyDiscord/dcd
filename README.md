# dcd

Zero-downtime **red-black** Docker deploys from a YAML file. One static binary that runs on
**your** machine — a CI runner or a laptop — and drives the target over SSH. Nothing is
installed on the server.

It creates the new ("black") container from your compose service next to the live ("red") one,
waits for its health gate, flips the router to it, and drains the old one — without dropping a
request.

**Your compose file declares the containers; `dcd.yaml` declares the orchestration.** Images,
env, volumes, restart policies and network aliases stay where you already write them.

## Quickstart

```bash
dcd init --from-compose docker-compose.prod.yml   # derive a dcd.yaml from what you have
$EDITOR dcd.yaml                                  # name the app + router services
dcd check prod                                    # validate it (no ssh, touches nothing)
dcd deploy prod --dry-run                         # see every action, run nothing
dcd deploy prod                                   # do it
```

## Commands

| Command | What it does |
|---------|--------------|
| `dcd deploy [stage]` | the red-black deploy |
| `dcd deploy --resume [stage]` | finish a deploy that died after the cutover |
| `dcd rollback [stage]` | re-point to the previous release (code only — no migrations) |
| `dcd unlock [stage]` | escape hatch: accept the stuck release as deployed and clear the stage lock |
| `dcd status [stage]` | current release + history |
| `dcd tasks [stage]` | print the step plan |
| `dcd deploy --dry-run` | print every command, touch nothing |
| `dcd gc [stage]` | reclaim disk: remove image versions past the retention counts (`--all` also offers host tags dcd never recorded, after asking) |
| `dcd check [stage]` | validate the config |
| `dcd init` | scaffold a `dcd.yaml` (`--with-plugin` adds a Lua stub) |
| `dcd init --from-compose <file>` | derive a `dcd.yaml` from an existing compose file |
| `dcd schema` | print a JSON Schema for `dcd.yaml`, for editor completion |
| `dcd deploy -v [stage]` | trace every command: argv, exit code, elapsed, output |
| `dcd --version` | the built version (`-V`) |

Global flags: `--config <path>` · `--ssh <target>` · `--env-dir <path>` · `--env-file <path>` ·
`--env-stdin` · `--json` · `--dry-run` · `--resume` · `--image <service>=<ref>` (repeatable) ·
`--set path=value` (repeatable) · `--yes` · `--reason <text>` · `-v/--verbose` · `-V/--version`.

Env comes from a Symfony-style dotenv chain next to `dcd.yaml` (`.env` → `.env.local` →
`.env.<stage>` → `.env.<stage>.local`, real env wins). Every chain-defined key reaches the
containers as a bare `-e KEY`, values riding a document on ssh **stdin** — no value ever
appears in an argv on either machine, and dcd writes no env file anywhere. `--env-file
.env.deploy` rebases the whole chain onto another base name (`.env.deploy` →
`.env.deploy.local` → `.env.deploy.<stage>` → `.env.deploy.<stage>.local`), so dcd's chain can
live beside the app's own `.env` files without colliding.

## When a deploy gets stuck

A deploy that dies **after** the cutover leaves the new container live and the release recorded
as incomplete. `dcd deploy` then refuses (exit 4) until you pick a way out:

```bash
dcd status prod                # what is live, what is incomplete
dcd deploy --resume prod       # finish it: drain the old one, migrate:after, workers, done
dcd rollback prod              # go back to the previous release instead
dcd unlock prod                # accept what is live as done, and clear the lock
```

`unlock` is the last resort — resuming keeps failing, or a killed deploy left the stage locked.
It marks the incomplete release active and current, and removes the stage lock **even while
another dcd holds it**. That is all it does:

- No containers started, stopped or removed. No migrations. No workers recreated. No hooks fire
  — so nothing that already failed can block it.
- It warns about each leftover by name; the next `dcd deploy` runs fresh and cleans them up.
- With nothing incomplete, it only clears the lock.

## dcd.yaml

Drives the orchestration — which service is cut over to, how traffic switches, migrations,
drain, worker discovery, recreate policy, retention, and simple `hooks`. Containers themselves
are declared in your compose file. The **common case needs no Lua**. See the fully-worked
[example](docs/examples/roadrunner_app/) — config *and* its compose file.

## Lua plugins

Only when YAML isn't enough. List them under `plugins: [plugins/app.lua]`. A plugin file
registers tasks/hooks at the top level; each hook gets a `ctx`.

### Top-level functions

| Function | Does |
|----------|------|
| `task(name, fn)` | define a reusable body (`fn` gets `ctx`) to wire into a slot by name |
| `before(step, hook)` | run `hook` before a step — `hook` is a task name or `function(ctx)`. A step that does not exist is an error at load, never a hook that quietly never fires |
| `after(step, hook)` | run `hook` after a step — same check |
| `configure(fn)` | adjust `cfg` **once, before the deploy** (reads `state`/`env` to decide) |
| `set(k, v)` / `get(k)` | scratch vars (same store as `ctx.set/get`) |
| `cfg` / `state` | the live config / deploy state — **mutable**, same tables as `ctx.cfg`/`ctx.state` |

Hook steps for `before_`/`after_` — the complete list, and anything else is an error at load:
`sync` · `preflight` · `ensure_upstream` · `pull` · `infra` · `migrate:before` · `start:black` ·
`healthcheck` · `cutover` · `drain:red` · `migrate:after` · `workers` · `finalize`

`configure` is **not** among them — it is registered with `configure(fn)`, not hooked, and runs
once before the recipe. A `task()` is not a step either: wire one into a slot with
`after('cutover', 'my_task')`.

### `ctx` — effects

Routed through the engine, so they are **dry-run-safe** (and observable in `--dry-run`).

| Call | Returns | Does |
|------|---------|------|
| `ctx.run(cmd)` | stdout | shell command on the target (in `deploy_root`) |
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

`cfg` and `state` are **live**: assign with plain Lua
(`ctx.cfg.retention.keep_releases = 5`) and the engine reads the change back before the next
step — there is no setter function. Config applies to steps not yet run; state to what gets
persisted. Structural fields fixed at deploy start (`images`, the container name,
`deploy_root`) are snapshots.

| Field | Is | Mutable |
|-------|-----|---------|
| `ctx.cfg` | the parsed config | **yes** — change for later steps (retention, healthcheck, cutover, drain, workers…) |
| `ctx.state` | current stage: `{ current, releases = [ { id, container, status, images, ran_migrations, reason } ] }` | **yes** — read back into deploy state; persisted once past cutover |
| `ctx.vars` | scratch table shared across all hooks in a run | yes (not part of cfg/state) |
| `ctx.set(k, v)` / `ctx.get(k)` | the same scratch store, by key | — |
| `ctx.container` | the new (black) container name | no |
| `ctx.stage` | the stage, e.g. `'prod'` | no |

> `ctx.state` is full power: you can rewrite `releases`/`current`/`status`, and you can
> break rollback/resume (≤1 `cutover_pending`, etc.) — the engine trusts what you write.

> Raw `os.execute` / `io.open` / `io.popen` are sandboxed out — use `ctx.run(...)` so the action
> shows up in `--dry-run`. `ctx.run('jq …')`, `ctx.run('bash script.sh')` are fair game.

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
copied into the **CI image that runs the deploy** (dcd runs there, not on the server):

```dockerfile
COPY --from=ghcr.io/fluffydiscord/dcd:0.5.3 /usr/local/bin/dcd /usr/local/bin/dcd
```

**A version tag is immutable and names one exact release. Pin it.** No `0.5` tag is published —
a partial-version tag can only ever move you onto a build you did not choose.

| Tag | What it points at |
|-----|-------------------|
| `0.5.3` | Debian-slim, the release `v0.5.3` |
| `0.5.3-alpine` | Alpine, the same release |
| `latest` | Debian-slim, the current tip of `master` |
| `latest-alpine` | Alpine, the same build |

Tag a release to publish one:

```bash
git tag v0.5.3 && git push origin v0.5.3
```

> **`latest` is not a release — do not deploy it.** It is rebuilt on every push to `master`, so
> it is whatever the branch happens to be: untagged, unreleased, and moving under you between
> two CI runs of the same pipeline. It exists to try the newest work, nothing more. A version
> tag is the real release; prefer it everywhere, and in production use nothing else.

## Build & test

```bash
cargo test                                                  # unit + integration, no Docker
cargo test --test e2e -- --test-threads=1 --include-ignored      # real-Docker integration
cargo test --test e2e_ssh -- --test-threads=1 --include-ignored  # a full deploy over ssh
cargo build --release --target x86_64-unknown-linux-musl    # static binary
```

## Docs

- [Implementation spec](docs/implementation-spec.md) — the buildable contract.
- [Strategic blueprint](docs/strategic-blueprint.md) — decisions + ADRs.
- [RoadRunner example](docs/examples/roadrunner_app/) — a full deploy-script → `dcd.yaml` translation + CI wiring.
