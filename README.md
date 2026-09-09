# dcd

Deploy Docker containers without dropping a request, driven by one YAML file.

dcd is a single static binary that runs on **your** machine — a CI runner or a laptop — and
drives the server over SSH. Nothing is installed on the server.

## How a deploy works

1. Start the new container, from your compose service, next to the one serving traffic.
2. Wait for its health check to pass.
3. Point the router at the new container.
4. Drain and remove the old one.

If the new container never gets healthy, the old one keeps serving and nothing changed.

The old and new containers are called red and black, and that is the name you will see in the
rest of the docs.

**Your compose file declares the containers. `dcd.yaml` declares the deploy.** Images, env,
volumes, restart policies and network aliases stay where you already write them.

## Quickstart

```bash
dcd init --from-compose docker-compose.prod.yml   # write a dcd.yaml from what you have
$EDITOR dcd.yaml                                  # name the app and router services
dcd check prod                                    # validate it; no ssh, changes nothing
dcd deploy prod --dry-run                         # print every action, run none of them
dcd deploy prod                                   # do it
```

## Commands

| Command | What it does |
|---------|--------------|
| `dcd deploy [stage]` | Deploy. |
| `dcd deploy --resume [stage]` | Finish a deploy that died after the switchover. |
| `dcd rollback [stage]` | Point back at the previous release. Code only — no migrations. |
| `dcd status [stage]` | Show the current release and the history. |
| `dcd tasks [stage]` | Print the list of steps. |
| `dcd deploy --dry-run` | Print every command, change nothing. |
| `dcd gc [stage]` | Free disk space by removing old images. |
| `dcd check [stage]` | Validate the config. |
| `dcd init` | Write a starter `dcd.yaml`. `--with-plugin` adds a Lua stub. |
| `dcd init --from-compose <file>` | Write a `dcd.yaml` from an existing compose file. |
| `dcd schema` | Print a JSON Schema for `dcd.yaml`, for editor completion. |
| `dcd unlock [stage]` | Last resort. Accept a stuck release as deployed and clear the lock. |
| `dcd deploy -v [stage]` | Trace every command: argv, exit code, elapsed time, output. |
| `dcd --version` | Print the version (`-V`). |

`dcd gc --all` also offers tags on the host that dcd never recorded, after asking.

Flags: `--config <path>` · `--ssh <target>` · `--env-dir <path>` · `--env-file <path>` ·
`--env-stdin` · `--json` · `--dry-run` · `--resume` · `--image <service>=<ref>` (repeatable) ·
`--set path=value` (repeatable) · `--yes` · `--reason <text>` · `-v/--verbose` ·
`-V/--version`.

## Secrets and environment

Env comes from dotenv files next to `dcd.yaml`, read in this order, with later files winning
and the real process environment beating all of them:

```
.env → .env.local → .env.<stage> → .env.<stage>.local
```

Every key defined there reaches the container as a bare `-e KEY`. The values travel over SSH
on stdin, so no value ever appears in a command line on either machine, and dcd writes no env
file anywhere.

If your project already uses `.env`, move dcd's files onto another base name:

```bash
dcd deploy prod --env-file .env.deploy
```

That reads `.env.deploy` → `.env.deploy.local` → `.env.deploy.<stage>` →
`.env.deploy.<stage>.local` and leaves the app's own files alone.

## When a deploy gets stuck

A deploy that dies **after** the switchover leaves the new container serving traffic and the
release recorded as incomplete. `dcd deploy` then refuses to run (exit 4) until you choose:

```bash
dcd status prod                # what is live, what is incomplete
dcd deploy --resume prod       # finish it: drain the old one, migrations, workers
dcd rollback prod              # go back to the previous release instead
dcd unlock prod                # accept what is live as done, clear the lock
```

Use `unlock` only when resuming keeps failing, or a killed deploy left the stage locked. It
marks the incomplete release as current and removes the lock **even if another dcd still holds
it**. That is all it does:

- It starts, stops and removes nothing. No migrations, no workers, no hooks — so nothing that
  already failed can block it.
- It names every leftover it sees. The next `dcd deploy` starts fresh and cleans them up.
- With nothing incomplete, it just clears the lock.

