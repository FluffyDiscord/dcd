# Upgrade notes

## 0.5.x → v2 (SSH transport, compose-owned containers)

v2 does not read a v1 `dcd.yaml`. No automatic conversion — every removed key errors with its
replacement, so `dcd check` is the checklist.

- dcd runs on the deploying machine, over SSH. Drop the
  `scp dcd dcd.yaml … && ssh server "cd … && ./dcd deploy"` wrapper; run `dcd deploy prod`
  from the checkout.
- Your compose file declares the containers — image, env, volumes, restart policy, network
  alias, entrypoint, command. The release container is created with `docker compose run`
  against your own service.

### Key by key

| v1 | v2 |
|----|----|
| *(nothing)* | `ssh: deploy@host` — omit to keep driving a local Docker socket |
| `docker.images.<name>: <tag>` | delete. Images come from the compose file; pin one per run with `--image <service>=<ref>` |
| `docker.services.<name>.image` / `.container` | delete. Identity comes from the compose service |
| `docker.services.<name>.recreate` / `.on_recreate_drain_workers` / `.wait` | move to top-level `services.<name>` (policy only) |
| `network: x` | delete. Networks are declared in compose; dcd no longer creates them |
| `release.image: app` | `release.service: app` (a compose service) |
| `release.run.*` | delete — declare it on the compose service. Kept as `release.run` **only** for a project with no compose service for the app, where it is mutually exclusive with `service:` |
| `release.healthcheck` | optional now: the compose service's own `healthcheck:` is the default gate. Keep it as the escape hatch when the image cannot self-probe. `exec_in` is now a **service** name |
| `cutover.reload.exec_in: my-nginx` | a **service** name, and `cutover.service:` is now required |
| `workers.template.*` | delete — declare a worker compose service instead |
| `workers.compose_file` | delete. dcd renders no workers file; N containers come from ONE service |
| `workers.name_filter` | `workers.name_prefix` — naming only. Discovery is by compose service label |
| *(nothing)* | `workers.service` — required, and must **not** equal `release.service` |
| `retention.keep_images` keyed by image logical | keyed by **compose service name** |
| `plugins:` relative to `deploy_root` | relative to the **config file's directory**; they run locally and are never uploaded |

### Before the first deploy

- Health gate on every managed service. The release service and every service under
  `services:` needs a compose `healthcheck:` or a `wait:` probe. Now an error:
  `compose up --wait` returns as soon as a gate-less container is *running*.
- `profiles: ["dcd-release"]` on the app and worker services. Optional; without it a hand-run
  `docker compose up` starts a second app container beside the release.
- Mount `{deploy_root}/{cutover.upstream_file}` into the router. dcd writes it; only your
  compose file can put it inside the container.
- Relative paths in compose are not uploaded — bind-mount sources, `env_file:` targets and
  build contexts must already exist on the target. `dcd check` warns for bind-mount sources
  only; check the other two yourself.
- Deploying machine: `docker` with the compose plugin, plus `ssh`. Target: `docker`, `sshd`,
  a POSIX shell, `flock`, `base64` (busybox covers the last two). `preflight` names what is
  missing.

### v1 workers are reaped once

v1 generated one compose *service per worker* (`worker-async`, `worker-sched`), so those
containers carry `com.docker.compose.service=worker-async` — which v2's
`service=<workers.service>` filter never matches. Left alone they are never drained, and the
first v2 deploy collides on the container name after cutover.

While a stage has no v2 release recorded, `preflight` removes every container under
`workers.name_prefix` whose compose service label is not `workers.service`, naming each one.
Once a stage has a recorded release, its live workers are never swept.

## 0.5.1 → 0.5.3

Bug fix, no configuration change. Retention converges instead of repeating itself.

**0.5.2 was withdrawn** — same fix, but tagged before `latest` was removed, so its build
published moving tags. Its images are deleted. Use 0.5.3.

- No moving tags. `latest`, `latest-alpine`, `edge`, `edge-alpine` and the
  `{{major}}.{{minor}}` tags are gone from the registry and can no longer be produced: the
  publish workflow sets `latest=false`, emits only `{{version}}`, and runs on tag pushes
  alone. `latest` marked whichever non-prerelease semver built last rather than the newest,
  so pushing an old tag silently downgraded every consumer. Pin an exact version —
  `ghcr.io/fluffydiscord/dcd:0.5.3`
