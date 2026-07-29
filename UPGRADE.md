UPGRADE FROM 0.2 to 0.3
=======================

`--env-stdin` on its own is now the WHOLE chain: no `.env` is discovered next to
`dcd.yaml`. Before, a stdin-only run still absorbed an implicitly discovered
`.env`/`.env.local` from the config directory — an application's own dotenv sitting
beside `dcd.yaml` was shipped, dev credentials included, into every container.

 * If you piped secrets on stdin AND relied on those implicit file layers, add
   `--env-dir .` (or `--env-file <base>`) to keep them — both still stack a stdin
   layer on top

 * Run `dcd check <stage>` and compare the `env: loaded …` lines before and after
   upgrading; a shrunken key set means you needed the flag

 * `--env-file` and `--env-dir` runs are unaffected, as are runs without `--env-stdin`


UPGRADE FROM 0.1 to 0.2
=======================

Env is loaded from a dotenv chain where dcd runs and passed through the process
environment (`-e KEY`). dcd writes no env file. See `docs/implementation-spec.md` §5.2.

Check the installed binary with `dcd --version` (`-v`).

Config
------

 * Remove `compose.env_file`, use the `compose.env` map instead — it is injected into
   the process environment of every `docker compose` call

 * Add the dotenv chain next to `dcd.yaml` (`--env-dir` overrides). Later wins, and
   the real process environment wins over every file. Only keys defined in a chain
   file are delivered to containers:

   ```
   .env (or .env.dist) → .env.local → .env.<stage> → .env.<stage>.local → --env-stdin
   ```

 * The parser and layering are a port of symfony/dotenv 8.1: cross-layer `${VAR}`
   references resolve deferred (a later layer overriding `REDIS_HOST` rewrites an
   earlier `redis://${REDIS_HOST}`, forward references work, self-referencing
   `${VAR:-default}` sees the pre-chain value), circular references are an error

 * If the project has its own `.env`/`.env.<stage>` files, point dcd at a separate
   base with `--env-file .env.deploy` — the whole chain rebases onto it
   (`.env.deploy` → `.env.deploy.local` → `.env.deploy.<stage>` →
   `.env.deploy.<stage>.local`) and the app's files are never read

 * Remove `DOCKER_*`, `COMPOSE_*`, `PATH`, `HOME`, `LD_*`, `BUILDX_*` and proxy vars
   from chain files, they are refused

 * Add `env_include`/`env_exclude` (full-match regexes, exclude wins) to
   `release.run` and `workers.template` to filter per container

 * `release.run.env_file` is unchanged

Compose
-------

 * Remove service-level `env_file`, use bare `environment` names instead — compose no
   longer reads an implicit `.env` (`--env-file /dev/null` is pinned)

   *Before*
   ```yaml
   services:
     app:
       env_file: compose.env
   ```

   *After*
   ```yaml
   services:
     app:
       environment:
         - DATABASE_URL
         - APP_SECRET
   ```

 * Compose runs with `deploy_root` as its working directory, relative `-f` paths
   resolve against it

 * Recreate workers with `dcd deploy`, the generated workers compose file holds key
   names only and no longer works with a hand-run `docker compose up`

Runtime
-------

 * `docker run -e KEY=VALUE` becomes `-e KEY`, export the keys before replaying a
   printed command by hand

 * `dcd check <stage>` prints the loaded chain files and the key names each container
   class receives

 * Rollback and resume re-read today's chain and warn when the delivered key set
   differs from the release's `env_keys` in `dcd-state.json`

Server
------

 * Remove the leftover env file and rotate its secrets

   ```bash
   rm <deploy_root>/compose.env
   ```
