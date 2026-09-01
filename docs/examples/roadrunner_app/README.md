# Wiring `dcd` into an existing deploy

Replaces a hand-rolled deploy script with `dcd` + [`dcd.yaml`](dcd.yaml) and
[`docker-compose.prod.yml`](docker-compose.prod.yml). The build stage is unchanged (it still
produces the app / database / nginx images); the **deploy** stage becomes one
`dcd deploy prod` from the checkout. dcd runs on the CI runner and drives the server over SSH
— nothing is copied there by hand.

## What maps to what

| the original script did | `dcd` does |
|---------------------|------------|
| red-black `docker run` + nginx-upstream cutover | the built-in `docker-redblack` recipe |
| conditional postgres/nginx recreate on image change | `services.*.recreate: on-image-change` (the default) |
| drain workers before DB recreate | `services.postgres.on_recreate_drain_workers: true` |
| `app:db:migrate before/after` | `release.migrate.before/after` |
| `app:worker:list` → workers compose | `workers.provider.command_in_release` + one `worker` compose service |
| `app:realtime:config` + `docker cp` + start | the `hooks.after_healthcheck` block |
| `bin/graceful-stop.sh` graceful drain | `release.drain` |
| `docker image prune -a` (deleted rollback targets!) | state-based retention — **fixed**, plus `dcd rollback` |
| no rollback / no lock / no crash recovery | `dcd rollback`, the stage lock, and `dcd deploy --resume` |

## The deploy stage

```yaml
deploy:
  stage: deploy
  tags: [default]
  needs: [build]
  rules:
    - if: '$CI_COMMIT_REF_NAME == "master" && ($CI_PIPELINE_SOURCE == "push" || $CI_PIPELINE_SOURCE == "web")'
  before_script:
    - eval $(ssh-agent -s)
    - echo "$SSH_PRIVATE_KEY" | tr -d '\r' | ssh-add -
    - mkdir -p ~/.ssh && chmod 700 ~/.ssh
    - ssh-keyscan -p "$SSH_PORT" "$SSH_HOST" >> ~/.ssh/known_hosts
  script:
    # Fetch the dcd binary — it runs HERE, on the runner, not on the server.
    - 'curl -fsSL -o dcd "$DCD_BINARY_URL" && chmod +x dcd'

    - ssh $SSH_USER@$SSH_HOST -p $SSH_PORT
        "docker login -u $CI_REGISTRY_USER -p $CI_REGISTRY_PASSWORD $CI_REGISTRY"

    - export DEPLOY_SSH="$SSH_USER@$SSH_HOST"
    - ./dcd deploy prod --yes --image app=$DOCKER_IMAGE_TAG_APP

    - ssh $SSH_USER@$SSH_HOST -p $SSH_PORT "rm ~/.docker/config.json 2>/dev/null || true"
```

No `scp`, no `mkdir`, no `chmod +x` on the server, no remote `cd`. `dcd.yaml` and
`docker-compose.prod.yml` live in the repo and are uploaded by the `sync` step at the start of
every deploy. `deploy_root` is created if missing.

**Note the shape of the secrets.** The old pipeline put `MAXMIND_LICENSE_KEY=…` on the remote
`ssh` command line, exposing it in the server's process list. dcd reads the dotenv chain next
to `dcd.yaml` and streams the values over ssh **stdin**, so no value appears in any argv on
either machine. Keep a secret-free `.env` in the repo naming the keys and let CI export the
values, or pipe a document with `--env-stdin`.

`--image app=$DOCKER_IMAGE_TAG_APP` names a **compose service** and pins that exact image ref
for the release. The database and nginx tags come from the compose file's own `${…}`
variables, which dcd passes through from the resolved chain.

For a non-default ssh port or a jump host, put it in `~/.ssh/config` and set `DEPLOY_SSH` to
the `Host` alias — dcd does not re-implement ssh configuration.

## Operating it

```
dcd deploy prod           # the red-black deploy
dcd status prod           # current release + history
dcd rollback prod --yes   # re-point to the previous release (no migrations)
dcd deploy --resume prod  # finish a deploy that died after cutover
dcd deploy prod --dry-run # print the whole plan, touch nothing
```

## Notes

- **`docker-compose.prod.yml` declares the app and its workers too**, each with
  `profiles: ["dcd-release"]` so a hand-run `docker compose up` never starts a second copy
  beside the release. dcd creates them with `docker compose run` and addresses them by name
  afterwards.
- **Every service dcd manages declares a `healthcheck:`.** Mandatory — `dcd check` fails
  without it, because `compose up --wait` returns as soon as a gate-less container is
  *running*. The app is the exception: its image carries no probe, so `release.healthcheck`
  runs `curl` from the nginx container against the new container by name.
- The nginx service must bind-mount the upstream file dcd writes
  (`${DEPLOY_ROOT}/nginx-upstream.conf`). dcd writes it on the target; only the compose file
  can put it inside the router.
- **dcd uploads the compose documents and nothing they reference.** Bind-mount sources and
  `env_file:` targets must already exist on the target; `directories:` creates the
  directories, not their contents. `dcd check` warns, naming each path.
- Chain values reach containers as bare `-e KEY`, values travelling over ssh stdin — nothing
  is written to disk on either machine.
- `host: <prod-hostname>` under `stages.prod` makes dcd refuse to run unless the **target**
  reports that name. Worth more now than in v1: an ssh alias can be repointed.