- Evicting a release removed its container and image but never marked the release row done,
  so `evictions`/`gc_candidates` re-derived the same long-dead work from `releases[]` on
  **every** later deploy. `docker rm -f` answers `0` for a container that is already gone and
  `docker image rm` answers `1 / No such image`, so no exit code ever showed it — it just
  grew with the release history.
- `Release` gains `reaped: bool` in `dcd-state.json`, set once teardown is complete. It
  defaults to `false`, so no migration is needed: an existing state file gets one final
  cleanup pass on the next deploy and settles afterwards.
- History is not deleted. Evicted releases keep their rows and stay visible in `dcd status`;
  only the retention pass ignores them.
- A release is reaped only when Docker confirmed its container gone *and* its image settled.
  An image proposed for removal and refused leaves the release unreaped, so it stays
  proposable — that row is the only evidence dcd put the image on the host (INV-11).
- `dcd gc` settles rows too, but only for releases whose container is already gone: `gc`
  removes images, never containers.

## 0.5.0 → 0.5.1

**Breaking default change, shipped in a patch release.** Retention keeps one previous release
instead of three. Configs that already set `retention` explicitly are unaffected.

Keep the old behaviour:

```yaml
retention:
  keep_releases: 3
  keep_managed_images: 2
```

- `keep_releases` 3 → 1, `keep_managed_images` 2 → 1. Superseded releases count *in addition
  to* the current one, so a stage holds current + 1 = 2 release images, down from current + 3
  = 4.
- `keep_managed_images` counts tags kept per managed image, newest first, **including the one
  in use** — 1 keeps the running tag and no previous version.
- Rollback to the immediately previous release still works. INV-6 is enforced directly:
  retention spares the rollback target whatever `keep_releases` says. Before, a same-tag
  redeploy could put the real target outside the keep window and delete its image, which
  `keep_releases: 1` would have made routine.
- `retention.keep_releases: 0` is rejected at `dcd check` — at 0 there is no local rollback
  target and `dcd rollback` depends on the tag still being pullable.
- **Upgrading because the disk is full? Run `dcd gc <stage>` first.** A deploy pulls its new
  image (step 3) long before it reclaims anything (step 12). `dcd gc` reclaims images only —
  evicted release containers are reaped by the next deploy, and a tag a stopped container
  still pins is kept.

## 0.4 → 0.5

Image GC only ever saw tags from finished deploys, so images pulled by a deploy that later
failed stayed on the host forever. Disks filled up while retention looked fine.

- Every tag is recorded in `dcd-state.json` before it is pulled. Old state files work as-is;
  the record starts empty and fills from the next deploy.
- Tags no old state file recorded are NOT reclaimed by deploying — dcd has no record of them.
  A host that is already full needs one `dcd gc <stage> --all`.
- From then on, images left by a failed deploy are reclaimed like any other. Images of
  retained releases are still never removed, and a stage no longer removes another stage's
  rollback target (stages sharing one `deploy_root`, i.e. one state file).
- New `dcd gc [stage]` — retention without deploying, for a host that is already full.
  Removes only what dcd recorded.
- New `dcd gc [stage] --all` — also offers tags on the host that dcd never recorded (pulled
  by hand, or before the upgrade). Lists everything and asks first; `-y` skips the prompt,
  `--dry-run` changes nothing. Skips Docker Hub repositories (`postgres`,
  `bitnami/postgresql`) — those are not yours to delete.
- New optional `retention.keep_images: {<image>: <count>}` — per-image override of
  `keep_managed_images`. Rejected for `release.image`; `keep_releases` bounds that.
- `workers.template.image` is now pulled and retention-bounded like every other image. Built
  on the host and never pushed → the deploy now fails at `pull`. Push it, or point the
  template at an image dcd already pulls.

## 0.3 → 0.4

`-v` is `--verbose`, not `--version`. The version short flag is now `-V`.

