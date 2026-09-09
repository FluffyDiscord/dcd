# Upgrade notes

Each section lists what changed and what you have to do. If a section says "nothing to do",
upgrade and move on.

## 2.0.1 → 2.0.2

Worker containers now carry the project name.

| Before | After |
|----|----|
| `worker-async` | `acme-worker-async` |

The new default is `{project}-{workers.service}-`. It matches how dcd already names the
release container, and how compose names everything else.

**Nothing to do.** dcd finds workers by their compose service label, not by their name, so
the next deploy removes the old containers and starts the new ones.

- Want the old names? Pin them: `workers.name_prefix: 'worker-'`.
- `dcd check` can now fail with *overlaps release container prefix*. This happens when one
  prefix starts with the other — `release.service: app` together with
  `workers.service: app-worker` gives `acme-app` and `acme-app-worker-`. Docker's
  `--filter name=` matches anywhere in a name, so dcd's two cleanup passes would delete each
  other's containers. Set a `workers.name_prefix` that does not overlap.
- Coming from v1? The one-time v1 worker cleanup further down now also looks for the old
  `worker-` names, so old consumers cannot keep running beside the new ones.

## 2.0.0 → 2.0.1

Four kinds of hook used to pass `dcd check` and then never run. They are now errors when the
config loads. None of them ever fired on any deploy, so the error is the first you hear of a
dead hook, not a new problem. Fix the name or delete the block — every message lists the
names that work.

| Now refused | Why it never ran |
|----|----|
| `hooks: { after_finalise: … }`, or any key that is not `before_`/`after_` plus a real step | dcd looks slots up by name. A key nothing looks up is never read. |
| `hooks: { configure: … }` | `configure` is Lua-only, registered with `configure(fn)`. dcd never read it from the YAML `hooks:` map. |
| `after('my_task', fn)`, where `my_task` came from `task()` | A task is a body you wire into a step — `after('cutover', 'my_task')` — not a step you can hook. |
| `after('cutover', 'no_such_task')` | It used to fail while the deploy ran, which for an `after_cutover` hook is past the point of no return. Now checked once plugins have loaded. |

Configs whose hooks were already spelled right are unaffected. `dcd schema` now covers the
`hooks` keys too, so your editor flags the same typos as you type.

## 0.5.x → v2

v2 does not read a v1 `dcd.yaml`. There is no automatic conversion, but every removed key
gives you an error naming its replacement — so run `dcd check` and work down the list.

Two big changes:

- **dcd runs on your machine now** and drives the server over SSH. Delete the
  `scp dcd dcd.yaml … && ssh server "cd … && ./dcd deploy"` wrapper. Run `dcd deploy prod`
  from your checkout.
- **Your compose file owns the containers** — image, env, volumes, restart policy, network
  alias, entrypoint, command. dcd creates the release container with `docker compose run`
  against your own service.

### Key by key

| v1 | v2 |
|----|----|
| *(nothing)* | `ssh: deploy@host`. Leave it out to keep using a local Docker socket. |
| `docker.images.<name>: <tag>` | Delete. Images come from the compose file. Pin one for a single run with `--image <service>=<ref>`. |
| `docker.services.<name>.image` / `.container` | Delete. The compose service decides both. |
| `docker.services.<name>.recreate` / `.on_recreate_drain_workers` / `.wait` | Move to top-level `services.<name>`. |
| `network: x` | Delete. Networks come from compose; dcd no longer creates them. |
| `release.image: app` | `release.service: app`, a compose service. |
| `release.run.*` | Delete and declare it on the compose service. `release.run` survives only for projects with no compose service for the app, and cannot be combined with `service:`. |
| `release.healthcheck` | Optional now. The compose service's own `healthcheck:` is the default. Keep it when the image cannot check itself. `exec_in` now takes a service name. |
| `cutover.reload.exec_in: my-nginx` | Now a service name. `cutover.service:` is required. |
| `workers.template.*` | Delete and declare a worker compose service instead. |
| `workers.compose_file` | Delete. dcd writes no workers file; every worker comes from one service. |
| `workers.name_filter` | `workers.name_prefix`. It only names containers — dcd finds them by compose service label. |
| *(nothing)* | `workers.service`, required, and it must differ from `release.service`. |
| `retention.keep_images` keyed by image | Keyed by compose service name. |
| `plugins:` relative to `deploy_root` | Relative to the config file's own directory. They run on your machine and are never uploaded. |

### Before your first v2 deploy

- **Give every managed service a health check.** The release service and everything under
  `services:` needs a compose `healthcheck:` or a `wait:` probe. Missing one is now an error:
  `compose up --wait` returns as soon as a container without a check is merely running.
- **Add `profiles: ["dcd-release"]`** to the app and worker services. Optional, but without it
  a hand-run `docker compose up` starts a second app container next to the release.
- **Mount `{deploy_root}/{cutover.upstream_file}` into the router.** dcd writes the file; only
  your compose file can put it inside the container.
