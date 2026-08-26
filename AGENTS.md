# Helping a developer configure dcd — a guide for AI agents

You are helping a developer wire **dcd** (this tool) into their project: a single static
binary that runs a zero-downtime **red-black** Docker deploy from a `dcd.yaml`. It creates
the new ("black") app container next to the live ("red") one, waits for its health gate,
flips the router's upstream over to it, then drains the old one.

**dcd runs on the deploying machine** — a CI runner or a workstation — and reaches the
server over SSH. Nothing is installed there.

**The organising rule: the compose file declares CONTAINERS; `dcd.yaml` declares
ORCHESTRATION.** Images, environment, volumes, restart policies, network aliases,
entrypoints and healthchecks belong in `docker-compose.yml`, where the developer was
already writing them. `dcd.yaml` carries only what compose cannot express.

Your job has two modes — **author** a `dcd.yaml` with the developer, and **validate** it.
Do both with the commands below; never hand over a config you haven't run `dcd check` on.

> Keep this guide accurate: it, `docs/examples/all_in_one/dcd.yaml`, and the CLI `--help`
> text must match the real schema in `src/config/`. If you change one, change all (see
> CLAUDE.md). When a fact here disagrees with the code, the code wins — fix the guide.

---

## 1. Orient yourself first

Reference configs ship with the repo — read them before writing anything:

| File | What it is | Use it to… |
|------|------------|-----------|
| output of `dcd init` | the smallest runnable skeleton | start a brand-new config |
| `dcd init --from-compose <file>` | a config derived from an existing compose file | skip most of the typing |
| `docs/examples/roadrunner_app/` | a real, lean config **plus its compose file** | copy a production-shaped pair |
| `docs/examples/fpm_app/` | a multi-stage setup, prod + beta | copy a multi-stage one |
| `docs/examples/all_in_one/dcd.yaml` | **every** field, described, with defaults | look up any option |

Then read the project's **compose file first** — it already answers most of the questions.
What is left to ask: which service is the **app** (the one traffic is cut over to)? which is
the **router** that reloads its upstream? are there **migrations** or **background workers**?
does the app image contain a probe binary, or must the health check run from the router?

---

## 2. The schema, at a glance

```yaml
version: 2                 # required — v1 configs are rejected with a migration hint
project: myapp             # optional — defaults to the deploy_root folder name

ssh: deploy@prod.example   # a target, or an ~/.ssh/config Host alias. OMIT to run against a
                           # local Docker socket. Override per run with --ssh
deploy_root: /srv/myapp    # absolute path ON THE TARGET — default: $DEPLOY_ROOT, else "."
host: prod.example         # optional guard: refuse to run unless the TARGET reports this name

compose:
  files: [docker-compose.prod.yml]   # required; a stage APPENDS more. Uploaded every deploy
  profiles: [dcd-release]            # enabled when RESOLVING the model — default: [dcd-release]
  env: {}                            # optional override; the whole dotenv chain is passed anyway

directories:               # optional — created on the TARGET; relative, no ".."
  - { path: .docker/logs, owner: '1000:1000' }

release:
  service: app             # THE compose service to deploy (or `run:` — see §6)
  container_prefix: myapp-app        # optional — default {project}-{service}
  healthcheck:             # OPTIONAL if the compose service declares its own `healthcheck:`
    exec_in: nginx         #   a compose SERVICE name
    cmd: 'curl -sf http://{container}:8080/health'   # {container} is mandatory here
  migrate: { before: '...', after: '...' }           # optional
  drain: '...'                                       # optional

cutover:
  service: nginx           # the router, as a compose service
  backend_port: 8080
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }

services:                  # POLICY ONLY — identity comes from compose. Optional.
  postgres: { on_recreate_drain_workers: true }
  cache: { recreate: never, wait: { cmd: 'redis-cli ping' } }

workers:                   # optional
  service: worker          # ONE compose service; must NOT be release.service
  provider: { command_in_release: '...' }   # or { static: [a, b] }

retention: { keep_releases: 1, keep_managed_images: 1, keep_images: {} }
plugins: [plugins/app.lua] # resolved against THIS FILE's directory; run locally
hooks: {}
stages:
  prod: {}
```

---