## dcd.yaml

Declares the deploy: which service to switch to, how traffic moves, migrations, drain, how
workers are found, recreate policy, retention, and simple hooks. The containers themselves
live in your compose file.

**The common case needs no Lua.** See the [worked example](docs/examples/roadrunner_app/) —
config and its compose file.

## Lua plugins

For the cases YAML cannot express. List them under `plugins: [plugins/app.lua]`. A plugin
registers tasks and hooks at the top level, and every hook is handed a `ctx`.

### Top-level functions

| Function | What it does |
|----------|--------------|
| `task(name, fn)` | Define a reusable body, to wire into a step by name. `fn` receives `ctx`. |
| `before(step, hook)` | Run `hook` before a step. `hook` is a task name or `function(ctx)`. A step that does not exist is an error at load. |
| `after(step, hook)` | Run `hook` after a step. Same check. |
| `configure(fn)` | Adjust `cfg` once, before the deploy starts. |
| `set(k, v)` / `get(k)` | Scratch variables, the same store as `ctx.set`/`ctx.get`. |
| `cfg` / `state` | The live config and deploy state. Mutable; the same tables as `ctx.cfg`/`ctx.state`. |

These are the steps you can hook with `before_`/`after_`, in the order they run. Anything
else is an error at load.

| Step | What happens |
|------|--------------|
| `sync` | Copy the compose files to the server, plus the file that pins the image refs. |
| `preflight` | Check the networks exist, create the `directories:` entries, remove containers left by earlier deploys. |
| `ensure_upstream` | Point the router at the fallback if the current container is dead or the upstream file is missing. Runs before `infra` so a restarted router never points at a container that is gone. |
| `pull` | Record every image tag in the state file, then pull. Recording first means a failed deploy's images can still be cleaned up later. |
| `infra` | Start or recreate the supporting services — database, router, cache — and wait for their health checks. |
| `migrate:before` | Run the pre-deploy migration in a throwaway container. Skipped if you set none. |
| `start:black` | Create the new container and apply its restart policy. |
| `healthcheck` | Poll the new container until its health check passes. If it never does, remove it and leave the old one serving. |
| `cutover` | Write the new container into the upstream file and reload the router. **Past this point there is no automatic way back.** |
| `drain:red` | Drain and remove the old app container, then the old workers. |
| `migrate:after` | Run the post-deploy migration inside the new container. Make it idempotent — `--resume` can run it again. |
| `workers` | Work out the worker names, then start one container per name. |
| `finalize` | Mark the release active, save the state file, and remove images past the retention counts. |

`configure` is not a step — register it with `configure(fn)`. A `task()` is not one either;
wire it into a step with `after('cutover', 'my_task')`.

### ctx — doing things

These go through the engine, so they are safe in `--dry-run` and show up there.

| Call | Returns | What it does |
|------|---------|--------------|
| `ctx.run(cmd)` | stdout | Run a shell command on the server, in `deploy_root`. |
| `ctx.in_release(cmd)` | stdout | Run it inside the new app container. |
| `ctx.exec_in(svc, cmd)` | stdout | Run it inside a managed service, e.g. `'nginx'`. |
| `ctx.docker({args})` | stdout | Raw `docker …`. |
| `ctx.compose({args})` | stdout | `docker compose …`, project and files already wired. |
| `ctx.cp_from_release(src, dst)` | — | Copy a file out of the new container. |
| `ctx.cp_to_release(src, dst)` | — | Copy a file into the new container. |
| `ctx.read_file(path)` | string | Read a file, relative to `deploy_root`. |
| `ctx.write_file(path, s)` | — | Write a file. Skipped in `--dry-run`. |
| `ctx.file_exists(path)` | bool | |
| `ctx.env(name)` | string or nil | Read the resolved environment. Process env wins over the dotenv chain. |

### ctx — helpers

| Call | Returns | What it does |
|------|---------|--------------|
| `ctx.json_decode(s)` / `ctx.json_encode(v)` | value / string | JSON. |
| `ctx.yaml_decode(s)` / `ctx.yaml_encode(v)` | value / string | YAML. |
| `ctx.log(msg)` / `ctx.warn(msg)` | — | Print to the deploy output. |
| `ctx.inspect(v)` | string | Pretty-print a value as YAML. |
| `ctx.dump(v)` | — | Log `v` as YAML. With no argument, logs `cfg` and `state`. |

