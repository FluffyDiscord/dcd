# Wiring `dcd` into an existing deploy

This replaces a hand-rolled deploy script with `dcd` + [`dcd.yaml`](dcd.yaml). The build stage is
unchanged (it still produces the app / database / nginx images); only the **deploy**
stage changes — it ships the `dcd` binary + `dcd.yaml` and runs `dcd deploy prod`.

## What maps to what

| the original script did | `dcd` does |
|---------------------|------------|
| red-black `docker run` + nginx-upstream cutover | the built-in `docker-redblack` recipe |
| conditional postgres/nginx recreate on image change | `docker.services.*.recreate: on-image-change` |
| drain workers before DB recreate | `docker.services.postgres.on_recreate_drain_workers: true` |
| `app:db:migrate before/after` | `release.migrate.before/after` |
| `app:worker:list` → workers compose | `workers.provider.command_in_release` + `template` |
| `app:realtime:config` + `docker cp` + start | the `hooks.after_healthcheck` block |
| `bin/graceful-stop.sh` graceful drain | `release.drain` |
| `docker image prune -a` (deleted rollback targets!) | state-based retention — **fixed**, plus `dcd rollback` |
| no rollback / no lock / no crash recovery | `dcd rollback`, the stage lock, and `dcd deploy --resume` |

## The deploy stage (replaces the previous CI deploy stage)

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
    - '[[ -f /.dockerenv ]] && echo -e "Host *\n\tStrictHostKeyChecking no\n\n" > ~/.ssh/config'
  script:
    # Fetch the dcd binary built by the docker-compose-deployer pipeline
    # (GitLab job artifact / package registry / release — pick your source):
    - 'curl -fsSL -o dcd "$DCD_BINARY_URL" && chmod +x dcd'

    - ssh $SSH_USER@$SSH_HOST -p $SSH_PORT
        "docker login -u $CI_REGISTRY_USER -p $CI_REGISTRY_PASSWORD $CI_REGISTRY"
    - ssh $SSH_USER@$SSH_HOST -p $SSH_PORT "mkdir -p $DEPLOY_ROOT"

    - scp -P $SSH_PORT dcd            $SSH_USER@$SSH_HOST:$DEPLOY_ROOT/dcd
    - scp -P $SSH_PORT dcd.yaml       $SSH_USER@$SSH_HOST:$DEPLOY_ROOT/dcd.yaml
    - scp -P $SSH_PORT docker-compose.prod.yml $SSH_USER@$SSH_HOST:$DEPLOY_ROOT/docker-compose.prod.yml
    - scp -P $SSH_PORT bin/graceful-stop.sh $SSH_USER@$SSH_HOST:$DEPLOY_ROOT/bin/graceful-stop.sh
    - ssh $SSH_USER@$SSH_HOST -p $SSH_PORT "chmod +x $DEPLOY_ROOT/dcd"

    - ssh $SSH_USER@$SSH_HOST -p $SSH_PORT
        "cd $DEPLOY_ROOT &&
         REGISTRY=$CI_REGISTRY/$CI_PROJECT_PATH
         DEPLOY_ROOT=$DEPLOY_ROOT
         MAXMIND_ACCOUNT_ID=$MAXMIND_ACCOUNT_ID
         MAXMIND_LICENSE_KEY=$MAXMIND_LICENSE_KEY
         ./dcd deploy prod
           --image app=$DOCKER_IMAGE_TAG_APP
           --image database=$DOCKER_IMAGE_TAG_DATABASE
           --image nginx=$DOCKER_IMAGE_TAG_NGINX"

    - ssh $SSH_USER@$SSH_HOST -p $SSH_PORT "rm ~/.docker/config.json 2>/dev/null || true"
```

`dcd.yaml` lives in the repo and is `scp`-ed each deploy. The same CI variables as before
are reused (`SSH_*`, `DEPLOY_ROOT`, `MAXMIND_*`, the registry vars). `--image …=$DOCKER_IMAGE_TAG_*`
threads in the tags the build stage produced.

## Operating it

```
dcd deploy prod          # the red-black deploy
dcd status prod          # current release + history
dcd rollback prod --yes  # re-point to the previous release (no migrations)
dcd deploy --resume prod # finish a deploy that died after cutover
dcd deploy prod --dry-run # print the whole plan, touch nothing
```

## Notes

- `docker-compose.prod.yml` is unchanged; `dcd` renders `compose.env` and the workers
  compose file the same way the original script did.
- The `nginx` prod image must still `include` the upstream file `dcd` writes
  (`nginx-upstream.conf`) — that wiring already exists in acme's nginx config.
- Add `host: <prod-hostname>` under `stages.prod` to make `dcd` refuse to run on the
  wrong machine.
