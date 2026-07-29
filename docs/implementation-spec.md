# dcd — Implementation Spec (Implementation)

**Document type:** Implementation
**Status:** Specified (2026-06-11), revised post-gate-review (v2), implemented; §5.2 environment rework specified 2026-07-28 (v3, gate-reviewed), pre-implementation
**Source:** reverse-engineered from an existing production red-black deploy script, its CI deploy stage, and the compose file for the managed side services.
**Strategy & rationale:** [Strategic Blueprint](strategic-blueprint.md)

`dcd` is a single static Rust binary that runs a zero-downtime **red-black** Docker deploy on the target server from a YAML config, extensible via sandboxed embedded-Lua plugins. Strategy/scope/7-Questions live in the Blueprint; this is the buildable HOW. The exact-argv discipline in §7 exists so code generation is mechanical and unit tests assert the precise `docker …` commands.

---

## 1. Glossary & invariants

| Term | Meaning |
|------|---------|
| **Red** | the currently-live release container, serving traffic |
| **Black** | the new release container built alongside red this deploy |
| **Cutover** | the router switch (write upstream file → single `nginx -s reload`); the point of no automatic return is the reload **exit 0** |
| **Release** | one deployed image set; identified by `release_id` (epoch seconds, sampled once at plan start) and the black container name |
| **Serving release** | the release currently receiving traffic: the `cutover_pending` release if one exists, else `current` |
| **Managed service** | a compose-owned, long-lived container (postgres, nginx, valkey, …) recreated only on policy |
| **Worker** | a compose service consuming a queue, regenerated each deploy |
| **Stage** | a named deploy profile (`beta`, `prod`) merged over the shared base config |
| **Chain** | the Symfony-style dotenv layer stack (`.env` → `.env.local` → `.env.<stage>` → `.env.<stage>.local` → `--env-stdin`) resolved at launch (§5.2.1) |
| **Interpolation env / container env** | the two maps derived from the chain + process env (§5.2.2): what `${VAR}` in `dcd.yaml` sees / what containers receive |
| **Recipe** | the one built-in fixed task sequence: `docker-redblack` |

**Load-bearing invariants** (each maps to a test, §10.3):

| # | Invariant |
|---|-----------|
| INV-1 | Red serves continuously until black passes healthcheck **and** the cutover reload returns 0. Any failure **before** that point leaves red serving and removes black. |
| INV-2 | **The point of no automatic return is the cutover reload exit 0.** Failures after it (`drain:red`, `migrate:after`, `workers`) never auto-rollback; black is already live. They stop the run, report, and exit `4`. A brief dual-serving window exists while nginx old workers drain in-flight requests — benign for idempotent HTTP; noted for non-idempotent POST/WS. |
| INV-3 | The black release is **recorded in state at cutover** (status `cutover_pending`) before `drain:red`. `finalize` flips it to `active` and advances `current`. A crash between cutover and finalize therefore leaves a recoverable, recorded live release (recovered by `dcd deploy --resume`). |
| INV-4 | The stage lock is a `flock(2)` advisory lock: the OS releases it on process exit **including SIGKILL**. SIGINT/SIGTERM are caught and run orderly cleanup (pre-cutover: remove black; release lock). A dead-holder lock is reclaimable. |
| INV-5 | Rollback never runs migrations (ADR-005); it re-deploys the previous release's images. |
| INV-6 | Rollback to the **immediately previous** release is available while `keep_releases ≥ 1`; its images are retained (never pruned). Deeper/repeated rollback is bounded by `keep_releases` + registry retention; §4 verifies the target's images exist before acting. |
| INV-8 | If a stage declares `host:` and the machine hostname does not match, `dcd` refuses to act (exit `5`). |
| INV-9 | If `state.current` is set but that container is **not running**, the upstream file is reset to `cutover.fallback_backend` **before** any managed-service recreate, so a recreated nginx never points at a dead container (self-heal; mirrors the original script). |
| INV-10 | **At most one `cutover_pending` release exists at any time.** The cutover append (§7.8) demotes any pre-existing `cutover_pending` → `rolled_back` in the same atomic state write, so a crash during a recovery run can never leave two. `serving` is therefore unambiguous. |

---

## 2. Architecture

### 2.1 Process model

Single-threaded, synchronous orchestration (no async runtime): a sequence of `docker` CLI invocations with retry/timeout loops (`std::process::Command` + `std::thread::sleep`). Keeps the binary small, control flow linear, and `--dry-run` a runner swap. A caught-signal flag (§2.5) is polled between tasks and inside retry loops.

```
parse args ─▶ load+merge config ─▶ resolve stage ─▶ host guard ─▶ sample release_id
   ─▶ acquire stage lock ─▶ load Lua plugins ─▶ build plan (recipe + hooks)
   ─▶ execute plan (each task via Context) ─▶ release lock (RAII) ─▶ report
```

### 2.2 Modules

| Module | Responsibility | Key types |
|--------|----------------|-----------|
| `cli` | clap commands/flags → `Action` | `Cli`, `Command`, `Action` |
| `config` | parse `dcd.yaml`, stage-merge, `${VAR}` interpolation, `--set` overrides, identity defaults (project from deploy_root folder, network `<project>_default`, `{project}` token expansion), validation | `Config`, `Stage`, `RawConfig`, `ConfigError` |
| `dotenv` | Symfony-port parser + chain loader (§5.2): 4-file chain + stdin layer, the two resolved maps, per-container filters, reserved-key guard | `DotenvParser`, `EnvChain`, `ResolvedEnv` |
| `effects` | the testability seam: all side effects behind traits | `CommandRunner`, `FileSystem`, `Clock` |
| `effects::real` | production impls | `SystemRunner`, `SystemFs`, `SystemClock` |
| `effects::record` | recording / read-pass-through impls for tests + `--dry-run` | `RecordingRunner`, `MemoryFs`, `FixedClock` |
| `docker` | typed helpers building docker/compose argv over `CommandRunner`; tags each call read-only or mutating | `Docker`, `Compose` |
| `engine` | fixed ordered recipe + before/after hook slots, expansion, execution | `Engine`, `Task`, `HookSlot`, `Plan`, `Context` |
| `recipe` | the `docker-redblack` recipe: registers tasks from `Config` | `redblack::register` |
| `lua` | sandboxed mlua host: globals + `ctx` userdata, plugin loading | `LuaHost` |
| `state` | `dcd-state.json` read/write, release lifecycle, rollback target, retention | `State`, `Release`, `ReleaseStatus` |
| `lock` | per-stage `flock(2)` lock + stale-holder reclamation (RAII) | `StageLock` |
| `ui` | adaptive reporter (rich TTY / plain / `--json`) over an event stream | `Reporter`, `Event` |
| `signal` | catches SIGINT/SIGTERM → interrupt flag | `Interrupt` |
| `error` | typed error → exit-code mapping | `DcdError`, `ExitCode` |

### 2.3 The effects seam (why this is fully testable)

Every side effect goes through one of three traits; `Context` holds trait objects, never concrete impls.

```rust
enum Access { Read, Mutate }                 // every CommandRunner call is classified
trait CommandRunner {
    fn run(&self, cmd: &Argv, access: Access, opts: RunOpts) -> Result<CmdOutput, RunError>;
}
trait FileSystem {
    fn write(&self, path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()>;
    fn read(&self, path: &Path) -> Result<Vec<u8>>;
    fn exists(&self, path: &Path) -> bool;
    fn remove(&self, path: &Path) -> Result<()>;
    fn create_dir_all(&self, path: &Path) -> Result<()>;
}
trait Clock { fn now_epoch(&self) -> u64; fn now_iso(&self) -> String; }
```

- **Production:** `SystemRunner` spawns real processes — since the §5.2 rework, with `current_dir = deploy_root` and the chain runner env (plus per-command overlays), so the effective command contract is **argv + recorded env**, not argv alone; bare `-e KEY` flags in dry-run output are the manifest of what rides the env.
- **Tests:** `RecordingRunner` (canned outputs keyed by argv prefix; records calls) + `MemoryFs` + `FixedClock` → engine/recipe/Lua exercised with **zero Docker**, asserted against exact recorded argv.
- **`--dry-run`** is *not* a fully-synthetic runner. It is **read-pass-through** (§2.4): `Access::Read` commands execute for real (so current image state, running workers, transport lists are truthful); `Access::Mutate` commands are stubbed (printed as planned actions, return synthetic ok). Points that cannot be resolved without a mutation having happened (e.g. the dynamic worker provider, which execs in the not-yet-started black) are emitted as `⚠ data-dependent` lines, never silently defaulted.

### 2.4 Read vs mutate classification (drives dry-run honesty)

| `Access::Read` (run for real in dry-run) | `Access::Mutate` (stubbed in dry-run) |
|---|---|
| `docker inspect`, `docker ps [-a]`, `docker images`, `docker network inspect`, `docker version` | `docker run`, `docker rm`, `docker stop`, `docker pull`, `docker cp`, `docker image rm`, `docker network create` |
| reads of state/upstream files | `docker exec` that runs a project command (migrate, drain, healthcheck against black), `docker compose up/stop`, the `nginx -s reload`, writes of upstream/workers/state files |

The dynamic worker provider (`exec` in black) is `Mutate`-adjacent: in dry-run there is no black, so it emits `⚠ worker set is dynamic (provider command); not resolvable in dry-run` rather than "0 workers".

### 2.5 Signals, lock, and crash recovery

- **Lock:** `flock(2)` (LOCK_EX|LOCK_NB) on `${deploy_root}/.dcd.{stage}.lock`. The OS releases it on any process exit, including SIGKILL — so it never goes stale from a crash. A sidecar `.dcd.{stage}.lock.meta` (pid, ISO start, stage) is written for the human "who holds it" message; because the flock itself is the source of truth, if `dcd` can acquire the flock any meta present is treated as stale and overwritten. Failure to acquire → exit `3` with the meta details.
- **Signals:** a `signal` handler sets an atomic `Interrupt`. The executor checks it between tasks and inside retry loops. Interrupt **before** cutover → pre-cutover cleanup (remove black) + lock release via normal unwind, exit `130`. Interrupt **during** cutover → finish the in-flight reload-or-restore deterministically (never leave the upstream half-written), then exit. SIGKILL cannot run cleanup; the flock auto-releases and `--resume`/orphan-reaping (§7.1) recover the rest.