- **Create anything compose refers to by relative path** on the server first — bind-mount
  sources, `env_file:` targets, build contexts. dcd uploads none of them. `dcd check` warns
  about bind-mount sources only; check the other two yourself.
- **Install the tools.** Your machine needs `docker` with the compose plugin, plus `ssh`. The
  server needs `docker`, `sshd`, a POSIX shell, `flock` and `base64` (busybox covers the last
  two). `preflight` tells you what is missing.

### v1 workers are cleaned up once

v1 made one compose service per worker, so those containers are labelled
`com.docker.compose.service=worker-async` — which v2's `service=<workers.service>` filter
never matches. Left alone they are never drained, and the first v2 deploy hits a name clash
after cutover.

So while a stage has no v2 release recorded, `preflight` removes every container whose name
starts with `workers.name_prefix` — or with the plain `worker-` that v1 used, since 2.0.2
made the default include the project — and whose compose service label is not
`workers.service`. It names each one as it goes. Once a stage has a recorded release, live
workers are never touched.

### `latest` is back, as a branch tag

`ghcr.io/fluffydiscord/dcd:latest` and `latest-alpine` are rebuilt on every push to `master`.

That is the tip of the branch, not a release. It is untagged and unreleased, and it can move
between two runs of the same pipeline. Use it to try unreleased work. Never deploy it —
pin a version tag in production. Pushing a version tag no longer moves `latest`, so the 0.5.2
downgrade cannot happen again.

## 0.5.1 → 0.5.3

A bug fix. No configuration change.

**Skip 0.5.2 — it was withdrawn.** Same fix, but it was tagged before `latest` was removed, so
its build published moving tags. Its images are deleted.

- **Pin an exact version**, e.g. `ghcr.io/fluffydiscord/dcd:0.5.3`. `latest`, `latest-alpine`,
  `edge`, `edge-alpine` and the `{{major}}.{{minor}}` tags are gone from the registry. The
  publish workflow now sets `latest=false`, emits only `{{version}}`, and runs on tag pushes
  alone. `latest` used to mark whichever release built last rather than the newest, so pushing
  an old tag silently downgraded everyone. (v2 brings `latest` back on different terms — see
  above.)
- **The fix:** evicting a release removed its container and image but never marked the release
  row as done, so every later deploy re-derived the same long-dead cleanup work. Nothing
  showed it — `docker rm -f` answers 0 for a container that is already gone — it just grew
  with the release history.
- `Release` gains `reaped: bool` in `dcd-state.json`, set once teardown finishes. It defaults
  to `false`, so no migration is needed: an existing state file gets one final cleanup pass on
  the next deploy and then settles.
- History is kept. Evicted releases keep their rows and still show in `dcd status`; only the
  retention pass ignores them.
- A release is marked done only once Docker confirms the container is gone *and* the image has
  settled. An image that was proposed for removal and refused leaves the release open, so it
  stays proposable — that row is the only record that dcd put the image on the host.
- `dcd gc` settles rows too, but only for releases whose container is already gone. `gc`
  removes images, never containers.

## 0.5.0 → 0.5.1

**A default changed in a patch release.** Retention now keeps one previous release instead of
three. Configs that set `retention` explicitly are unaffected.

To keep the old behaviour:

```yaml
retention:
  keep_releases: 3
  keep_managed_images: 2
```

- `keep_releases` went 3 → 1 and `keep_managed_images` 2 → 1. Superseded releases count on top
  of the current one, so a stage now holds 2 release images instead of 4.
- `keep_managed_images` counts tags kept per managed image, newest first, **including the one
  in use**. At 1 you keep the running tag and nothing older.
- Rolling back one release still works. Retention always spares the rollback target, whatever
  `keep_releases` says. Before, redeploying the same tag could push the real target out of the
  keep window and delete its image — which `keep_releases: 1` would have made routine.
- `retention.keep_releases: 0` is rejected by `dcd check`. At 0 there is no local rollback
  target, and `dcd rollback` needs the tag to still be pullable.
- **Upgrading because the disk is full? Run `dcd gc <stage>` first.** A deploy pulls its new
  image at step 3 and only reclaims space at step 12. `dcd gc` reclaims images only — evicted
  containers are cleaned up by the next deploy, and a tag that a stopped container still holds
  is kept.

## 0.4 → 0.5

Image cleanup only ever saw tags from finished deploys, so images pulled by a deploy that
later failed stayed on the host forever. Disks filled up while retention looked healthy.

- Every tag is now recorded in `dcd-state.json` before it is pulled. Old state files work as
  they are; the record starts empty and fills from the next deploy.
- **Tags that no old state file recorded are not reclaimed by deploying** — dcd has no record
  of them. A host that is already full needs one `dcd gc <stage> --all`.
- From then on, images left behind by a failed deploy are reclaimed like any other. Images of
  retained releases are still never removed, and one stage no longer deletes another stage's
  rollback target when they share a `deploy_root`.
