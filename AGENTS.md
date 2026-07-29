# Helping a developer configure dcd — a guide for AI agents

You are helping a developer wire **dcd** (this tool) into their project: a single static
binary that runs a zero-downtime **red-black** Docker deploy from a `dcd.yaml`. It builds
the new ("black") app container next to the live ("red") one, health-checks it, flips the
nginx upstream over to it, then drains the old one.

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
| `docs/examples/roadrunner_app/dcd.yaml` | a real, lean PHP-app config | copy a production-shaped one |
| `docs/examples/fpm_app/dcd.yaml` | a PHP-FPM app, prod + beta stages | copy a multi-stage FPM setup |
| `docs/examples/all_in_one/dcd.yaml` | **every** field, described, with defaults | look up any option |

Then skim the project you're configuring and answer: what's the **app image**? what
**side containers** does it need (db, router/nginx, cache)? how do you **health-check** the
app? how does traffic get **cut over** (which container runs `nginx -s reload`)? are there
**migrations** or **background workers**?

---

## 2. The schema, at a glance

```yaml
version: 1                 # default: 1 — optional
project: myapp             # optional — defaults to the deploy_root folder name; names state + compose project
network: myapp_net         # optional — defaults to <project>_default (the compose default network)
registry: …                # optional — defaults from $REGISTRY / $CI_REGISTRY_IMAGE
deploy_root: …             # optional — defaults from $DEPLOY_ROOT, else "."

docker:                    # required
  images: { app: <tag>, … }      # logical name -> tag (release/services/workers ref these)
  services: { … }                # long-lived side containers (db, nginx, cache)

compose:                   # required
  files: [docker-compose.yml]    # -f files
  env: { … }                     # injected into every compose call's process env (no file is written)

directories: [ … ]         # optional — host paths to mkdir/chown before deploy
release: { … }             # required — the app: image, container_prefix, healthcheck, run, migrate?, drain?
cutover: { … }             # required — backend_port + the nginx reload
workers: { … }             # optional — background consumers
retention: { … }           # optional — keep_releases (default 3), keep_managed_images (default 2)
plugins: [ … ]             # optional — Lua extension files
hooks: { … }               # optional — zero-Lua before_/after_<step> actions
host: …                    # optional — refuse to run unless `hostname` matches
stages: { prod: {}, … }    # named targets; the chosen one is deep-merged over the above
```

`docs/examples/all_in_one/dcd.yaml` documents every nested field and default. **Show only
what differs from a default** — a good config is short.

---

## 3. Author it — the workflow

1. `dcd init` (in their deploy dir) to drop a skeleton, **or** copy `roadrunner_app/dcd.yaml`.
2. Fill the **required** blocks (see the checklist in §5). Map their real container names,
   image tags, healthcheck URL, and the `nginx -s reload` command.
3. Delete anything that equals a default (timeouts, `recreate: on-image-change`,
   `restart: unless-stopped`, the whole `cutover` upstream/template, worker stop signals,
   `retention`). Look each up in `all_in_one` to confirm the default.
4. Leave `registry` / `deploy_root` out if CI sets `$REGISTRY` / `$DEPLOY_ROOT`.
5. Run the **validation loop** (§4). Iterate until clean.

Minimal skeleton (what `dcd init` writes, trimmed — `dcd check prod` passes with
`REGISTRY`/`APP_TAG` set). Note the **block style** for the `${VAR}` lines:

```yaml
project: myapp
network: myapp_net
docker:
  images:
    app: ${APP_TAG}
  services:
    nginx:
      container: myapp-nginx
      recreate: never
      wait: { exec_in: myapp-nginx, cmd: 'test -f /var/run/nginx.pid' }
compose:
  files: [docker-compose.prod.yml]
  env:
    REGISTRY: ${REGISTRY}
    APP_TAG: ${APP_TAG}
release:
  image: app
  container_prefix: myapp-app
  healthcheck: { exec_in: myapp-nginx, cmd: 'curl -sf http://{container}:8080/health' }
cutover:
  backend_port: 8080
  reload: { exec_in: myapp-nginx, cmd: 'nginx -s reload' }
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

- `check` needs any `${VAR}` resolvable: either export them, or use `${VAR:-default}` in the
  config so it validates standalone. An unresolved `${VAR}` with no default is an error.
- If the config has multiple `stages`, a stage arg is required (e.g. `dcd check prod`).
- `--dry-run` runs read-only probes for real but stubs every mutation, so the printed plan is
  truthful about the current state without changing anything.

---

## 5. Required-fields checklist + what `check` enforces

Required, or `check` fails: `docker.images`, `compose`, `release.image`,
`release.container_prefix`, `release.healthcheck` (`exec_in` + `cmd`), `cutover.backend_port`,
`cutover.reload`. (`project` and `network` are optional — see defaults below.)

**Identity defaults (so a lean, multi-stage config can omit them):**
- `project` defaults to the **`deploy_root` folder name**; `network` to **`<project>_default`**;
  `compose.env.COMPOSE_PROJECT_NAME` to the project. An explicit value always wins.
- The **`{project}` token** is expanded everywhere after defaulting. Because `deploy_root` differs
  per stage, writing container names / `exec_in` / `network_alias` as `{project}-foo` namespaces every
  container and network per stage — prod and beta run side-by-side on one host with no repetition.

Validation rules (all reported by `check` with the offending path):

- Every referenced image (`release.image`, `docker.services.*.image`,
  `workers.template.image`) must be a key in **`docker.images`**.
- Every `exec_in` (`release.healthcheck`, `cutover.reload`, `cutover.validate`,
  `docker.services.*.wait`) must name a **container declared in `docker.services`**.
- `release.healthcheck.cmd` **must contain `{container}`** — the new container's name is
  substituted in. Never target the `network_alias`; it still resolves to the old container.
- `workers.provider` needs **either** `static: [...]` **or** `command_in_release: '...'`.
- Unknown keys are rejected (typo guard) — `services` at the top level is now
  `docker.services`, `images` is `docker.images`.

---

## 6. Gotchas that bite

- **Block style for `${VAR}`.** A value containing `${...}` must be on its own indented line,
  never inside flow `{ a: ${X} }` — flow + `${}` is a YAML parse error.
- **`{container}` not the alias** in the healthcheck (above). The #1 mistake.
- **One app container.** dcd deploys a single black container per release; there is no
  replica/scale knob yet. Multiple instances = multiple `dcd` projects for now.
- **`recreate`** decides side-container churn: `on-image-change` (default), `always`,
  `never`. Use `never` for anything whose image is pinned in compose (it's just waited on).
- **`--image app=<tag>`** is the CI override; it sets `docker.images.app`. `--set
  <path>=<value>` overrides any existing scalar path (a *new* path is an error).
- **Migrations are expand-contract:** `release.migrate.before` runs pre-cutover in a
  throwaway container (additive only); `after` runs in the live container. The throwaway
  inherits the delivered env (dotenv chain + `release.run.env` + `release.run.env_file`),
  so runtime `DATABASE_URL`/secrets reach it.
- **Secrets come from the dotenv chain** (spec §5.2): dcd loads `.env` → `.env.local` →
  `.env.<stage>` → `.env.<stage>.local` from the config file's directory (`--env-dir`
  overrides; `--env-file <base>` rebases the whole chain onto another base name, e.g.
  `.env.deploy[.local|.<stage>|.<stage>.local]`, so it can coexist with the app's own
  `.env` files — the explicit base must exist; `--env-stdin` adds a disk-free top layer),
  real process env wins over every file, and every chain-defined key is delivered to
  app/migrate/worker containers as bare `-e KEY` + process env — never written to disk.
  Cross-layer references resolve deferred (forward references work, a later layer
  overriding a key rewrites earlier references to it, circular references error).
  Filter per container with
  `release.run.env_include`/`env_exclude` and `workers.template.env_include`/`env_exclude`
  (full-match regexes, exclude wins). Commit a secret-free `.env` naming the keys; values
  come from `.env.<stage>.local`, CI-exported env, or stdin.
- **`release.run.env_file`** (optional, operator-managed) still points `docker run
  --env-file` at a host env-file resolved vs `deploy_root`; any chain/`env:` key overrides
  the same key in the file. Prefer the chain — the file is at-rest on the server.
- **Rollback runs no migrations** and re-deploys the previous release's images. Env is
  re-read from **today's** chain — dcd warns when the key set differs from what the
  rolled-back release recorded.

---

## 7. Common errors → fixes

| `dcd check` says… | Cause | Fix |
|-------------------|-------|-----|
| `references image 'X' … not declared in docker.images:` | image name typo / missing | add `X` to `docker.images` or fix the reference |
| `execs in 'X' … not a declared service container` | `exec_in` names a container with no service | add a `docker.services` entry whose `container:` is `X` |
| `healthcheck.cmd must reference {container}` | hardcoded host/alias in the probe | use `http://{container}:<port>/…` |
| `unknown field 'X'` | typo, or pre-`docker:` schema | nest under `docker:` / fix the key |
| `compose.env_file was removed …` | pre-rework config | delete the key; see UPGRADE.md |
| `unresolved … ${VAR}` | var in no chain file and not exported | add it to a chain layer, export it, or write `${VAR:-default}` |
| `X in <file> is reserved (configures dcd's own tooling)` | `DOCKER_*`/`COMPOSE_*`/`PATH`/proxy var in a chain file | remove it; use `release.run.env` if a container truly needs it |
| `env dir <path> does not exist` | bad `--env-dir` | fix the path |
| `env file <path> does not exist (checked <path>.dist too)` | bad `--env-file` base | fix the path or create the base file |
| `Too many levels of variable indirection in env vars: …` | circular `${VAR}` references across chain layers | break the cycle |
| a dotenv parse error with a `^` caret | syntax error in a chain file | fix the named file:line |
| `multiple stages; pass one of: …` | `check`/`deploy` with no stage | add the stage arg |
| a `parse:` error pointing at a line with `${VAR}` | `${}` inside a flow `{ }` map | switch that line to block style |

---

## 8. When YAML isn't enough — Lua plugins

Reach for Lua only when a step needs logic. List files in `plugins: [plugins/app.lua]`; each
registers `task(name, fn)` and `before('step', …)` / `after('step', …)` hooks, or a
`configure(fn)` that adjusts config before the run. Inside a hook, `ctx.cfg` and `ctx.state`
are **live and mutable** — assign to them and the deploy honors it (no setter function).
Effects (`ctx.run`, `ctx.in_release`, `ctx.exec_in`, `ctx.docker`, `ctx.compose`, `ctx.cp_*`)
are dry-run-safe. Full reference: the "Lua plugins" section of `README.md`. Most projects
need none — a `hooks:` block (§2) covers the common "run a command around a step" case.