### 2.6 Container & release naming

`release_id = clock.now_epoch()`, sampled **exactly once** at plan start and stored on `Context.release.id`; black container = `{release.container_prefix}-{release_id}` (matches today's `acme-app-rr-<epoch>`). Every `{release_id}`/`{container}` reference reads the stored value — never re-samples the clock. Preflight orphan-reaping (§7.1) removes any `{container_prefix}-*` container absent from state, covering same-second/crash leftovers.

---

## 3. Deployment lifecycle

`docker-redblack` is this fixed ordered task list. Each task has `before_<task>`/`after_<task>` hook slots. The cutover boundary (INV-1/2/3) is marked. `--resume` re-enters at `drain:red` against the recorded `cutover_pending` release.

| # | Task | Action (exact commands in §7) | Phase |
|---|------|-------------------------------|-------|
| 1 | `preflight` | ensure network; mkdir+chown dirs; **reap orphan `{prefix}-*` containers** | pre-cutover (red live) |
| 2 | `ensure_upstream` | if `state.current` not running → write `fallback_backend` to upstream file (INV-9) | pre-cutover |
| 3 | `pull` | `docker pull` app + every managed-service image | pre-cutover |
| 4 | `infra` | reconcile managed services **in declared order** (conditional recreate; if a recreated service has `on_recreate_drain_workers`, drain workers first via discovery, set `drained`); then run all `wait` gates | pre-cutover |
| 5 | `migrate:before` | run `release.migrate.before` in a throwaway `docker run --rm` app container | pre-cutover |
| 6 | `start:black` | `docker run -d` the black container | pre-cutover |
| 7 | `healthcheck` | poll `release.healthcheck` against the black **container name** until pass or `retries` exhausted | pre-cutover (last safe point) |
| 8 | *(hook slot)* | e.g. a centrifugo config push — `after_healthcheck` YAML hook | pre-cutover |
| 9 | `cutover` | capture upstream → write black backend → reload; on reload≠0 restore + abort; on reload=0 **record `cutover_pending`** | **← boundary** |
| 10 | `drain:red` | reap running app containers except the new black (`release.drain` + `rm`); drain old workers unless `drained` | post-cutover |
| 11 | `migrate:after` | run `release.migrate.after` via `docker exec` in black | post-cutover |
| 12 | `workers` | regenerate workers compose; scoped `compose up -d worker-…` | post-cutover |
| 13 | `finalize` | flip `cutover_pending`→`active`, advance `current`, apply retention + image GC | post-cutover |

Tasks 5, 11, 12 are **skipped** (logged) when their config is absent.

---

## 4. Rollback & resume lifecycle

### 4.1 `dcd rollback [stage]` (INV-5/6)

| # | Step | Detail |
|---|------|--------|
| 1 | resolve target | serving = `cutover_pending` release if present, else `current`; target = the most recent release **before** serving with status `active`/`superseded`/`rolled_back`. Skip a target whose images equal the serving images (no-op); error if none remains. |
| 2 | verify images | target's app (+ managed) images exist locally or are re-pullable; else error early with the missing tag (INV-6) |
| 3 | confirm | print {from → to, images, age, whether target ran migrations}; require `--yes` when non-interactive |
| 4 | `pull` + `start:black` | spin a fresh black from the **target** images |
| 5 | `healthcheck` → `cutover` | same gates as deploy; record `cutover_pending` at reload=0 |
| 6 | `drain:red` + `workers` | drain the rolled-back-from release; restart workers on the target image |
| 7 | finalize | flip the **fresh black** (recorded `cutover_pending` at step 5) → `active` and set `current` = fresh black; mark the rolled-back-from **serving** release → `rolled_back`; the **target** stays `superseded` (it was only the image source); record a history entry `{from: serving, to: fresh-black, source: target, reason}`. *Same shape as deploy finalize — the serving record is always the fresh black, never the image-source target.* |

**No migrations run** (INV-5). `dcd status` flags releases that ran migrations so the operator knows the schema is ahead of the rolled-back code (expand-contract makes that safe).

### 4.2 `dcd deploy --resume [stage]` (recovers an exit-4 state, INV-3)

If state holds a `cutover_pending` release (a prior deploy died/​failed after cutover), `--resume` re-runs **only** the post-cutover tasks (`drain:red` → `migrate:after` → `workers` → `finalize`) against that recorded live black — it does **not** start a new black or re-cutover. Resume reads `current` and the `cutover_pending` release from the on-disk state; `drain:red` on resume re-checks whether the old `current` container is still running (the failed run likely already removed it) and skips it if gone (§7.10). Because the `drained` flag is per-process, resume re-runs the worker drain (safe: `workers.drain` is best-effort and the `workers` task recreates the set). `migrate:after` re-runs and so must be idempotent (Doctrine version-tracking skips applied migrations). Without `--resume`, a `deploy` that finds a `cutover_pending` release refuses (exit `4`) and tells the operator to `--resume` or `rollback`. As a defensive guard, any run that finds **more than one** `cutover_pending` (which INV-10 forbids) aborts with a clear state-corruption error rather than guessing.

### 4.3 State transitions (authoritative)

`current` = the last finalized release. **`serving`** (the container actually receiving traffic) = the `cutover_pending` release if one exists, else `current`. Exactly one `cutover_pending` exists at any time (INV-10). Each event below is one atomic state write.

| Event | Effect on state |
|-------|-----------------|
| cutover reload=0 (deploy **or** rollback) | append `R_new{ status: cutover_pending }`; **demote any other `cutover_pending` → `rolled_back`** (INV-10) |
| `drain:red` | no state write; drains containers (see §7.10) |
| finalize (deploy) | `R_new → active`; the release that was `current` at run start → `superseded`; `current = R_new` |
| finalize (rollback) | `R_new → active`; the rolled-back-from `serving` → `rolled_back`; the run-start `current` (if different from `serving`) → `superseded`; the image-source **target stays `superseded`**; `current = R_new`. *(superseded vs rolled_back is informational only — both are retained and rollback-eligible, so no conflict when target == run-start current.)* |
| retention | evict beyond `keep_releases` (§7.13) |

Because finalize always demotes the **run-start `current`** (not merely "the previous release"), a recovery run cleanly resolves a stale `current` left by a crashed deploy: no release is left `active`-but-not-`current`, and no container is left running-but-unrecorded (§7.10 reaps it).

---

## 5. Configuration schema (`dcd.yaml`)

`${VAR}`/`${VAR:-default}` interpolate from the **resolved environment** (process env layered over the dotenv chain, §5.2) at load; missing var, no default → `ConfigError` (exit `2`). Below is the **complete acme translation**, exercising every feature.

```yaml
version: 1
project: acme                       # naming prefix for containers/lock/state

deploy_root: ${DEPLOY_ROOT}            # abs path on server (default: $DEPLOY_ROOT, else cwd; omittable)
network: acme_default               # external docker network (created if absent)

registry: ${REGISTRY}                  # default: $REGISTRY or $CI_REGISTRY_IMAGE (omittable)

docker:
  images:                              # logical name -> tag (tags usually from --image / env)
    app: ${APP_TAG}
    database: ${DB_TAG}
    nginx: ${NGINX_TAG}

  services:                            # managed services, reconciled in THIS declared order
    postgres:
      image: database
      container: acme-postgres
      recreate: on-image-change        # on-image-change | always | never
      on_recreate_drain_workers: true  # drain workers before recreating this service
      wait: { exec_in: acme-postgres, cmd: 'pg_isready -U app -h 127.0.0.1', retries: 60, interval: 1s }
    nginx:
      image: nginx
      container: acme-nginx
      recreate: on-image-change
      wait: { exec_in: acme-nginx, cmd: 'test -f /var/run/nginx.pid', retries: 30, interval: 1s }
    valkey:
      container: acme-valkey        # no `image:` -> dcd never owns its tag (recreate: never)
      recreate: never
      wait: { exec_in: acme-valkey, cmd: 'valkey-cli ping', retries: 30, interval: 1s }

compose:
  files: [docker-compose.prod.yml]     # -f files; stage compose.files APPENDS (additive, see §5.1)
  env:                                 # injected into every compose command's process env (§5.2); map MERGES on stage merge; NEVER written to disk
    COMPOSE_PROJECT_NAME: acme
    COMPOSE_IGNORE_ORPHANS: 'true'     # app/workers managed outside this file -> suppress orphan noise
    REGISTRY: ${REGISTRY}
    APP_TAG: ${APP_TAG}
    DB_TAG: ${DB_TAG}
    NGINX_TAG: ${NGINX_TAG}
    MAXMIND_ACCOUNT_ID: ${MAXMIND_ACCOUNT_ID}
    MAXMIND_LICENSE_KEY: ${MAXMIND_LICENSE_KEY}
    DEPLOY_ROOT: ${DEPLOY_ROOT}

directories:
  - { path: .docker/logs/symfony, owner: '1000:1000' }
  - { path: .docker/valkey/data }

release:                               # the red-black app
  image: app
  container_prefix: acme-app-rr     # -> acme-app-rr-<release_id>
  run:
    network_alias: app-rr              # SHARED by red & black; never used for healthcheck (§7.7 / §9)
    restart: unless-stopped
    env_file: .env.deploy              # optional --env-file (resolved vs deploy_root); operator-managed file, the sanctioned dcd-consumed env-at-rest (§5.2.5 names the full residual list); also fed to migrate:before
    env: { TZ: UTC }                   # extra vars; delivered via process-env passthrough (§5.2), override env_file and chain keys
    env_include: []                    # optional full-match regex list: chain keys passed to this container (empty = all) (§5.2)
    env_exclude: []                    # optional full-match regex list: chain keys withheld; exclude wins over include
    volumes: ['${DEPLOY_ROOT}/.docker/logs/symfony:/usr/src/myapp/var/log']
  healthcheck:
    exec_in: acme-nginx             # check runs from nginx; {container} = black NAME (not the alias)
    cmd: 'curl -sf http://{container}:2114/health?plugin=http'
    retries: 60
    interval: 2s
  migrate:
    before: 'php bin/console app:db:migrate before --no-interaction'   # throwaway container
    after:  'php bin/console app:db:migrate after --no-interaction'    # exec in black
  drain: 'bin/graceful-stop.sh'              # graceful stop in the old container

cutover:
  upstream_file: nginx-upstream.conf
  template: 'set $backend "{backend}";'   # {backend} = <black_container>:<backend_port>
  backend_port: 8080
  fallback_backend: '127.0.0.1:8080'      # INV-9 self-heal target
  validate: { exec_in: acme-nginx, cmd: 'nginx -t' }   # optional pre-reload syntax check
  reload:   { exec_in: acme-nginx, cmd: 'nginx -s reload' }

workers:
  name_filter: 'worker-'              # discovery prefix (docker ps name filter) + generated service prefix
  drain: 'php bin/console messenger:stop-workers --env=prod'
  stop_timeout: 120s
  compose_file: docker-compose.workers.yml   # generated by dcd
  provider: { command_in_release: 'php bin/console app:worker:list --no-ansi --env=prod' }
  # provider: { static: [async, scheduler] }   # alternative
  template:
    image: app
    entrypoint: ['php', 'bin/console', 'messenger:consume']
    command: ['{name}', '--memory-limit=256M', '--time-limit=3600']
    stop_signal: SIGTERM
    stop_grace_period: 120s
    restart: unless-stopped
    env: { TZ: UTC }                   # values ride the compose process env; the generated file carries key NAMES only (§5.2/§7.12)
    env_include: []                    # optional full-match regex filters over chain keys, as in release.run (§5.2)
    env_exclude: []
    volumes: ['${DEPLOY_ROOT}/.docker/logs/symfony:/usr/src/myapp/var/log']

retention: { keep_releases: 3, keep_managed_images: 2 }   # app releases + managed-image versions retained

plugins: [plugins/centrifugo.lua]      # optional Lua

hooks:                                 # zero-Lua extension path; actions in §6.3
  after_healthcheck:
    - exec_in_release: 'php bin/console app:realtime:config --output=/tmp/centrifugo-config.json --no-interaction --env=prod'
    - cp_from_release: { from: '/tmp/centrifugo-config.json', to: '.docker/centrifugo/config.json' }
    - compose: ['up', '-d', 'centrifugo']

stages:                                # merged over the base above
  beta:
    host: beta.example.internal        # guard (INV-8); omit to disable
    compose:
      files: [docker-compose.beta.yml] # APPENDS to base files (§5.1)
      env: { APP_ENV: beta }
    retention: { keep_releases: 2 }
  prod:
    host: prod.example.internal
    compose: { env: { APP_ENV: prod } }
    retention: { keep_releases: 5 }
```

### 5.1 Merge, interpolation, override (each tested, §10)

| Rule | Behaviour |
|------|-----------|
| Stage merge — maps | deep-merged; stage keys override base |
| Stage merge — scalars | stage replaces base |
| Stage merge — lists | stage list **replaces** base list — **except `compose.files`, which APPENDS** (compose `-f` is additive; the one ergonomic exception, demonstrated by the `beta` stage) |
| Interpolation | `${VAR}` / `${VAR:-default}` from the resolved environment (§5.2: process env over dotenv chain) at load; unresolved + no default → `ConfigError` |
| Removed keys | `compose.env_file` in any config → targeted `ConfigError` naming UPGRADE.md (pre-deserialize scan; a plain `deny_unknown_fields` "unknown field" would misdirect to the typo row of the AGENTS.md §7 table) |
| `--set path=value` | applied **after** interpolation, **before** validation; may override **existing scalar paths only** (a new path → error, preserving no-laundered-defaults); dotted grammar with `[i]` list indices |
| Validation | unknown keys → error (typo guard); referenced `image:` must exist in `docker.images`; `exec_in`/`wait.exec_in` must name a declared `docker.services` container; `healthcheck.cmd` must reference `{container}` (guard against accidental alias use) |

### 5.2 Environment system (dotenv chain → ephemeral delivery)

**Goal (2026-07-28 rework, ADR-011/012):** no dcd-written secret bytes at rest on the deploy server. The canonical secret source is Symfony-style dotenv files held wherever `dcd` is launched from; values reach containers only through process-env passthrough and are persisted solely by Docker itself in the container config (root-only `/var/lib/docker`, which is what makes `--restart` survive reboot — verified against Docker 29.4.0: env is baked at create and survives stop/start with no env present).

#### 5.2.1 The dotenv chain

Loaded before config interpolation. The chain hangs off a **base file**: `--env-file <path>` when given (Symfony `loadEnv` semantics — every layer name below is `<path>` + suffix, so dcd's chain can live beside an application's own `.env` files, e.g. `.env.deploy[.local|.<stage>|.<stage>.local]`; the base — or its `.dist` — **must exist**, a missing explicit base is a `ConfigError`), else `<--env-dir>/.env` (default `--env-dir`: **the directory of the config file**; `--env-file` and `--env-dir` conflict). The stage is resolved by `config::peek_stage` (the stage positional — the CLI's only stage selector — else the sole `stages:` key, else the empty string for a stages-less config; safe pre-interpolation because interpolation is values-only and stage names are keys):

| Order | Layer | Notes |
|-------|-------|-------|
| 1 | `.env` — or `.env.dist` when `.env` is absent | Symfony parity |
| 2 | `.env.local` | always loaded (deviation: Symfony skips it for `test` envs; dcd has no test-env concept) |
| 3 | `.env.<stage>` | skipped entirely (with 4) when the stage is literally `local` (Symfony parity) |
| 4 | `.env.<stage>.local` | |
| 5 | `--env-stdin` | one dotenv-format document read from stdin (same parser; error-context filename `<stdin>`); the highest **file** layer. Refused **eagerly at argument parsing** when stdin is a TTY, or when the command can prompt (`rollback`, refuse-confirmations) and `-y/--yes` is absent |

Later layers override earlier; the **real process environment overrides every layer** (captured once at startup, `src/cli.rs`). An **absent** file is silently skipped (an empty chain is the pre-rework status quo); a file that is present but unreadable, a directory, or not valid UTF-8 is a loud `ConfigError` naming the path — never a silent skip. A `--env-dir` pointing at a missing directory is a `ConfigError`. A stage resolved to the empty string (no `stages:`) loads layers 1–2 only — never `.env.` / `.env..local`.

**Parser:** a Rust port of `symfony/dotenv` **8.1** (`Dotenv::parse` + `parseRaw`), including the exact grammar (quoting, concatenated segments, `export`, comments, CRLF/BOM rules, NUL-byte rejection, `_*`-prefixed variable names, `${VAR}` / `${VAR:-default}` / `${VAR:=default}` with Symfony's brace/default edge cases) and the `FormatException` context format (`<msg> in "<file>" at line N` + snippet + caret). 8.1 lexes values **raw** — literal `$` is protected as a `\x00` marker (`\$`, single-quoted `$`), backslashes stay escaped — and resolves afterwards; an unquoted value containing `$` may contain spaces (only space-without-`$` errors). Ported deviations, each a hard error or documented: `$(command)` is **lexed but never executed** — a completed `$(…)` expression is a `ConfigError` (a deploy tool must not shell-execute env-file content; during deferred chain resolution the error names the key instead of a file position; the refusal also fires for `$(…)` spanning a quoted newline, and — fail-closed divergence — for empty `$()`/`$(())`, which Symfony's command regex leaves literal); no `$_SERVER`/`HTTP_`/`putenv` semantics (dcd is process-env-model only; the process-env snapshot is the single "external" source); no `.env.local.php`. Conformance is proven by porting the `DotenvTest.php` data providers (§10.1 TC-031).

**Variable resolution** (Symfony 8.1 deferred model, map-based — no process-env mutation): chain layers are parsed **raw**, layered (process env wins for keys it already defines — those keep their external value verbatim, backslashes and `$` included, never executed), then resolved together in **up to 5 passes to a fixpoint** (`resolveLoadedVars`). Consequences, each pinned by a ported case: a later layer overriding `REDIS_HOST` rewrites an earlier layer's `redis://${REDIS_HOST}`; **forward references** across layers resolve; a **self-referencing** value (`MY_VAR=${MY_VAR}_suffix`, `${MY_VAR:-default}`) hides its own raw value and sees the pre-chain external value / the previous layer's value / the default; values still changing after 5 passes are a `ConfigError`: `Too many levels of variable indirection in env vars: <NAMES>.`. A `$NAME` lookup resolves against the working map (process env ∪ loaded layers) with Symfony's external-value protection. `:=` additionally assigns the default. **The ported test suite (TC-031) is the authoritative oracle**: where this prose and a ported Symfony case could be read to disagree, the case wins and the prose is corrected.

#### 5.2.2 The two resolved maps

| Map | Contents | Consumers |
|-----|----------|-----------|
| **interpolation env** | process env layered over the chain (process wins) | `${VAR}` in `dcd.yaml`; `registry`/`deploy_root` defaults; `ctx.env()` in Lua (both hosts) |
| **container env** | keys **defined in any chain layer** (incl. stdin), values after process-env override | passthrough to release/migrate/worker containers, after per-container filters |

A key present only in the process env (e.g. `PATH`, CI noise) never reaches a container. The Symfony convention makes CI-only delivery work with no extra mechanism: a committed secret-free `.env` names the key (`DATABASE_URL=`), CI exports the value over ssh, process-env-wins supplies it — the file is the manifest, the env is the value.

#### 5.2.3 Reserved keys (tooling-hijack guard)

A chain layer that defines any of `PATH`, `HOME`, `LD_*`, `DOCKER_*`, `COMPOSE_*`, `BUILDX_*`, `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` (case-insensitive for the proxy set) → `ConfigError` naming the key **and the defining file**, plus the escape hatch (`release.run.env`). The same guard applies to the `compose.env` and `workers.template.env` maps and to `release.run.env`, with one carve-out: `compose.env` may set `COMPOSE_*` (deliberate config, e.g. `COMPOSE_PROJECT_NAME`, `COMPOSE_IGNORE_ORPHANS`), and `release.run.env`/`workers.template.env` may set the proxy family (a container legitimately needs proxies; the acknowledged cost is that the value then rides that one command's docker-CLI child env — the overlay — during create). Rationale: the chain and overlays ride the child process env of `docker`/`docker compose`, which read their own configuration from it — a `.env` with `DOCKER_HOST` would silently retarget the deploy.

#### 5.2.4 Delivery

- **Runner-level env:** `SystemRunner` carries the **container-env map** (chain-derived, §5.2.2) and applies it via `Command::envs()` to **every** spawned child, and sets `current_dir(deploy_root)` on every child — fixing, for an **absolute** `deploy_root`, the pre-rework bug where compose resolved relative `-f` paths against dcd's cwd (a relative `deploy_root`, default `.`, still resolves against dcd's cwd exactly as before — cwd-launch workflows are unchanged). The `CommandRunner` trait signature is unchanged (`RunOpts` gains an optional per-command env overlay); hooks (`run:`), `ctx.run`, and compose substitution therefore all see one consistent environment. The three explicit config env maps are all delivered as **per-command overlays** on top of the runner env, overlay wins: `compose.env` on every compose invocation, `release.run.env` on the release/migrate `docker run`, `workers.template.env` on the workers `compose up` (which also carries `compose.env`; `template.env` wins on collision — the more specific map). Precedence for any consumer is therefore: explicit map (overlay) > chain (runner env) > nothing — and a key in both an overlay and the chain is delivered once with the overlay's value.
- **`docker run` (release + migrate throwaway):** for every delivered key, a bare `-e KEY` flag — the docker CLI reads the value from its own (child) environment; values never appear in argv, `ps`, or on disk (verified live). Explicit `release.run.env` pairs are delivered the same way (bare `-e KEY`, value in a per-command env overlay via `RunOpts`), replacing the pre-rework `-e K=V`-in-argv leak; they override chain keys and `env_file` keys. `run.env_file` (operator-managed, the one sanctioned at-rest file) keeps its `--env-file` flag, emitted before all `-e` flags.
- **Compose:** no `--env-file` flag and no rendered file. `compose.env` + container env ride the child process env (compose substitutes `${VAR}` in compose files from its environment — verified). To keep compose's *implicit* `.env`-discovery from re-introducing filtered-out keys, every compose invocation passes `--env-file /dev/null` (verified live: suppresses discovery, no error); `dcd check` warns when `deploy_root/.env` exists and is not the chain's own `.env`.
- **Workers compose file:** `environment:` is a YAML list of bare key **names** (template `env` keys + filtered chain keys; template values ride the workers-`up` command overlay and win over chain on collision). The generated file contains no values — also closing the pre-rework YAML-injection hazard of unquoted `{key}: {value}` embedding. A bare name whose variable is unset at `compose up` resolves to *unset in the container* (compose `null`, verified), which keeps `env_exclude` honest. Consequence, stated deliberately: the generated workers file is **not operator-usable outside dcd** — a hand-run `docker compose up` without dcd's env resolves every bare name to unset.
- **Delivered set per container** = container env filtered by that container's `env_include` (empty = all) then `env_exclude` (exclude wins), plus its explicit `env` map. Filters are **full-match** regexes (`regex-lite`; an unanchored pattern like `MAILER` does *not* match `MAILER_DSN`). `-e` flags and generated key lists are emitted as a **sorted, deduplicated union** — a key in both the chain set and the explicit map appears once (deterministic argv for tests). Empty-valued keys are delivered as empty (verified: docker passes them; only *unset* keys are omitted).

#### 5.2.5 Observability & lifecycle

- **`dcd check [stage]`** prints: chain files found/skipped (paths), per-layer key counts, the sorted key **names** delivered to each container after filters, keys shadowed by the process env, and reserved-key errors. **Resolved values are never printed** (redaction stays removed — the right response is to never print values at all). One deliberate exception: a dotenv **parse error** reproduces Symfony's byte-exact snippet+caret (§5.2.1), which quotes up to 20 raw bytes around the error — a value adjacent to a syntax error can appear in stderr. Accepted: the conformance oracle (TC-031) pins the format, and the exposure requires a malformed file the operator is actively editing. `--dry-run` prints bare `-e KEY` argv — strictly better than the pre-rework `-e K=V`.
- **State fingerprint:** each `Release` records `env_keys` (sorted container-env key names — names are not secrets; `dcd-state.json` stays `0600`). `rollback` and `--resume` diff the recorded set against the currently-resolved set and print a loud warning naming added/removed keys (env is *not* versioned — a rollback runs old images under **today's** chain; the warning is the guard). Value hashes are deliberately not stored (low-entropy secrets are offline-crackable from a hash).
- **Recovery model:** the chain files live at the launch source (operator machine, CI secrets, or `ssh host 'dcd deploy prod --env-stdin --yes' < .env.prod.local` for a genuinely disk-free path — `--env-stdin` consumes stdin, so interactive prompts error and `-y/--yes` is required for any confirming command). Server loss no longer loses secrets. Residual at-rest copies on the server, named deliberately: Docker's own container config under `/var/lib/docker` (inherent — it is what makes reboot-restart work; root-only), the optional operator-managed `release.run.env_file`, any `env_file:` entries inside user-owned compose files (UPGRADE.md shows the bare-key migration), and — in the default layout, where `--env-dir` is the config directory on the server — the operator's own chain files themselves; `--env-stdin` (or values-over-ssh with a secret-free `.env` manifest) is the path that removes that last class. The claim dcd itself makes is precise: **dcd writes no secret bytes to disk** (IT-007's grep proves it over dcd-written files).

---

## 6. Lua plugin API

Loaded after config resolution, before plan execution. Zero plugins is valid (ADR-008). Mirrors PHP Deployer (ADR-003); the engine is a fixed recipe with hook slots, so Lua **adds tasks and wires them into slots** — it does not define an arbitrary graph.

### 6.1 Globals (registration time)

| Global | Signature | Effect |
|--------|-----------|--------|
| `task(name, fn)` | `(string, function(ctx))` | register/override a task body |
| `before(task, hook)` | `(string, string\|function)` | run `hook` in the `before_<task>` slot (string = task name; function = anonymous) |
| `after(task, hook)` | `(string, string\|function)` | run `hook` in the `after_<task>` slot |
| `configure(fn)` | `(function(ctx))` | register the **configure hook** (§6.5): runs once before the recipe to adjust `cfg` (reads `state`/`env` to decide) |
| `set(key, value)` / `get(key)` | | scratch var store (the same persistent `vars` table seen by `ctx`) |
| `cfg` / `state` | table | the live config / current-stage state — the same **mutable** tables as `ctx.cfg` / `ctx.state` (§6.2) |

`before`/`after` may only target a recipe task or another registered task; a hook referencing an unknown task → run error.

### 6.2 `ctx` (passed to every hook body)

**Effects** (engine-routed → dry-run-safe; all `Mutate` per §2.4 unless noted):

| Method | Effect |
|--------|--------|
| `ctx.run(cmd)` | shell on host in `deploy_root`; returns stdout |
| `ctx.in_release(cmd)` | `docker exec {black} sh -c '<cmd>'`; returns stdout |
| `ctx.exec_in(service, cmd)` | `docker exec {service} sh -c '<cmd>'`; returns stdout |
| `ctx.docker(args)` / `ctx.compose(args)` | `docker <args>` / fully-qualified `docker compose … <args>`; returns stdout |
| `ctx.cp_from_release(src,dst)` / `ctx.cp_to_release(src,dst)` | `docker cp` to/from black |
| `ctx.read_file(path)` / `ctx.write_file(path, s)` / `ctx.file_exists(path)` | via the fs seam (`write` dry-run-safe; `read`/`exists` execute) |
| `ctx.env(name)` | resolved-environment var (§5.2.2 interpolation env — process env over the dotenv chain) → string or nil |

**Utilities** (pure): `ctx.json_decode(s)` / `ctx.json_encode(v)` / `ctx.yaml_decode(s)` / `ctx.yaml_encode(v)`, `ctx.log(msg)` / `ctx.warn(msg)`.

**Debug:** `ctx.inspect(v)` → pretty YAML string; `ctx.dump(v?)` → logs `v` (or, with no arg, `cfg`+`state`) as formatted YAML.

**Data:** `ctx.cfg` (resolved config) and `ctx.state` (current stage: `{current, releases:[{id,container,status,images,ran_migrations,reason}]}`) are **live, mutable tables** — the engine refreshes them from the typed config/state before each hook and reads any direct assignment back (no setter function), so a plugin mutation changes the deploy: `cfg` for steps not yet run, `state` read back into deploy state and persisted past cutover. Full power — a `state` rewrite can violate the §4.3 invariants the engine relies on. `ctx.vars` + `ctx.set/get` is a **persistent scratch table shared across all hooks in the run** (not part of cfg/state). Plus `ctx.container`, `ctx.stage`. Structural values fixed at deploy start (`docker.images`, container name, `deploy_root`) are snapshots, not re-read.

**Dry-run honesty (Clarity):** a Lua hook that branches on `ctx.in_release(...)` output gets the stubbed empty result in `--dry-run` (the black isn't started); the host cannot introspect the branch, so the dynamic worker provider and such data-dependent points emit a `⚠ data-dependent` event rather than a fabricated plan. Pure utilities and `cfg`/`state`/`env`/`read_file` resolve for real in dry-run.

### 6.5 The `configure` hook

Registered with `configure(fn)`; fires **once before the recipe**, with a host offering `run`/`read_file`/`write_file`/`file_exists`/`env`/utilities/`cfg`/`state` (no `in_release`/`docker`/`compose`/`cp_*` — there is no release container yet). Its `run` commands execute under the same §5.2.4 runner env and `deploy_root` cwd as recipe commands, and its `ctx.env` reads the §5.2.2 interpolation env — the configure host and the deploy host see one environment. It adjusts the initial config by **mutating `ctx.cfg` directly** (`ctx.cfg.retention.keep_releases = 5`); the mutated table is read back, re-parsed and re-validated into the typed config the engine then runs against. The **same live read-back applies to every hook mid-deploy** (§6.2), not just `configure`: a `before_`/`after_` hook may mutate `ctx.cfg` (honored for any step not yet run) or `ctx.state` (read back into deploy state, persisted once past cutover). Implementation: before firing a slot's hooks the engine `refresh`es the `cfg`/`state` tables from the typed values; after, it re-reads them and, if changed, re-parses (`cfg` re-validated; a failure aborts the deploy). The round-trip goes through Lua, so an empty map serializes as an empty table and is parsed back as an empty map (`de_lenient_map`); map ordering (`docker.images`/`env`/`docker.services`) is not guaranteed across a mutated round-trip but does not affect correctness. `ctx.vars` remains for scratch state that is not part of cfg/state.

### 6.3 YAML hook actions (zero-Lua path)

A `hooks.<slot>` entry is one typed action (a bare string = `run`), each mapping to the same effect as the `ctx` method of the same name: `run` · `exec_in: {service, cmd}` · `exec_in_release` · `docker: [args]` · `compose: [args]` · `cp_from_release: {from,to}` · `cp_to_release: {from,to}`. Slots: `before_<task>` / `after_<task>` for every §3 task.

### 6.4 Sandboxing

mlua with stdlib minus process/file escapes: `os.execute`, `os.exit`, `os.getenv`, `io.popen`, `io.open`, `dofile`, `loadfile`, `require` of arbitrary paths are removed/replaced. All process/file effects must go through `ctx` (so they honour `--dry-run` and the seam). Plugin load/runtime error → exit `10` with the Lua traceback.

---

## 7. Built-in recipe `docker-redblack` (exact commands)

Each task: inputs, the exact argv, the failure rule. `{…}` are resolved values; `compose(...)` = `docker compose -p {project} --env-file {empty-file pin, §5.2.4} -f {compose.files…} [+ -f {workers.compose_file} for worker ops]`, spawned with `current_dir = deploy_root` and the §5.2.4 runner env. Argv is built as `Vec<String>` — no shell unless an action explicitly wraps in `sh -c`.

### 7.1 `preflight`
- `docker network inspect {network}` *(Read)* → on failure `docker network create {network}` *(Mutate)*.
- For each `directories[]`: `fs.create_dir_all(path)`; if `owner` → `docker run --rm -v {deploy_root}:/wd busybox chown {owner} /wd/{path}` (unprivileged-safe chown; resolves OQ-1).
- **Orphan reaping:** `docker ps -a --filter name={release.container_prefix}- --format '{{.Names}}'` *(Read)*; for each not present in `state.releases` → `docker rm -f {name}` *(Mutate)* (clears crashed-deploy leftovers; §2.6).
- No env file is written (§5.2 — env rides the process environment of every spawned command).
- Failure → abort, red untouched.

### 7.2 `ensure_upstream` (INV-9)
- If `state.current` set and `docker ps -q --filter name={current}` *(Read)* is empty (dead), **or** upstream/state file missing → `fs.write(upstream_file, render(template, backend=fallback_backend))`.
- Mirrors the original script; runs before `infra` so a recreated nginx never points at a corpse.

### 7.3 `pull`
- `docker pull {docker.images.app}` *(Mutate)*; for each managed service with an `image:` → `docker pull {tag}`.
- Failure → abort (red untouched).

### 7.4 `infra` (ordered; mirrors the original script)
- For each service in **declared order**: desired = `docker.images[service.image]`; current = `docker inspect {container} --format '{{.Config.Image}}'` *(Read; missing ⇒ `none`)*.
  - `recreate: never` → `compose up -d --no-recreate {service}`.
  - `recreate: always` → `compose up -d {service}`.
  - `recreate: on-image-change` → if current≠desired: if `on_recreate_drain_workers` and not yet `drained` → **worker drain** (§7.9), set `drained`; then `compose up -d {service}`; else `compose up -d --no-recreate {service}`.
- After all recreates, run every `wait` gate in declared order: poll `docker exec {wait.exec_in} sh -c '{wait.cmd}'` *(Read)* each `interval` up to `retries`; exhaustion → abort.
- **Ordering contract (tested):** worker drain (if any) precedes the first recreate; wait-gates run after all recreates.

### 7.5 `migrate:before`
- Skip if unset. `docker run --rm --network {network} --name {project}-migrate-{release_id} {--env-file run.env_file} {-e KEY…} {docker.images.app} php bin/console app:db:migrate before --no-interaction` — argv passed **directly** (no `sh -c`), matching the original script. `-e` flags are bare key names in sorted order (filtered chain keys + `run.env` keys; values in the child env, §5.2.4), so the migrate throwaway carries the same runtime env as the release; image-baked env + `--network`; no volumes.
- Failure → abort (red untouched). Note: migrations are not guaranteed atomic — expand-contract discipline must keep even a partially-applied `before` migration backward-compatible with red (§11, §9).

### 7.6 `start:black`
- `container = {container_prefix}-{release_id}` (stored id, §2.6); fail if it already exists (defence behind the lock; orphan-reaping in 7.1 clears stale ones).
- `docker run -d --name {container} --network {network} --network-alias {run.network_alias} --restart {run.restart} {--env-file run.env_file} {-e KEY…} {-v vol…} {docker.images.app}`. `-e` flags are bare sorted key names (values via child env, §5.2.4); `--env-file` (resolved vs deploy_root) precedes `-e`, so a delivered key overrides the file. Mirrors the original script.

### 7.7 `healthcheck`
- Repeat up to `retries`, sleeping `interval`: substitute the black **container name** into `healthcheck.cmd` (`{container}`), run `docker exec {healthcheck.exec_in} sh -c '<cmd>'` *(Mutate — execs into the black, which exists only after `start:black`; stubbed-OK in `--dry-run` like any post-`start:black` step)*; success on exit 0.
- **Never substitute the shared `network_alias`** — red and black both answer to `app-rr`, so the alias would resolve to red and pass falsely (§9). Validation (§5.1) enforces `{container}` is present in `healthcheck.cmd`.
- Exhaustion → remove black, keep red (INV-1). Mirrors the original script.

### 7.8 `cutover` (INV boundary, mirrors the original script)
- `prev = fs.read(upstream_file)` (capture for restore).
- `backend = {container}:{cutover.backend_port}`; `fs.write(upstream_file, render(template, backend))`.
- Optional `cutover.validate` → `docker exec … nginx -t` *(Read)*; on failure: restore `prev`, abort (red serving).
- Reload: `docker exec {reload.exec_in} sh -c '{reload.cmd}'`.
  - reload ≠ 0 → restore `prev` (`fs.write(upstream_file, prev)`), remove black, abort, exit `1` (red still serving its in-memory config; on-disk now matches).
  - reload = 0 → **point of no automatic return.** In one atomic state write: append `Release{ id, container, images: {app, …each managed service's resolved tag}, created_at, status: cutover_pending, ran_migrations: (before non-empty) }` **and demote any pre-existing `cutover_pending` → `rolled_back`** (INV-10), then persist (INV-3). Flip run into post-cutover mode (INV-2).

### 7.9 worker drain (shared helper; discovery-based, no compose file needed)
- `names = docker ps --filter label=com.docker.compose.project={project} --filter name={workers.name_filter} --format '{{.Names}}'` *(Read)*.
- For each: `docker exec {name} sh -c '{workers.drain}'` *(Mutate, best-effort)*.
- `docker stop --timeout {stop_timeout_secs} {names…}` *(Mutate)* — uses `docker stop` on discovered names, **not** `docker compose stop`, so it works even on the first deploy when the workers compose file does not yet exist (fixes the ordering bug). The `drained` flag on `Context` suppresses a second drain **within one run** (set in `infra`, checked in `drain:red`); it is per-process, so `--resume` (a fresh process) re-runs the drain — safe because `workers.drain` is best-effort and the `workers` task recreates the set.

### 7.10 `drain:red` (mirrors the original script)
- Drain the old red by **reaping every running app container except the just-cut-over black**: `docker ps --filter name={release.container_prefix}- --format '{{.Names}}'` *(Read)* minus the current black; for each → `docker exec {c} sh -c '{release.drain}'` *(best-effort)*, then `docker rm -f {c}`. This drains the prior `serving` container whether it was `current` (normal) or a crashed `cutover_pending` (recovery), and is resume-safe (already-removed → nothing to reap). Preflight orphan-reaping (§7.1) never touches these — the old red is still in `state` — so only `drain:red` removes it.
- Then worker drain (§7.9) unless `drained` already set in `infra`.

### 7.11 `migrate:after` (mirrors the original script)
- Skip if unset. `docker exec {black} php bin/console app:db:migrate after --no-interaction` (argv direct; shares the running black's env).
- Failure → post-cutover (INV-2): stop, report, exit `4` (black stays live; recover via `--resume` after fixing). Must be idempotent — `--resume` may re-run it after a later (`workers`) failure; Doctrine version-tracking skips already-applied migrations.

### 7.12 `workers` (mirrors the original script)
- Names: `provider.static`, or run `provider.command_in_release` in black *(Mutate — execs in black; in `--dry-run` there is no black, so it emits `⚠ worker set is dynamic; not resolvable in dry-run`)* and split stdout into non-empty lines, filtering names containing `.` and any in an `exclude` list (matches the worker-listing command's behaviour).
- Render `workers.compose_file`: one `{name_filter}{name}` service per name from `template` (substitute `{name}` in `command` + service name), plus the external-network footer. `environment:` is a bare-key-name list (§5.2.4) — the file contains no env values.
- `compose up -d {name_filter}{name}…` — **explicit worker service list only; never a bare `up -d`; `--remove-orphans` is forbidden** (the app is `docker run`-managed and would be deleted) (§9).

### 7.13 `finalize` (mirrors the original script, corrected)
- Apply the §4.3 finalize transition (deploy or rollback) — flip the `cutover_pending` release → `active`, reconcile the prior statuses exactly per §4.3, set `current = black` — then `fs.write(dcd-state.json, …, 0600)`. Any leftover `cutover_pending` was already demoted at cutover (INV-10), so finalize sees exactly one.
- **Retention (state-based, not `docker images` parsing):** retain `current`, any `cutover_pending`, and the newest `keep_releases` releases with status in {`superseded`, `rolled_back`}; evict older ones — for each evicted release, `docker rm -f` any leftover container and `docker image rm {release.images.app}` **iff** no retained release and no running container references that image. Replaces the script's `docker image prune -a -f` (which would delete rollback targets — INV-6).
- **Managed-image GC:** every `Release.images` records all resolved tags (app + each managed service). Retain the newest `keep_managed_images` distinct tags per managed image across `releases[].images`; `docker image rm` older unreferenced ones — bounds the disk growth the blanket prune used to cover. Never `docker image prune -a`.

---

## 8. CLI surface

```
dcd <command> [stage] [flags]
```

| Command | Behaviour |
|---------|-----------|
| `deploy [stage]` | run the recipe; `stage` optional if exactly one exists (or the config has no `stages:` block — the stages-less case §5.2.1 covers). `--resume` recovers a `cutover_pending` state (§4.2); a non-resume deploy that finds one refuses (exit 4). Stage selection is the **positional only** (there is no `--stage` flag) |
| `rollback [stage]` | §4.1; requires `--yes` when non-interactive |
| `status [stage]` | current + history from state: each release's status, images, age, `ran_migrations`, and any `cutover_pending` recovery hint |
| `tasks [stage]` | print the resolved, ordered task plan (graph + hooks); no side effects |
| `check [stage]` | validate config + stage merge + dotenv chain + interpolation + `--set` + plugin load; prints the §5.2.5 env observability report; no side effects |
| `init` | scaffold config — see §8.4 |
| `version` | binary version |

| Global flag | Meaning |
|-------------|---------|
| `-c, --config <path>` | config file (default `./dcd.yaml`) |
| `--resume` | (deploy) recover a post-cutover-incomplete release |
| `--dry-run` | read-pass-through plan; mutations stubbed (§2.3/§2.4) |
| `--json` | newline-delimited JSON events |
| `--image <logical>=<tag>` | override a `docker.images.<logical>` entry (repeatable; CI passes app/db/nginx) |
| `--set <path>=<value>` | override an existing config scalar (repeatable; §5.1 semantics) |
| `--env-dir <path>` | directory of the dotenv chain (default: the config file's directory) (§5.2.1) |
| `--env-stdin` | read one dotenv-format document from stdin as the highest file layer; interactive prompts then error without `-y/--yes` (§5.2) |
| `-v/--verbose`, `-q/--quiet`, `--no-color` | output control |
| `-y/--yes` | assume yes (rollback / refuse prompts) |
| `--reason <text>` | annotate this deploy/rollback in state |

### 8.1 Exit codes

| Code | Meaning | System state |
|------|---------|--------------|
| 0 | success | black is the new red |
| 1 | pre-cutover failure | **red still serving**; black removed |
| 2 | config / usage error | nothing ran |
| 3 | stage lock held | another deploy in progress; nothing ran |
| 4 | post-cutover incomplete (`drain:red`/`migrate:after`/`workers`), or a `deploy` found a `cutover_pending` state | **black is live**; recover with `--resume` (or `rollback`) |
| 5 | host guard mismatch (INV-8) | nothing ran |
| 10 | Lua plugin error | reported with traceback |
| 130 | interrupted (SIGINT/SIGTERM) pre-cutover | cleaned up; red serving |

### 8.2 Output modes (ADR-009)

One `Event` stream → reporter renders by environment: **rich** (TTY: per-task status + elapsed + summary), **plain** (no TTY: `[HH:MM:SS] <task>: <status>` — matches today's `log()`), **`--json`** (`{ts,stage,task,status,ms,detail}` per line). All failures print the failing argv + captured stderr.

### 8.3 CI integration (replaces the existing CI deploy stage)

```
scp dcd dcd.yaml .env plugins/ → $DEPLOY_ROOT
ssh server "cd $DEPLOY_ROOT && ./dcd deploy prod --env-stdin --yes \
  --image app=$DOCKER_IMAGE_TAG_APP \
  --image database=$DOCKER_IMAGE_TAG_DATABASE \
  --image nginx=$DOCKER_IMAGE_TAG_NGINX" < .env.prod.local
# secrets stream over ssh stdin — no dcd-written secret FILES at rest
# (Docker still bakes values into root-only /var/lib/docker container
#  config; that is what makes reboot-restart work — §5.2.5);
# the scp'd secret-free .env names the keys (§5.2.2);
# MAXMIND_* / REGISTRY may instead be exported as CI env → process-env-wins
```

### 8.4 `dcd init`

Writes `./dcd.yaml` (a single-stage runnable skeleton: `project`, `network`, `registry`, a `docker:` block with `images` + one managed `services` entry, a `release` block with healthcheck/migrate, `cutover`, `retention`). Refuses if `dcd.yaml` exists unless `--force`. `--with-plugin` also writes `plugins/app.lua` (a commented `after('healthcheck', …)` stub). The emitted skeleton is fixed (snapshot-tested, §10) so two runs are identical.

### 8.5 Code quality requirements (pre-empts comment churn)

Generated Rust MUST be dumb-simple and readable: intention-revealing names, small single-purpose functions, guard clauses over nesting, exhaustive `match`, `Result` + `?` (no `unwrap`/`expect` outside tests and provably-infallible spots, each carrying a one-line WHY). `///` rustdoc on **public** items only, one line, no WHAT/HOW prose; **no inline narration**. `clippy -D warnings` + `rustfmt` are the floor. If a construct needs a comment to be understood, rewrite it simpler.

---

## 9. Anti-Patterns (DO NOT)

| Don't | Do Instead | Why |
|-------|-----------|-----|
| `docker image prune -a -f` after deploy | state-based eviction beyond `keep_releases` + managed-image GC (§7.13) | the blanket prune deletes the rollback target — a real bug in the original script (INV-6) |
| `docker compose up -d` with no service list, or `--remove-orphans` | scope every compose `up` to explicit services; never `--remove-orphans` | a bare up / orphan-removal would recreate or **delete** the `docker run`-managed app containers (§7.12) |
| Healthcheck via the shared `network_alias` (`app-rr`) | healthcheck the black **container name** (§7.7) | red & black share the alias; the alias resolves to red → false-positive health, cut over to a sick black |
| Auto-rollback on a post-cutover failure | stop, report, `exit 4`, `--resume` after fixing | black is already live; tearing it down for a failed worker causes more disruption (INV-2) |
| Advance `current` before cutover succeeds | record `cutover_pending` at reload=0; flip in `finalize` (INV-3) | otherwise an exit-4 leaves the live black unrecorded and breaks rollback math |
| Drain workers via `docker compose stop` against the workers file | discovery (`docker ps`) + `docker stop {names}` (§7.9) | the workers compose file doesn't exist on the first deploy |
| Compose calls without `-p {project} --env-file -f{files}` | the fully-qualified `compose(...)` helper, always | else compose derives a different project name and targets nothing (the original script uses explicit `-p`) |
| Re-sample the clock for `release_id` mid-run | sample once at plan start, store on `Context` (§2.6) | the migrate/black/healthcheck names must all share one id |
| Build docker argv as one interpolated string | `Argv` as `Vec<String>`; `sh -c` only when an action needs a shell | injection/quoting bugs; argv is also what `--dry-run` prints |
| Rely on Rust `Drop` to release the lock on a signal | catch SIGINT/SIGTERM; use `flock(2)` (OS-released on SIGKILL) (§2.5) | `Drop` doesn't run on default-terminating signals |
| Silently default an unknown/missing config key | unknown → error; missing required → error | a laundered default produces a confident wrong deploy |
| Parse container names to find the rollback target | read typed `state.releases` (§4.1) | names are display, state is truth |
| Write env values to any file dcd owns (compose env, workers compose, temp files) | process-env passthrough + bare `-e KEY` / bare compose keys (§5.2.4) | a file at rest is the leak class this rework removes; crash-safety of "write then delete" is a lie |
| `-e KEY=VALUE` in `docker run` argv | bare `-e KEY`, value in the child process env | argv is world-readable in `ps` for the container's whole create window |
| Execute `$(command)` found in a dotenv value | `ConfigError` naming file+line (§5.2.1) | env files are data; shell execution of them is an injection primitive |
| Silently drop or empty an unresolvable env construct | loud `ConfigError` with the Symfony-format context | a silently-empty `DATABASE_URL` deploys a broken container with exit 0 |
| Pass the whole process env (or chain) into containers | container env = chain-defined keys only, then per-container filters (§5.2.2/5.2.4) | `PATH`/CI noise in a container, and secrets reaching containers that don't need them |

---

## 10. Test Case Specifications

Unit tests use the effects seam (no Docker). Integration tests (`IT-*`) run against real Docker behind `DCD_E2E=1` + a `docker` CI service.

### 10.1 Unit tests (per component)

| Test ID | Component | Input | Expected | Edge cases |
|---------|-----------|-------|----------|------------|
| TC-001 | config merge | base + `prod` | scalars overridden, maps deep-merged, lists replaced | empty stage; stage absent → error |
| TC-002 | `compose.files` append | base `[a]` + stage `[b]` | `[a,b]` (additive exception) | stage absent → `[a]` |
| TC-003 | interpolation | `${X}`,`${Y:-d}`,missing `${Z}` | X from env, Y default, Z → error | `${}`, nested, value with `$` |
| TC-004 | `--set` | existing scalar / new path / list `[i]` | override / error / index set | runs after interp, before validate |
| TC-006 | config validation | unknown key; `image: ghost`; bad `exec_in`; healthcheck without `{container}` | each → distinct error | valid → ok |
| TC-007 | plan build | tasks + before/after + anon fn hooks | exact ordered plan incl. hook slots | self/mutual cycle → error |
| TC-008 | `infra` recreate | current==/≠desired / `never` | `--no-recreate` / recreate / `--no-recreate` argv (with `-p`) | `none` current |
| TC-009 | `infra` order+drain | DB changed + `on_recreate_drain_workers` | worker-drain argv precedes DB recreate; waits last | flag false → no drain |
| TC-010 | worker drain | discovered names | `docker stop --timeout N {names}` (not compose stop) | none found → no-op; `drained` skips second |
| TC-011 | `ensure_upstream` | current dead / running / missing | fallback written / untouched / written | no state → written |
| TC-012 | `start:black` argv | release config | exact `docker run -d …`; uses stored id | name collision → error |
| TC-013 | healthcheck | fail N then ok / always fail; cmd uses `{container}` | pass on N+1 / abort (red kept, black removed); argv has black **name** | retries=1 |
| TC-014 | cutover reload fail | reload≠0 | upstream **restored** to prev, black removed, exit 1, state unchanged | validate `nginx -t` fail path |
| TC-015 | cutover success | reload=0 | `cutover_pending` appended to state **before** drain:red | — |
| TC-016 | workers gen | dynamic stdout `async\nsched` / static | 2 `worker-*` services + footer; scoped `up -d worker-async worker-sched` | empty → none; `.`-names filtered |
| TC-017 | finalize retention | 1 current + 5 superseded, `keep_releases` 3 | current + newest 3 superseded kept (4 total); oldest 2 evicted (rm container+image) | keep 1 keeps the rollback target; a `rolled_back` release is retained like `superseded` |
| TC-018 | managed-image GC | 3 db tags across `releases[].images`, `keep_managed_images` 2 | oldest unreferenced db image removed | referenced by a retained release → kept |
| TC-019 | rollback plan | state w/ current+previous | deploys previous images, **no** migrate tasks (INV-5); verifies images exist | no previous → error; images gone → error |
| TC-020 | resume plan | state w/ `cutover_pending` | runs only drain:red→migrate:after→workers→finalize against recorded black | no pending → refuse |
| TC-021 | exit-code mapping | each `DcdError` | correct code (1/2/3/4/5/10/130) | post-cutover error → 4 |
| TC-022 | host guard | `host:` == / != hostname | proceed / exit 5 | no `host:` → proceed |
| TC-023 | stage lock | acquire while held; dead-holder | exit 3; reclaim dead-holder flock | released on drop |
| TC-024 | Lua registration | plugin `task/after`; hook→unknown task | task at right slot; unknown → load error | anon-fn hook |
| TC-025 | Lua `ctx` over recorder | plugin runs `ctx.in_release/compose/cp` | exact recorded argv (compose fully-qualified) | `ctx.run{check=false}` swallows nonzero |
| TC-026 | dry-run read-pass-through | full deploy `--dry-run` | reads execute, mutations stubbed+printed; dynamic provider → `⚠ data-dependent`; zero real mutations | matches golden snapshot |
| TC-027 | `init` | empty dir / existing file | skeleton written / refuse without `--force` | `--with-plugin` writes stub |
| TC-028 | second (synthetic) config | the §15 appendix config (no Postgres, static workers, non-nginx healthcheck host, different ports, no migrations) | recipe builds a valid ordered plan + correct argv with **zero core changes** | proves generality |
| TC-029 | rollback finalize | state {B1 superseded, B2 current}; rollback | fresh black R3 from B1 images → `active`/`current`; B2 → `rolled_back`; B1 stays `superseded` (§4.1 step 7) | rollback-of-rollback steps to next target, skips no-op |
| TC-030 | single-pending (INV-10) | state has a crashed `cutover_pending` P; run cutover for R_new | after append exactly **one** `cutover_pending` (R_new); P → `rolled_back`; drain:red reaps P's container; finalize demotes run-start `current` | resume aborts if >1 `cutover_pending` found |
| TC-031 | dotenv parser (Symfony 8.1 port) | the ported `DotenvTest.php` data providers (`getEnvData`, `getEnvDataWithFormatErrors`, load/loadEnv chain cases) | byte-identical values / Symfony-format errors; `$(cmd)` cases → `ConfigError` instead of execution; `$_SERVER`/`putenv`/`.env.local.php` cases N/A (documented) | `${FOO:-a$a}` unsupported-char error; `${FOO:-a"a}` → missing-quote (8.1); `__FOO_BAR`; NUL rejection; brace-less `$FOO:-TEST}`; `:=` assignment; BOM; CRLF |
| TC-032 | chain layering + deferred resolution | dir with all 4 layers + stdin + process env | later layer wins; process env wins over all (external values verbatim — backslash matrix, `secret$word`, `value$(id)` never executed); `.env.dist` only when `.env` absent; stage `local` skips 3–4; empty stage loads 1–2 only; missing files skipped; missing `--env-dir` → error; `--env-file` rebases the chain, missing explicit base → error | cross-layer + forward `${VAR}` references resolve; later override rewrites earlier reference; self-referencing defaults; circular chain → `ConfigError` |
| TC-033 | container-env derivation | chain + process-env-only vars + filters | container env = chain keys only; `env_include`/`env_exclude` full-match regex, exclude wins; sorted `-e KEY` argv; empty-valued key delivered | unanchored pattern does not substring-match |
| TC-034 | reserved keys | chain defines `DOCKER_HOST` / `COMPOSE_FILE` / `PATH` | `ConfigError` naming key + escape hatch | `compose.env` map may set `COMPOSE_*` |
| TC-035 | runner env + cwd | full deploy over the recorder | every recorded call carries the chain runner env and `current_dir = deploy_root`; compose calls additionally carry the `compose.env` overlay (overlay wins); `docker run` bare `-e` keys present in its child env; no env-value bytes in any `fs.write` | recorder must capture env to make this assertable |
| TC-036 | workers env passthrough | template.env + chain + filters | generated YAML `environment:` = bare sorted deduplicated names only, no values; template.env value wins over chain in the workers-`up` command env (overlay) | excluded key absent from the list |
| TC-037 | env fingerprint | deploy records `env_keys`; rollback/resume under a changed chain | sorted names in state; loud warning listing added/removed keys | unchanged chain → no warning |
| TC-038 | removed key `compose.env_file` | pre-rework config | targeted `ConfigError` pointing at UPGRADE.md (not "unknown field") | `release.run.env_file` still accepted |
| TC-039 | `check` env DX | chain + filters + shadowing process env | prints files found/skipped, per-layer counts, per-container key names, shadowed keys; **no values anywhere in output** | reserved key → error |
| TC-040 | `--env-stdin` | dotenv doc on stdin; a confirming command without `--yes` | stdin layer overrides `.env.<stage>.local`; prompt → error demanding `--yes` | empty stdin = empty layer |

### 10.2 Integration tests

| Test ID | Flow | Setup | Verification | Teardown |
|---------|------|-------|--------------|----------|
| IT-001 | happy deploy | fake app image serving `/health` + nginx on a scratch network | health passes, upstream swaps, old container gone, state `active` | rm containers/network/images |
| IT-002 | failed healthcheck | app never passes `/health` | aborts pre-cutover, **red serving**, black removed, state unchanged, exit 1 | as above |
| IT-003 | rollback | deploy v1, v2, `dcd rollback` | upstream back to v1, no migration run, `status` shows rollback | as above |
| IT-004 | resume | kill dcd between cutover and finalize (inject failure in `migrate:after`) | exit 4; `dcd deploy --resume` finishes; state `active` | as above |
| IT-005 | concurrent lock | two `dcd deploy` in parallel | one runs, other exit 3; killed holder's flock reclaimed | as above |
| IT-006 | conditional recreate | redeploy with unchanged managed image | postgres/nginx `--no-recreate` (same container id) | as above |
| IT-007 | env at rest + reboot survival | deploy with a 4-layer chain incl. a secret value; **chain dir outside `deploy_root`, passed via `--env-dir`** | `docker inspect` shows the value in container config; `docker stop`+`start` (env-less shell) preserves it; **no file under `deploy_root` contains the value** (recursive grep over dcd-written files) | rm containers/network; shred chain files |

### 10.3 Invariant → test map

| INV | Test |
|-----|------|
| INV-1 | TC-013, IT-002 |
| INV-2 | TC-021 + TC-020 (no auto-rollback; exit 4 → resume) |
| INV-3 | TC-015 (record at cutover), TC-020, IT-004 |
| INV-4 | TC-023, IT-005 |
| INV-5 | TC-019 |
| INV-6 | TC-017, TC-019 |
| INV-8 | TC-022 |
| INV-9 | TC-011 |

---

## 11. Error Handling Matrix

### Infrastructure / Docker errors

| Error | Detection | Response | Fallback | Phase/Exit |
|-------|-----------|----------|----------|------------|
| `docker pull` fails | nonzero | abort | red untouched, black not started | pre-cutover / 1 |
| managed `wait` exhausts | poll hits `retries` | abort | red untouched | pre-cutover / 1 |
| `migrate:before` fails | nonzero (throwaway) | abort | red untouched; schema may be partially applied — expand-contract must keep it red-compatible (§9) | pre-cutover / 1 |
| `start:black` name collision | exists | abort with the name; hint orphan-reap | — | pre-cutover / 1 |
| healthcheck never passes | retries exhausted | remove black, keep red | red serving | pre-cutover / 1 |
| `nginx -t` validate fails | nonzero | restore prev upstream, abort | red serving (config untouched) | pre-cutover / 1 |
| cutover reload fails | reload nonzero | **restore prev upstream**, remove black | red serving (on-disk + in-memory both = red) | pre-cutover / 1 |
| `drain:red` fails | nonzero | warn, continue, still `rm -f` old | black live | post-cutover / 4 |
| `migrate:after` fails | nonzero | stop, report | black live; `--resume` after fix | post-cutover / 4 |
| worker gen/up fails | nonzero | report | black serving HTTP; workers degraded; `--resume` | post-cutover / 4 |
| Docker daemon unreachable | first call errors | abort | nothing changed | pre-cutover / 1 |
| SIGINT/SIGTERM pre-cutover | interrupt flag | cleanup (remove black), release lock | red serving | / 130 |

### Operator / config errors

| Error | Message | Exit | Recovery |
|-------|---------|------|----------|
| missing `${VAR}` no default | `config: ${REGISTRY} is not set` + file/line | 2 | export the var, add it to a chain layer, or write `${VAR:-default}` |
| dotenv syntax error | Symfony-format context: `<msg> in "<file>" at line N` + snippet + caret (§5.2.1) | 2 | fix the named line |
| `$(command)` in a dotenv value | `command expansion is not supported; remove $(...)` + file/line | 2 | compute outside, pass the result |
| reserved key in a chain layer or env map | `<KEY> in <file-or-config-path> is reserved (configures dcd's own tooling)` — the chain variant appends `; set it via release.run.env if a container needs it` (config-map variants omit it: no legitimate delivery exists for those keys) | 2 | move/remove the key |
| env-map key with whitespace/`=`/other bad chars | `invalid env key <key> in <owner> (letters, digits, and underscore only, not starting with a digit)` — guards argv/YAML injection, incl. via Lua `ctx.cfg` | 2 | fix the key |
| `--env-dir` missing directory | `env dir <path> does not exist` | 2 | fix the path |
| `--env-file` base missing | `env file <path> does not exist (checked <path>.dist too)` | 2 | fix the path or create the base |
| circular chain references | `Too many levels of variable indirection in env vars: <NAMES>.` (after 5 deferred passes) | 2 | break the reference cycle |
| NUL byte in a dotenv document | `Loading files containing NUL bytes is not supported.` + context | 2 | fix the file encoding |
| `compose.env_file` present | `compose.env_file was removed — dcd no longer writes an env file; see UPGRADE.md` | 2 | delete the key |
| `--env-stdin` on a prompting command without `--yes`, or stdin is a TTY | `--env-stdin consumes stdin; pass -y/--yes` / `--env-stdin requires piped input` — both refused at arg-parse, before the lock | 2 | add `--yes` / pipe the document |
| rollback/resume env drift | warning: `release <id> ran with env keys [+ADDED/-REMOVED] vs current chain` (not an error) | — | verify the chain before proceeding |
| unknown config key | `config: unknown key 'servces' (did you mean 'services'?)` | 2 | fix key |
| stage not found / ambiguous | `stage 'staging' not found; known: beta, prod` / `multiple stages; pass one of: …` | 2 | pass stage |
| lock held | `another deploy holds prod (pid 4123 since 16:40)`; dead pid → reclaimed | 3 | wait/retry |
| host guard mismatch | `stage prod expects host prod.example.internal, this is beta-box` | 5 | run on right host |
| deploy finds `cutover_pending` | `prod has an incomplete release <c>; run 'dcd deploy --resume prod' or 'dcd rollback prod'` | 4 | resume/rollback |
| rollback no previous / images gone | `no previous release for prod` / `target image <tag> not present and not pullable` | 1 | — |
| Lua error | plugin path + traceback | 10 | fix plugin |

---

## 12. Architecture Decision Records

| ADR | Decision | Rationale | Alternatives rejected |
|-----|----------|-----------|-----------------------|
| 001 | Run **on the target server**, local socket | matches today; file-gen needs local fs; simplest | SSH-remote (deferred), pluggable transport (v2) |
| 002 | **Fixed linear recipe + before/after hook slots** | logic written/tested once; PHP-Deployer feel without a general DAG (gate-refined) | generic task graph w/ cycle detection (over-built); pure declarative YAML |
| 003 | **Embedded Lua (mlua), sandboxed** | proven, liked, no system Lua; effects only via `ctx` | native Rust plugins (recompile), WASM (heavy) |
| 004 | **Shell out to `docker` CLI** | parity; `--dry-run` prints real cmds; `compose` has no API | Bollard, hybrid |
| 005 | **Code-only rollback; forward-only migrations** | expand-contract; down-migrations lossy | down-migrations; block-on-migrate |
| 006 | ~~Env-var `${VAR}` secrets, `compose.env` 0600 on disk~~ **superseded by ADR-011** (2026-07-28) | (historical) CI-native, no new dependency | sops/age, Vault/SSM |
| 007 | **One config, stages over base** + host guard | DRY, single source; server = CI SSH target | file-per-stage; independent stages |
| 008 | **Zero Lua for the common case** | best DX; YAML drives the recipe | Lua-first; scaffolded recipe |
| 009 | **Adaptive output** (TTY/plain/json) | right output everywhere from one stream | plain-only; json-only |
| 010 | binary **`dcd`**, config **`dcd.yaml`** | user choice (round 2) | `deployer`, `redblack` |
| 011 | **Dotenv-chain secrets, process-env passthrough, nothing dcd-written at rest** (§5.2; supersedes ADR-006) | secrets live at the launch source (recovery); Docker's create-time env baking makes reboot-restart work with no on-disk file; bare `-e KEY` removes the `ps` leak. Honest cost: the command contract becomes argv **plus** a recorded env (§2.3) — a copy-pasted dry-run line needs the named keys exported first | keep 0600 files (residue + no recovery); temp-file-then-delete (crash leaves secrets); `--env-file /proc/self/fd/N` via memfd (needs `libc`+`unsafe` and fd-lifetime plumbing across the spawn for no observable gain over passthrough); sops/age & Vault/SSM (still out, blueprint §7) |
| 012 | **Parser = Rust port of `symfony/dotenv` 8.1 (raw lexing + deferred ≤5-pass chain resolution), proven by its ported test suite; `$(cmd)` errors instead of executing** | the chain must parse the user's real Symfony files byte-identically; every surveyed crate fails (dotenvy/dotenv: per-file substitution scope + `${VAR:-default}` silently empty — read from source; darkweb-dotenv: abandoned beta, env-mutating, `regex` dep; ruby-lineage precedence is first-wins, the opposite of Symfony) | dotenvy + layering wrapper (cannot fix parse-time substitution); any listed crate; hand grammar without the ported suite (unproven parity) |

---

## 13. Assumptions & Open Questions

### Assumptions (labeled; what changes if wrong)

| Assumption | If wrong |
|------------|----------|
| The server has `docker` with the `compose` plugin (today's CI relies on it) | `config check` / a preflight `docker version` probe must error early |
| `docker exec {nginx} curl http://{black_name}:port` resolves the black by **container name** over `{network}` before cutover | the healthcheck container must be on `{network}`; validation forbids alias use, but the network-attachment is a documented requirement |
| Black and red co-exist briefly (RAM for 2 app containers) | already true today; rollback keeps images, not running containers (INV-6) |
| Workers carry `com.docker.compose.project={project}` + name prefix `{workers.name_filter}` | `name_filter` is config-driven (§5); adjust per project |
| `release_id = epoch seconds` is unique enough | orphan-reaping + name-collision guard turn a clash into a clear error, not corruption |
| `migrate:after` (and any post-cutover command) is idempotent under re-run | `--resume` re-runs it after a later-stage failure; if a project's command is not idempotent, record a per-release `migrated_after` marker in state and skip on resume |
| `docker`/`docker compose` on the target read env for `${VAR}` substitution and bare `-e KEY`/bare-list passthrough as verified on Docker 29.4.0 / Compose v2 (2026-07-28 live tests) | these are documented, long-stable CLI contracts; IT-007 re-verifies on the CI daemon — a regression there fails the e2e, not production |
| The chain-as-interpolation-source also feeds `registry`/`deploy_root` defaults (one rule, no exceptions — deviation from the design-review recommendation to keep those process-env-only) | if a stray `.env` redefining `deploy_root` proves a real hazard, narrow `inject_env_default` to process-env-only; `dcd check` printing the chain files makes the stray visible first |

### Open Questions

| # | Question | Why it matters | Blocks | Proposed default |
|---|----------|----------------|--------|------------------|
| OQ-1 | chown preflight dirs via a `busybox` container vs assume a privileged uid | the original script chowns directly (implies privilege) | `preflight` impl | busybox-container chown (unprivileged); adopted in §7.1 |
| OQ-2 | Should `config check` optionally **lint** the expand-contract contract (flag destructive `before` migrations)? | enforces ADR-005 | a v1.1 feature | defer; document the contract, opt-in linter later |
| OQ-3 | `serde_yaml` is in maintenance mode — pin it or use `serde_yml`/`saphyr` | dependency longevity | crate choice | pin a maintained YAML crate at impl start; isolate behind `config` |
| OQ-4 | ~~mechanism to pin compose's implicit `.env` discovery off~~ **resolved 2026-07-28:** `--env-file /dev/null` verified on Compose v2 (suppresses discovery, no error); adopted in §5.2.4 | — | — | — |

---

## 14. References

| Topic | Location | Anchor |
|-------|----------|--------|
| Strategy, 7Q, scope | [Strategic Blueprint](strategic-blueprint.md) | §1–7 |

---

## 15. Appendix: synthetic second config (TC-028 fixture)

A deliberately non-acme project — **no Postgres, static workers, a self-/web-exec healthcheck, different ports, no migrations** — proving the recipe is config-driven. TC-028 builds the plan + argv against this with zero core changes; if the recipe hard-coded any acme assumption (a `postgres` service, an `nginx`-named host, a dynamic worker provider, a migration step), this fixture fails.

```yaml
version: 1
project: blogapp
network: blogapp_net
registry: ${REGISTRY}
docker:
  images: { app: ${APP_TAG}, web: ${WEB_TAG} }
  services:
    web:
      image: web
      container: blogapp-web
      recreate: on-image-change
      wait: { exec_in: blogapp-web, cmd: 'wget -qO- localhost/up', retries: 20, interval: 1s }
compose:
  files: [compose.prod.yml]
  env:
    COMPOSE_PROJECT_NAME: blogapp
    COMPOSE_IGNORE_ORPHANS: 'true'
    REGISTRY: ${REGISTRY}
    APP_TAG: ${APP_TAG}
    WEB_TAG: ${WEB_TAG}
release:
  image: app
  container_prefix: blogapp-app
  run: { network_alias: app, restart: unless-stopped }
  healthcheck: { exec_in: blogapp-web, cmd: 'wget -qO- http://{container}:9000/up', retries: 30, interval: 1s }
  drain: 'php artisan app:shutdown'
  # no migrate: block -> migrate:before / migrate:after are skipped
cutover:
  upstream_file: upstream.conf
  template: 'set $backend "{backend}";'
  backend_port: 9000
  fallback_backend: '127.0.0.1:9000'
  reload: { exec_in: blogapp-web, cmd: 'nginx -s reload' }
workers:
  name_filter: 'worker-'
  drain: 'php artisan queue:restart'
  stop_timeout: 60s
  compose_file: workers.yml
  provider: { static: [default, mail] }      # static, not a dynamic provider command
  template:
    image: app
    entrypoint: ['php', 'artisan', 'queue:work']
    command: ['{name}', '--max-time=3600']
    stop_signal: SIGTERM
    stop_grace_period: 60s
    restart: unless-stopped
retention: { keep_releases: 3 }
stages:
  prod: { host: blog.example.internal }
```

Expected plan differences the test asserts: tasks `migrate:before`/`migrate:after` are **skipped** (no `release.migrate`); `infra` reconciles only `web`; `workers` generates exactly `worker-default` + `worker-mail` from the static list; healthcheck/cutover/reload run via `blogapp-web`, not an `nginx` container.