## 3. Author it — the workflow

1. **Read their compose file.** It names the services, images, volumes and networks; you
   are only deciding which service plays which role.
2. `dcd init --from-compose docker-compose.prod.yml`, **or** copy `roadrunner_app/dcd.yaml`.
3. **Give the app and worker services a health gate and a profile** in the *compose* file:
   ```yaml
   services:
     app:
       profiles: ["dcd-release"]      # so a hand-run `docker compose up` starts no second copy
       healthcheck: { test: ["CMD", "curl", "-sf", "http://localhost:8080/health"] }
   ```
   A health gate is **mandatory** for the release and for every service under `services:`.
   If the image has no probe binary, use the `release.healthcheck` escape hatch instead.
4. Fill the required blocks (§5): `release.service`, `cutover.service` + `reload`,
   `compose.files`.
5. Delete anything that equals a default (timeouts, `recreate: on-image-change`, the whole
   `cutover` upstream/template, worker stop signals, `retention`). Look each up in
   `all_in_one` to confirm.
6. Leave `deploy_root` out if CI sets `$DEPLOY_ROOT`.
7. Run the **validation loop** (§4). Iterate until clean.

Minimal skeleton — the compose file does the heavy lifting:

```yaml
version: 2
project: myapp
ssh: ${DEPLOY_SSH}
deploy_root: ${DEPLOY_ROOT}
compose:
  files: [docker-compose.prod.yml]
release:
  service: app
cutover:
  service: nginx
  backend_port: 8080
  reload: { exec_in: nginx, cmd: 'nginx -s reload' }
stages:
  prod: {}
```

---

## 4. Validate it — the loop (this is the "validator" job)

Run these in order; fix and repeat until the first is clean:

```bash
dcd check <stage>                 # parse + merge + interpolate + validate. MUST pass.
dcd tasks <stage>                 # the resolved step plan + where hooks fire — sanity check it
dcd deploy <stage> --dry-run      # the exact docker/compose argv, nothing executed
```

- `check` resolves the **compose model locally** (`docker compose config`), so the deploying
  machine needs the docker CLI with the compose plugin, and the compose files must be in the
  checkout. That resolution is what proves every service reference and every health gate.
- `check` needs any `${VAR}` resolvable: either export them, or use `${VAR:-default}` in the
  config so it validates standalone. An unresolved `${VAR}` with no default is an error.
- `check` opens no ssh connection and touches nothing on the target.
- If the config has multiple `stages`, a stage arg is required (e.g. `dcd check prod`).
- `--dry-run` runs read-only probes for real but stubs every mutation, so the printed plan is
  truthful about the current state without changing anything.
- Add `-v` to any of these when a step behaves unexpectedly: it traces each command dcd runs
  with its exit code and the output that is otherwise only shown on failure.

---

## 5. Required-fields checklist + what `check` enforces

Required, or `check` fails: `version: 2`, `compose.files`, `release.service` (or
`release.run`), `cutover.service`, `cutover.backend_port`, `cutover.reload`.
`project` is optional — it defaults to the `deploy_root` folder name, and the `{project}`
token is expanded everywhere afterwards, so `{project}-foo` namespaces per stage.

**Static rules** (checked when the config loads):

- Exactly **one** of `release.service` / `release.run`.
- `workers.service` **must not equal** `release.service`. Worker discovery is by compose
  service label, so sharing it would make worker drain stop the container serving traffic.
- `workers.name_prefix` must not overlap `release.container_prefix` — `docker ps --filter
  name=` is an unanchored match, so overlapping prefixes make the reapers sweep each other.
- `release.healthcheck.cmd` **must contain `{container}`**. Never target a network alias:
  red and black share the service's aliases, so an alias resolves to red and passes falsely.
- `directories[].path` must be **relative** and contain no `..` — it is interpolated into a
  root-equivalent `chown`.
- `retention.keep_images` keys are **compose service names**; `release.service` is rejected
  there — `keep_releases` bounds it.
- `workers.provider` needs **either** `static: [...]` **or** `command_in_release: '...'`.
- Unknown keys are rejected, and every **v1** key names its v2 replacement rather than
  surfacing as "unknown field".

**Model rules** (checked against the resolved compose file, before anything is touched):