### ctx — data

`cfg` and `state` are live. Assign to them with plain Lua
(`ctx.cfg.retention.keep_releases = 5`) and the engine picks the change up before the next
step. There is no setter function. Config changes affect steps that have not run yet; state
changes affect what gets saved. A few fields are fixed when the deploy starts and are
snapshots: `images`, the container name, `deploy_root`.

| Field | What it is | Mutable |
|-------|------------|---------|
| `ctx.cfg` | The parsed config. | Yes — affects later steps: retention, healthcheck, cutover, drain, workers. |
| `ctx.state` | The current stage: `{ current, releases = [ { id, container, status, images, ran_migrations, reason } ] }`. | Yes — read back into deploy state, saved once past the switchover. |
| `ctx.vars` | Scratch table shared by every hook in a run. | Yes. Not part of cfg or state. |
| `ctx.set(k, v)` / `ctx.get(k)` | The same scratch store, by key. | — |
| `ctx.container` | The new container's name. | No. |
| `ctx.stage` | The stage, e.g. `'prod'`. | No. |

> `ctx.state` is full power. You can rewrite `releases`, `current` and `status`, and you can
> break rollback and resume. The engine trusts whatever you write.

> `os.execute`, `io.open` and `io.popen` are blocked. Use `ctx.run(...)` so the action shows up
> in `--dry-run`. `ctx.run('jq …')` and `ctx.run('bash script.sh')` are fine.

### Examples

```lua
-- keep more releases on a canary deploy, before anything runs
configure(function(ctx)
  if ctx.env('CANARY') == '1' then ctx.cfg.retention.keep_releases = 5 end
end)

-- generate a config file from the running app, once it is healthy
task('centrifugo', function(ctx)
  ctx.in_release('php bin/console app:realtime:config --output=/tmp/c.json')
  ctx.cp_from_release('/tmp/c.json', '.docker/centrifugo/config.json')
  ctx.compose({'up', '-d', 'centrifugo'})
end)
after('healthcheck', 'centrifugo')

-- print cfg and state to the deploy log
after('cutover', function(ctx) ctx.dump() end)
```

## Container images

Every release publishes `linux/amd64` and `linux/arm64` images on GHCR, in a Debian-slim and
an Alpine flavour. The binary is statically linked, so either flavour can be copied into the
CI image that runs your deploy — dcd runs there, not on the server:

```dockerfile
COPY --from=ghcr.io/fluffydiscord/dcd:2.0.2 /usr/local/bin/dcd /usr/local/bin/dcd
```

**Pin an exact version.** A version tag is immutable and names one release. No `2.0` tag is
published — a partial version can only ever move you onto a build you did not choose.

| Tag | What it points at |
|-----|-------------------|
| `2.0.2` | Debian-slim, release `v2.0.2` |
| `2.0.2-alpine` | Alpine, the same release |
| `latest` | Debian-slim, the current tip of `master` |
| `latest-alpine` | Alpine, the same build |

Publish a release by tagging it:

```bash
git tag v2.0.2 && git push origin v2.0.2
```

> **`latest` is not a release. Do not deploy it.** It is rebuilt on every push to `master`, so
> it is whatever the branch happens to be, and it can change between two runs of the same
> pipeline. Use it to try the newest work. In production, use a version tag.

## Build and test

```bash
cargo test                                                       # unit + integration, no Docker
cargo test --test e2e -- --test-threads=1 --include-ignored      # real Docker
cargo test --test e2e_ssh -- --test-threads=1 --include-ignored  # a full deploy over ssh
cargo build --release --target x86_64-unknown-linux-musl         # static binary
```

## Docs

- [Upgrade notes](UPGRADE.md) — what changed in each version, and what you have to do.
- [Implementation spec](docs/implementation-spec.md) — the buildable contract.
- [Strategic blueprint](docs/strategic-blueprint.md) — decisions and ADRs.
- [RoadRunner example](docs/examples/roadrunner_app/) — a full deploy script translated to
  `dcd.yaml`, with CI wiring.
