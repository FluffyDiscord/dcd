# A PHP-FPM app deployed red-black with `dcd`

A production-shaped example: a **Symfony app whose release image is nginx + php-fpm under
supervisord** (HTTP on `:8080`), deployed zero-downtime with [`dcd`](../../..) across two
stages (`prod` + `beta`) on a single host.

Where some apps **are** the HTTP server (RoadRunner, FrankenPHP), here nginx terminates HTTP
inside the container and speaks FastCGI to php-fpm. The dcd wiring is identical either way:
dcd cuts over an HTTP port; what serves it is up to the image.

## The shape

```
                          ┌─────────────── one host ───────────────┐
  TLS (system nginx) ──►  router :8080  ──proxy──►  webapp-app-<rel>   (release, red-black)
                          (dcd cutover              └ nginx :8080 ──FastCGI──► php-fpm :9000
                           target)                  webapp-mariadb, webapp-meilisearch,
                                                    webapp-scheduler        (side containers)
```

dcd builds `webapp-app-<new-release>` beside the live one, health-checks it **through** the
router (`curl http://{container}:8080/health`), then rewrites the router's upstream file and
reloads nginx. The flip is atomic; the old container drains gracefully.

## Files

```
dcd.yaml                  the deploy config (prod + beta stages)
docker-compose.prod.yml   EVERY container, the red-black app included: app, router, mariadb,
                            meilisearch, scheduler
docker-compose.beta.yml   beta-only overlay: adds mailpit + a loopback DB port
router/                   the dcd cutover-target nginx (CI-built, pushed to the registry): Dockerfile, router.conf, maintenance.html
host-nginx.example.conf   reference vhost for the SYSTEM nginx (TLS terminator → router :8080/:8081)
.env.deploy.app.example   template for app secrets   → copy to .env.deploy.app   (0600)
.env.deploy.infra.example template for infra secrets → copy to .env.deploy.infra (0600)
app-image/                REFERENCE (lives in the app repo, baked into the image — not shipped to the server):
  supervisord.conf          runs php-fpm + nginx; the drain socket
  nginx/release.conf        the in-container nginx on :8080 (health probe, static, FastCGI)
  php/fpm-pool.conf         the php-fpm pool
  drain.sh                  release.drain: graceful stop of nginx then php-fpm
  scheduler.sh              the scheduler loop, gated by SCHEDULER_ENABLED
```

`dcd.yaml` and the compose files live in the **repo**; dcd uploads them to `$DEPLOY_ROOT` at
the start of every deploy. The `.env.deploy.*` files are the exception — compose reads them by
relative path, and dcd uploads the compose *documents* and nothing they reference, so those
stay operator-managed on the server (`dcd check` warns, naming each such path). `app-image/`
is shown only to complete the release side of the picture.

## Why FPM changes almost nothing

| Concern | How this example handles it |
|---|---|
| What dcd cuts over to | an HTTP port (`:8080`) — nginx-in-the-image serves it, exactly like RR would |
| Health probe | `/health` → nginx → php-fpm → `health.php` (no kernel, no DB) — fails fast if fpm is down |
| Graceful drain | `release.drain` → `drain.sh` → `supervisorctl stop nginx php-fpm` (QUIT, finishes in-flight) |
| DB migrations | `release.migrate.before` runs `doctrine:migrations:migrate` on the new release before cutover |
| Periodic jobs | a `scheduler` side container loops `schedule:run`, gated per-stage by `SCHEDULER_ENABLED` |

No messenger workers here. Add them later and the `workers:` block stays global, so every
stage consumes.

## Two stages, one config

`prod` and `beta` share one `dcd.yaml`, namespaced by a **pinned** `project` per stage:

| | `prod` | `beta` |
|---|---|---|
| `project` | `webapp` | `beta-webapp` |
| network | `webapp_default` | `beta-webapp_default` |
| containers | `webapp-*` | `beta-webapp-*` |
| router port (loopback) | `8080` | `8081` |
| extras | scheduler runs (`SCHEDULER_ENABLED=1`) | + mailpit sink, + loopback DB port |

`project` is pinned rather than derived from the `deploy_root` folder name, so the two stages
coexist on one host regardless of their paths — and a folder name with a dot in it (which
Compose v2 rejects as a project name) can never leak in. Every container name, the network and
`COMPOSE_PROJECT_NAME` come from `{project}`: see `'{project}-app'` as the container prefix in
`dcd.yaml`, and `${COMPOSE_PROJECT_NAME}` per `container_name` in the compose file.

## One-time bootstrap (per stage)

```sh
# On the SERVER (dcd itself now runs on the CI runner, not here):
cd "$DEPLOY_ROOT"

# 1. Registry auth is EPHEMERAL. The CI deploy job logs the server in with job-scoped creds
#    (docker login --password-stdin), runs dcd, then docker logout. Do the same for a manual deploy;
#    do NOT leave a persistent login on the box.

# 2. Secrets (Docker env-file format — never shell-sourced). Fill in real values.
cp .env.deploy.app.example   .env.deploy.app   && chmod 600 .env.deploy.app
cp .env.deploy.infra.example .env.deploy.infra && chmod 600 .env.deploy.infra
#   INVARIANT: MARIADB_USER/PASSWORD/DATABASE (.infra) == user/pass/db in DATABASE_URL (.app)

# 3. Persistent data dirs (owned by the image's www-data uid 1000)
mkdir -p data/mysql data/meili data/uploads data/private data/log
chown -R 1000:1000 data/uploads data/private data/log

# 4. Wire the system nginx: adapt host-nginx.example.conf (set X-Forwarded-Proto $scheme!), enable, reload.
```

dcd generates `nginx-upstream.conf` itself — do not hand-edit it. The network is declared in
the compose file and created by `compose up`; dcd only inspects it, to fail early with a clear
error. Compose gets its variables via the process environment, from the resolved dotenv chain.

Alternative to step 2's at-rest files: keep app secrets in a `.env.prod.local` next to
`dcd.yaml`, or stream them with `--env-stdin`. dcd then delivers them to the app, migrate and
worker containers as bare `-e KEY` with the values riding ssh stdin, writing nothing to the
server. The infra side containers still use their compose `env_file:`, which is why those two
files remain operator-managed on the target.

## Operating it

```sh
dcd check prod                 # validate the config (stage merge, interpolation, rules)
dcd deploy prod --dry-run      # print the whole plan, touch nothing
dcd deploy prod --image app=<tag>
dcd status prod                # current release + history
dcd rollback prod --yes        # re-point to the previous release (runs NO migrations)
dcd deploy prod --resume       # finish a deploy that died after cutover
```

CI runs the deploy; by hand it is:

```sh
cd path/to/this/checkout          # where dcd.yaml and the compose files live
REGISTRY=<registry-image> DEPLOY_ROOT=/srv/app DEPLOY_SSH=deploy@host \
  dcd deploy prod --image app=<tag>
```

Run it from the **checkout**, not from `$DEPLOY_ROOT`: dcd runs on the deploying machine and
reaches the target over ssh, so `dcd.yaml` and the compose files are read here while
`DEPLOY_ROOT` names a path over there.

`REGISTRY` and `DEPLOY_ROOT` come from the environment (CI sets them per job), so they are not
repeated in `dcd.yaml`. `--image app=<tag>` threads in the tag the build stage produced.