- New: `dcd gc [stage]` — retention without deploying, for a host that is already full. It
  removes only what dcd recorded.
- New: `dcd gc [stage] --all` — also offers tags on the host that dcd never recorded, pulled
  by hand or before the upgrade. It lists everything and asks first. `-y` skips the prompt,
  `--dry-run` changes nothing. Docker Hub repositories such as `postgres` and
  `bitnami/postgresql` are skipped — those are not yours to delete.
- New, optional: `retention.keep_images: {<image>: <count>}`, a per-image override of
  `keep_managed_images`. Not allowed for `release.image`, which `keep_releases` already bounds.
- `workers.template.image` is now pulled and retention-bounded like every other image. If you
  build it on the host and never push it, the deploy now fails at `pull`. Push it, or point the
  template at an image dcd already pulls.

## 0.3 → 0.4

**`-v` now means `--verbose`, not `--version`. The version flag is `-V`.**

- `-v/--verbose` traces every command dcd runs: argv, exit code, elapsed time, and the output
  that is otherwise dropped unless the command fails. Under `--json` it adds two event kinds,
  `exec` and `exec_result`/`exec_error`.
- Anything scripted against `dcd -v` for the version string must move to `-V`. `--version` is
  unchanged.
- Env values still never show up in the trace. Chain keys reach containers as a bare `-e KEY`,
  so no argv ever carries a value.

## 0.2 → 0.3

**`--env-stdin` on its own is now the whole chain.** No `.env` is picked up next to
`dcd.yaml` any more. Before, a stdin-only run still absorbed whatever `.env`/`.env.local` sat
in the config directory — which shipped the application's own dotenv, dev credentials
included, into every container.

- Piping secrets on stdin *and* relying on those files? Add `--env-dir .` or
  `--env-file <base>` to keep them. Both still stack a stdin layer on top.
- Run `dcd check <stage>` before and after upgrading and compare the `env: loaded …` lines. A
  smaller key set means you needed the flag.
- Runs using `--env-file` or `--env-dir`, and runs without `--env-stdin`, are unaffected.

## 0.1 → 0.2

Env now comes from a dotenv chain where dcd runs, and reaches containers through the process
environment as `-e KEY`. dcd writes no env file. Details in
`docs/implementation-spec.md` §5.2.

Check what you have installed with `dcd --version` (`-V`).

### Config

- Remove `compose.env_file`. Use the `compose.env` map — it is injected into the process
  environment of every `docker compose` call.
- Put the dotenv chain next to `dcd.yaml` (`--env-dir` overrides the location). Later files
  win, and the real process environment beats every file. Only keys defined in a chain file
  reach containers:

  ```
  .env (or .env.dist) → .env.local → .env.<stage> → .env.<stage>.local → --env-stdin
  ```

- The parser and layering are a port of symfony/dotenv 8.1. `${VAR}` references across layers
  resolve late, so a later layer overriding `REDIS_HOST` also rewrites an earlier
  `redis://${REDIS_HOST}`; forward references work; a self-referencing `${VAR:-default}` sees
  the value from before the chain. Circular references are an error.
- One deliberate difference from upstream: an apostrophe inside a value is a literal
  apostrophe unless its partner is on the same line, so `PASS=pa'ss` parses instead of
  swallowing the following lines. Everything else behaves as upstream — `FOO="a b"`,
  `FOO='a'"$B"`, multi-line quoted values, `'bar '\'' baz'`, and `"` still has to be closed.
- Project has its own `.env`/`.env.<stage>`? Move dcd's whole chain onto another base name
  with `--env-file .env.deploy`, giving `.env.deploy` → `.env.deploy.local` →
  `.env.deploy.<stage>` → `.env.deploy.<stage>.local`. The app's own files are never read.
- Remove `DOCKER_*`, `COMPOSE_*`, `PATH`, `HOME`, `LD_*`, `BUILDX_*` and proxy variables from
  chain files. They are refused.
- `env_include`/`env_exclude` (full-match regexes, exclude wins) on `release.run` and
  `workers.template` filter which keys each container gets.
- `release.run.env_file` is unchanged.

### Compose

- Remove service-level `env_file` and list bare `environment` names instead. Compose no longer
  reads an implicit `.env` — dcd pins `--env-file /dev/null`.

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

- Compose runs with `deploy_root` as its working directory, so relative `-f` paths resolve
  against it.
- Recreate workers with `dcd deploy`. The generated workers compose file holds key names only
  and no longer works with a hand-run `docker compose up`.

### Runtime

- `docker run -e KEY=VALUE` is now `-e KEY`. Export the keys yourself before replaying a
  printed command by hand.
- `dcd check <stage>` prints which chain files loaded and which key names each kind of
  container receives.
- Rollback and resume re-read today's chain and warn when the delivered keys differ from the
  release's `env_keys` in `dcd-state.json`.

### On the server

Delete the leftover env file and rotate the secrets that were in it:

```bash
rm <deploy_root>/compose.env
```