- Every service named by `release.service`, `cutover.service`, `workers.service`,
  `services:` and every `exec_in` **must exist in the compose file**. The error lists the
  services that do exist.
- **Every service under `services:`, and the release service, must declare a health gate** —
  its own compose `healthcheck:`, or a `wait:` probe (`release.healthcheck` for the release).
  This is an error, not a warning: `compose up --wait` returns as soon as a container is
  *running* when it declares no healthcheck, so a missing gate would let dcd cut over to a
  database that is not ready.

---

## 6. Gotchas that bite

- **The compose file is half the config.** If a container option is missing, it belongs in
  compose, not here. There is no `image:`, `volumes:`, `restart:` or `network_alias:` in
  `dcd.yaml` any more.
- **Give the release and worker services `profiles: ["dcd-release"]`.** Not required, but
  without it a hand-run `docker compose up` starts a second app container beside the one
  dcd is deploying. dcd's own compose calls are always service-qualified, so it never does.
- **Health gates are mandatory** (§5). The commonest fix is adding a `healthcheck:` to the
  compose service; the fallback is `release.healthcheck` probing from the router.
- **`{container}` not an alias** in the escape-hatch healthcheck. The #1 mistake.
- **Block style for `${VAR}`.** A value containing `${...}` must be on its own indented line,
  never inside flow `{ a: ${X} }` — flow + `${}` is a YAML parse error.
- **dcd uploads compose *documents*, not what they reference.** A relative bind-mount
  source, an `env_file:`, or a `build.context` must already exist on the target. Docker
  silently creates an empty directory for a missing bind source, so a router whose config
  never arrived starts cleanly and serves nothing. `check` warns, naming each path.
- **The router must bind-mount the upstream file.** dcd writes
  `{deploy_root}/{cutover.upstream_file}` on the target; only the compose file can put it
  inside the router container.
- **One app container.** dcd deploys a single black container per release; there is no
  replica/scale knob yet.
- **`recreate`** decides side-container churn: `on-image-change` (default), `always`,
  `never`. Use `never` for anything whose image is pinned in compose.
- **`--image <service>=<ref>`** is the CI override; it pins that service's image in a
  generated override file, which is also how rollback replays an exact image. `--set
  <path>=<value>` overrides any existing scalar path (a *new* path is an error).
- **Migrations are expand-contract:** `before` runs pre-cutover in a throwaway container
  from the same compose service (its first word becomes the entrypoint, so the service's
  own entrypoint does not swallow it); `after` runs in the live container.
- **Secrets come from the dotenv chain:** `.env` → `.env.local` → `.env.<stage>` →
  `.env.<stage>.local`, read from the config file's directory (`--env-dir` overrides;
  `--env-file <base>` rebases the whole chain; `--env-stdin` adds a disk-free top layer and
  on its own is the whole chain). Real process env wins over every file. Every chain-defined
  key is delivered as a bare `-e KEY`, and over SSH the values ride a document on **stdin** —
  so no value ever appears in an argv, on either machine, and nothing is written to disk.
  Filter with `release.run.env_include`/`env_exclude` when using the `run` fallback.
- **Rollback runs no migrations.** It replays the previous release's *images* (pinned
  exactly via the override file) under the *current* compose spec — the same semantics side
  services have always had. Env is re-read from today's chain, and dcd warns when the key
  set differs from what the rolled-back release recorded.
- **Plugins run locally**, in dcd's own process. Paths resolve against the config file's
  directory; nothing on the target ever reads a `.lua` file.

---

## 7. Common errors → fixes