- `-v/--verbose` traces every command dcd spawns — argv, exit code, elapsed, and the
  stdout/stderr that is otherwise captured and dropped unless the command fails. Under
  `--json` it is two extra event kinds (`exec`, `exec_result`/`exec_error`).
- Anything scripted against `dcd -v` for the version string must move to `-V`; `--version` is
  unchanged.
- Env values still never appear in the trace: chain keys reach containers as a bare `-e KEY`,
  so no argv carries a value.

## 0.2 → 0.3

`--env-stdin` on its own is now the WHOLE chain: no `.env` is discovered next to `dcd.yaml`.
Before, a stdin-only run still absorbed an implicitly discovered `.env`/`.env.local` from the
config directory — shipping an application's own dotenv, dev credentials included, into every
container.

- Piped secrets on stdin *and* relied on those implicit file layers? Add `--env-dir .` (or
  `--env-file <base>`) to keep them — both still stack a stdin layer on top.
- Run `dcd check <stage>` and compare the `env: loaded …` lines before and after upgrading; a
  shrunken key set means you needed the flag.
- `--env-file` and `--env-dir` runs are unaffected, as are runs without `--env-stdin`.

## 0.1 → 0.2

Env is loaded from a dotenv chain where dcd runs and passed through the process environment
(`-e KEY`). dcd writes no env file. See `docs/implementation-spec.md` §5.2.

Check the installed binary with `dcd --version` (`-V`).

### Config

- Remove `compose.env_file`; use the `compose.env` map — it is injected into the process
  environment of every `docker compose` call.
- Add the dotenv chain next to `dcd.yaml` (`--env-dir` overrides). Later wins, and the real
  process environment wins over every file. Only keys defined in a chain file are delivered
  to containers:

  ```
  .env (or .env.dist) → .env.local → .env.<stage> → .env.<stage>.local → --env-stdin
  ```

- The parser and layering are a port of symfony/dotenv 8.1: cross-layer `${VAR}` references
  resolve deferred (a later layer overriding `REDIS_HOST` rewrites an earlier
  `redis://${REDIS_HOST}`, forward references work, self-referencing `${VAR:-default}` sees
  the pre-chain value), circular references are an error.
- One deliberate divergence from upstream: an apostrophe *inside* a value is a literal
  apostrophe unless its partner is on the same line, so `PASS=pa'ss` parses instead of opening
  a run that swallows the following lines. Everything else is unchanged — `FOO="a b"`,
  `FOO='a'"$B"`, multi-line quoted values, `'bar '\'' baz'`, and `"` still has to be closed.
- Project has its own `.env`/`.env.<stage>`? Rebase the whole chain onto a separate base with
  `--env-file .env.deploy` (`.env.deploy` → `.env.deploy.local` → `.env.deploy.<stage>` →
  `.env.deploy.<stage>.local`); the app's files are never read.
- Remove `DOCKER_*`, `COMPOSE_*`, `PATH`, `HOME`, `LD_*`, `BUILDX_*` and proxy vars from chain
  files — they are refused.
- Add `env_include`/`env_exclude` (full-match regexes, exclude wins) to `release.run` and
  `workers.template` to filter per container.
- `release.run.env_file` is unchanged.

### Compose

- Remove service-level `env_file`; use bare `environment` names. Compose no longer reads an
  implicit `.env` (`--env-file /dev/null` is pinned).

  ```yaml
  # before
  services:
    app:
      env_file: compose.env

  # after
  services:
    app:
      environment:
        - DATABASE_URL
        - APP_SECRET
  ```

- Compose runs with `deploy_root` as its working directory; relative `-f` paths resolve
  against it.
- Recreate workers with `dcd deploy`. The generated workers compose file holds key names only
  and no longer works with a hand-run `docker compose up`.

### Runtime

- `docker run -e KEY=VALUE` becomes `-e KEY`. Export the keys before replaying a printed
  command by hand.
- `dcd check <stage>` prints the loaded chain files and the key names each container class
  receives.
- Rollback and resume re-read today's chain and warn when the delivered key set differs from
  the release's `env_keys` in `dcd-state.json`.

### Server

Remove the leftover env file and rotate its secrets:

```bash
rm <deploy_root>/compose.env
```