| `dcd check` says… | Cause | Fix |
|-------------------|-------|-----|
| `docker was removed — container identity now comes from your compose file` | a v1 config | move images/services into compose; keep policy under `services:` (UPGRADE.md) |
| `network was removed — networks come from your compose file` | a v1 config | delete the key; compose declares and creates the network |
| `release.image was removed` / `workers.template was removed` / `workers.compose_file was removed` / `workers.name_filter was removed` | a v1 config | use `release.service:` / `workers.service:` / `workers.name_prefix:` |
| `release: declare service: … or run: …, not both` | both creation paths given | keep one |
| `release.service 'X' is not a service in the compose files (known: …)` | typo, or the service sits behind a profile | fix the name, or add its profile to `compose.profiles` |
| `still scaffolded — fill in: …` | a `dcd init --from-compose` config with `TODO:` values left | fill in each field the message names |
| `--image 'X' is not a service in the compose files (known: …)` | `--image` names a v1 logical alias, or a typo | use the **compose service** name |
| `--image applies to \`deploy\` and \`check\` only` | a pin on `gc`/`rollback`/`status`/`unlock` | drop it — rollback replays the image it recorded |
| `cutover.backend_port is unset` | left at `0` (what the scaffold writes when the release publishes no ports) | name the port the router should send traffic to |
| `the ssh ControlPath expands to N bytes` | a long `XDG_RUNTIME_DIR`; `%C` adds 38 bytes over the template | point `XDG_RUNTIME_DIR` at a shorter directory |
| `service 'X' has no health gate` | a managed service with no `healthcheck:` | add one to the compose service, or give it a `wait:` probe |
| `release service 'X' has no health gate` | the release has no gate at all | add `healthcheck:` to the compose service, or set `release.healthcheck` |
| `workers.service 'X' must not equal release.service` | one service used for both | declare a second compose service (same image is fine) |
| `workers.name_prefix 'X' overlaps release container prefix` | prefixes collide | rename one; `--filter name=` is an unanchored match |
| `release.healthcheck.cmd must reference {container}` | hardcoded host/alias in the probe | use `http://{container}:<port>/…` |
| `directories path 'X' must be relative` / `must not contain '..'` | escaping path | make it relative to `deploy_root` |
| `retention.keep_images cannot set 'X': it is release.service` | per-service count on the release | remove it; tune `keep_releases` |
| `retention.keep_releases must be at least 1` | `keep_releases: 0` | use 1 or more; 0 leaves no local rollback target |
| `cannot resolve the compose model: …` | a compose file is missing, or its `${VAR}`s are unset | run from the checkout with the same env CI uses |
| `deploy_root <path> does not exist on <target>` | first deploy to a fresh host | create it, or let `sync` create it — never reported as a held lock |
| `another deploy holds <stage>` | a live deploy is heartbeating the lock | wait; the lock releases itself if that deploy dies |
| `flock is required on <target>` | minimal target image | install `util-linux` |
| `not sweeping 'X'` / `no repository of P can be shown` (from `dcd gc --all`) | the repository is a Docker Hub name, not a registry host | expected for public images; set `registry:` to one you own |
| `unknown field 'X'` | typo | fix the key |
| `unresolved … ${VAR}` | var in no chain file and not exported | add it to a chain layer, export it, or write `${VAR:-default}` |
| `X in <file> is reserved (configures dcd's own tooling)` | `DOCKER_*`/`COMPOSE_*`/`PATH`/proxy var in a chain file | remove it |
| `Too many levels of variable indirection in env vars: …` | circular `${VAR}` references | break the cycle |
| a dotenv parse error with a `^` caret | syntax error in a chain file | fix the named file:line |
| `multiple stages; pass one of: …` | `check`/`deploy` with no stage | add the stage arg |
| a `parse:` error pointing at a line with `${VAR}` | `${}` inside a flow `{ }` map | switch that line to block style |

---

## 8. When YAML isn't enough — Lua plugins

Reach for Lua only when a step needs logic. List files in `plugins: [plugins/app.lua]`,
resolved against the **config file's** directory and executed on the **deploying machine**
in dcd's own process — nothing on the target ever reads a `.lua` file. Each file
registers `task(name, fn)` and `before('step', …)` / `after('step', …)` hooks, or a
`configure(fn)` that adjusts config before the run. Inside a hook, `ctx.cfg` and `ctx.state`
are **live and mutable** — assign to them and the deploy honors it (no setter function).
Effects (`ctx.run`, `ctx.in_release`, `ctx.exec_in`, `ctx.docker`, `ctx.compose`, `ctx.cp_*`)
are dry-run-safe and execute on the target; `ctx.exec_in` takes a compose **service** name. Full reference: the "Lua plugins" section of `README.md`. Most projects
need none — a `hooks:` block (§2) covers the common "run a command around a step" case.
