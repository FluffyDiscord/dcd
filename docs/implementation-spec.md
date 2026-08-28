# dcd — Implementation Spec (Implementation)

**v2 revision 2026-08-25.** dcd runs on the **deploying machine** and reaches the target over SSH (ADR-001 v2, ADR-014); **compose owns container definition** and dcd owns orchestration policy (ADR-013). v1 configs are not accepted — there is no migration path, by explicit decision.

**Document type:** Implementation
**Status:** v1 specified 2026-06-11 and **implemented**, including the §5.2 environment rework (specified 2026-07-28, shipped in 0.5.x — `src/dotenv/` exists and is wired at `src/cli.rs`). **v2 — SSH transport + compose-owned containers, this revision, 2026-08-25: specified, NOT implemented.**
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
| INV-1 | Red serves continuously **on the cutover path** until black passes healthcheck **and** the cutover reload returns 0. Note the limit: `--use-aliases` (§7.6) puts the black behind the release service's network aliases from creation, so *alias-addressed internal* traffic (`http://app:8080` from another container) reaches the black before it is healthy. Callers that must not see an unhealthy black address the release through `cutover`, not the alias. Any failure **before** that point leaves red serving and removes black. |
| INV-2 | **The point of no automatic return is the cutover reload exit 0.** Failures after it (`drain:red`, `migrate:after`, `workers`) never auto-rollback; black is already live. They stop the run, report, and exit `4`. A brief dual-serving window exists while nginx old workers drain in-flight requests — benign for idempotent HTTP; noted for non-idempotent POST/WS. |
| INV-3 | The black **release** is **recorded in state at cutover** (status `cutover_pending`) before `drain:red`. `finalize` flips it to `active` and advances `current`. A crash between cutover and finalize therefore leaves a recoverable, recorded live release (recovered by `dcd deploy --resume`). The only earlier state write is the pull ledger (INV-11); it adds no `releases`/`current` change of its own, so recovery reads what it would have read without it. |
| INV-4 | **The stage lock is released automatically, on the target, in bounded time — no contender ever judges whether a holder is alive.** Local runs keep `flock(2)` (kernel-released on any exit, SIGKILL included). Remote runs hold `flock(2)` **on the target**, owned by a process whose lifetime is a *lease over the ssh channel*: dcd stages a lease script (`echo <token>` then `while read -t {lease} _; do :; done`) and runs `flock -n <lock> sh <lease>` as bare words, fed a heartbeat every `{lease}/3` seconds. The script is a FILE, not `flock -c`: the command line has to survive an unknown login shell, and only bare words do (§2.7). dcd dying (or its ssh client dying) closes the channel → EOF → the remote process exits → the **kernel** releases the lock. A severed network leaves the channel open but silent → `read -t` times out → same exit, same kernel release, bounded by the lease. The v1 guarantee therefore survives the move: the lock cannot outlive its holder, and `dcd unlock` is an operator override, never a required repair. |
| INV-5 | Rollback never runs migrations (ADR-005); it re-deploys the previous release's images. |
| INV-6 | Rollback to the **immediately previous** release is available while `keep_releases ≥ 1` (0 is rejected at load); its images are retained (never pruned). Retention spares `rollback_target()`'s release explicitly rather than relying on it falling inside `keep_releases` — after a same-tag redeploy the target sits outside that window, since `rollback_target` skips releases carrying the serving image. Deeper/repeated rollback is bounded by `keep_releases` + registry retention; §4 verifies the target's images exist before acting. |
| INV-8 | If a stage declares `host:` and the **target's** hostname does not match, `dcd` refuses to act (exit `5`). Locally that is `gethostname()`; over SSH it is one command on the target, run before the lock and before any mutation. **Matching rule:** the target runs `hostname -f`, falling back to `hostname` when that fails; the guard passes if the reported name equals `host:` **or** `host:` starts with `<reported>.` — otherwise a correctly-configured box that reports a short name would fail against the FQDN every example uses. SSH makes this guard *more* load-bearing, not less: an ssh alias, a bastion, or a copy-pasted stage can all point at the wrong box. |
| INV-9 | If `state.current` is set but that container is **not running**, the upstream file is reset to `cutover.fallback_backend` **before** any managed-service recreate, so a recreated nginx never points at a dead container (self-heal; mirrors the original script). |
| INV-10 | **At most one `cutover_pending` release exists at any time.** The cutover append (§7.8) demotes any pre-existing `cutover_pending` → `rolled_back` in the same atomic state write, so a crash during a recovery run can never leave two. `serving` is therefore unambiguous. |
| INV-12 | **No env value ever appears in any argv, on either machine.** Locally this is `Command::envs()`; over SSH the process env does not cross the wire, so each remote command runs as `ssh <target> -- sh -c 'set -a; . /dev/stdin; exec <argv>'` with a `KEY='value'` document on **stdin**. Verified 2026-08-25: a document piped to a shell leaves nothing in `ps -eo args`. `KEY=VALUE` on an ssh command line is forbidden — it would expose every secret in the *target's* process list, strictly worse than v1. **Scope:** this binds every argv dcd builds. A Lua plugin can defeat it deliberately — `ctx.env(k)` returns the value and `ctx.run{...}` puts whatever it is given into an argv — because a plugin is operator code running in dcd's own process, with the operator's own secrets. dcd does not sanitise it; a plugin that wants a value in an argv gets one, and it will reach the target's `ps` and the `-v` trace. Use `ctx.in_release` / bare `-e KEY` delivery instead. |
| INV-13 | **dcd addresses its own containers by name, through plain `docker`, never through compose's inventory.** `compose run` is used purely as a *creation* primitive; the result is an ordinary container that dcd then inspects, execs, stops and removes by name. Compose's handling of one-off containers (`com.docker.compose.oneoff=True`) has changed across releases and is deliberately outside dcd's blast radius — notably, `docker compose ps` **hides** one-off containers, so it is never a source of truth for the release container. |
| INV-14 | **The release container is never matched by worker discovery.** Under ADR-013 the release container carries `com.docker.compose.project`, so a project-plus-name-prefix filter can match it — and worker drain issues `docker stop` on every match, post-cutover, against the container serving traffic. Workers are therefore discovered by `label=com.docker.compose.service={workers.service}`, which cannot match the release service. |
| INV-11 | **Every tag dcd pulls is recorded before it is pulled** (`stages[].pulled[]`, §7.13), so image GC's world is a census of what dcd put on this host — not only of what reached cutover. A tag leaves the ledger only when `docker image rm` confirms it gone. Plugins cannot edit the ledger: it survives the `ctx.state` round-trip unchanged. |

---

## 2. Architecture

### 2.1 Process model

Single-threaded, synchronous orchestration (no async runtime): a sequence of `docker` CLI invocations with retry/timeout loops (`std::process::Command` + `std::thread::sleep`). Keeps the binary small, control flow linear, and `--dry-run` a runner swap. A caught-signal flag (§2.5) is polled between tasks and inside retry loops.

```
parse args ─▶ load+merge config ─▶ resolve stage ─▶ open ssh master (if ssh:)
   ─▶ host guard (remote `hostname`) ─▶ sample release_id ─▶ acquire stage lock
   ─▶ load Lua plugins (LOCAL files) ─▶ build plan (recipe + hooks)
   ─▶ execute plan (each task via Context) ─▶ release lock (RAII)
   ─▶ report   (the ssh master is left to `ControlPersist`; see §2.7)
```

**Where each thing runs.** The engine, the config, the state machine, the dotenv
chain, the Lua host and all failure handling run on the **deploying machine**.
Only `docker`/`docker compose`/`hostname` invocations and the handful of file
operations dcd owns cross the wire. Lua plugins are read and executed locally —
nothing on the target ever reads a `.lua` file — so `plugins:` resolves against
the **config file's directory**, not `deploy_root`.

### 2.2 Modules

| Module | Responsibility | Key types |
|--------|----------------|-----------|
| `cli` | clap commands/flags → `Action` | `Cli`, `Command`, `Action` |
| `config` | parse `dcd.yaml`, stage-merge, `${VAR}` interpolation, `--set` overrides, identity defaults (project from deploy_root folder, `{project}` token expansion), validation | `Config`, `Stage`, `RawConfig`, `ConfigError` |
| `dotenv` | Symfony-port parser + chain loader (§5.2): 4-file chain + stdin layer, the two resolved maps, per-container filters, reserved-key guard | `DotenvParser`, `EnvChain`, `ResolvedEnv` |
| `effects` | the testability seam: all side effects behind traits | `CommandRunner`, `FileSystem`, `Clock` |
| `effects::real` | production impls, local and remote | `SystemRunner`, `SystemFs`, `SystemClock`, `SshRunner`, `SshFs` |
| `ssh` | **pure** `Argv` → `Argv` wrapping, POSIX quoting, the option baseline, ControlMaster lifecycle | `SshTarget`, `SshOptions` |
| `effects::record` | recording / read-pass-through impls for tests + `--dry-run` | `RecordingRunner`, `MemoryFs`, `FixedClock` |
| `docker` | typed helpers building docker/compose argv over `CommandRunner`; tags each call read-only or mutating | `Docker`, `Compose` |
| `engine` | fixed ordered recipe + before/after hook slots, expansion, execution | `Engine`, `Task`, `HookSlot`, `Plan`, `Context` |
| `recipe` | the `docker-redblack` recipe: registers tasks from `Config` | `redblack::register` |
| `lua` | sandboxed mlua host: globals + `ctx` userdata, plugin loading | `LuaHost` |
| `state` | `dcd-state.json` read/write **through `FileSystem`**, release lifecycle, rollback target, retention | `State`, `Release`, `ReleaseStatus` |
| `lock` | per-stage lock behind the effects seam: `flock(2)` locally, atomic `mkdir` remotely, + stale-holder reclamation (RAII) | `StageLock` |
| `ui` | adaptive reporter (rich TTY / plain / `--json`) over an event stream | `Reporter`, `Event` |
| `signal` | catches SIGINT/SIGTERM → interrupt flag | `Interrupt` |
| `error` | typed error → exit-code mapping | `DcdError`, `ExitCode` |

### 2.3 The effects seam (why this is fully testable)

Every side effect goes through one of three traits; `Context` holds trait objects, never concrete impls.

```rust
enum Access { Read, Mutate }
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

- **State *reads* and the lock must move onto the seam.** State *writes* already use it (`engine.rs` `persist_state` → `fs_write`); the gaps are the read and the lock. In v1 `cli.rs:594` reads `dcd-state.json` with `std::fs` and mapped **any** error to `State::default()`. With a remote `deploy_root` that silently yields empty state — no rollback target, no `cutover_pending` refusal — and `finalize` then overwrites the target's real state with the empty one. Every state read/write and the lock therefore go through `FileSystem`, and a **missing** state file is distinguished from an **unreadable** one: missing → fresh state; unreadable → `DcdError`, never a silent default.
- **Remote production:** `SshRunner` wraps each `Argv` (see `ssh` module) and `SshFs` implements the five file operations as `mkdir -p` / `cat` / `test -e` / `rm -f` / an **always-atomic** `cat > path.tmp && mv path.tmp path`. Atomicity is not optional: `persist_state` is the INV-3 write, and a torn `dcd-state.json` fails every later command on that stage, `unlock` included.
- **Production (local):** `SystemRunner` spawns real processes — since the §5.2 rework, with `current_dir = deploy_root` and the chain runner env (plus per-command overlays), so the effective command contract is **argv + recorded env**, not argv alone; bare `-e KEY` flags in dry-run output are the manifest of what rides the env.
- **Tests:** `RecordingRunner` (canned outputs keyed by argv prefix; records calls) + `MemoryFs` + `FixedClock` → engine/recipe/Lua exercised with **zero Docker**, asserted against exact recorded argv.
- **`--dry-run`** is *not* a fully-synthetic runner. It is **read-pass-through** (§2.4): `Access::Read` commands execute for real (so current image state, running workers, transport lists are truthful); `Access::Mutate` commands are stubbed (printed as planned actions, return synthetic ok). Points that cannot be resolved without a mutation having happened (e.g. the dynamic worker provider, which execs in the not-yet-started black) are emitted as `⚠ data-dependent` lines, never silently defaulted.

### 2.4 Read vs mutate classification (drives dry-run honesty)

| `Access::Read` (run for real in dry-run) | `Access::Mutate` (stubbed in dry-run) |
|---|---|
| `docker inspect`, `docker ps [-a]`, `docker images`, `docker network inspect`, `docker version` | `docker run`, `docker rm`, `docker stop`, `docker pull`, `docker cp`, `docker image rm`, `docker network create` |
| reads of state/upstream files, remote `hostname` | `docker exec` that runs a project command (migrate, drain, healthcheck against black), `docker compose up/run`, the `nginx -s reload`, writes of upstream/override/state files, **every upload**, `mkdir` of the lock |

**`--dry-run` does not take the lock.** It mutates nothing, so its `Read` commands may observe a concurrent deploy mid-flight and the printed plan may be stale. dcd therefore *checks for* the lock (a `Read`) and prints `⚠ a deploy holds <stage>; this plan may be stale` when present, rather than acquiring it.

**Uploads and the lock are mutations.** In a draft of this design the upload step sat outside the recipe and outside `Access`, which would have made `dcd deploy --dry-run` write files to the production target — before the host guard and before the lock. Both are now ordinary `Mutate` effects inside the recipe, so `--dry-run` prints them and touches nothing.

The dynamic worker provider (`exec` in black) is `Mutate`-adjacent: in dry-run there is no black, so it emits `⚠ worker set is dynamic (provider command); not resolvable in dry-run` rather than "0 workers".

### 2.5 Signals, lock, and crash recovery

- **Lock (local):** `flock(2)` (LOCK_EX|LOCK_NB) on `${deploy_root}/.dcd.{stage}.lock`. The OS releases it on any process exit, including SIGKILL — so it never goes stale from a crash. A sidecar `.dcd.{stage}.lock.meta` (pid, ISO start, stage) is written for the human "who holds it" message; because the flock itself is the source of truth, if `dcd` can acquire the flock any meta present is treated as stale and overwritten. Failure to acquire → exit `3` with the meta details. `dcd unlock` (§4.4) deletes both files whether or not the flock is currently held — the one deliberate override, for a hung deploy that will never release it.
- **Lock (remote, INV-4):** the lock is a real `flock(2)` **on the target**, held by a leased process:
  ```
  ssh <opts> <target> -- flock -n {deploy_root}/.dcd.{stage}.lock \
        -c 'while read -t {getLockLeaseSeconds()=30} _; do :; done'
  ```
  Every remote write on this path goes through `send_checked`, which **fails on a non-zero remote exit**. Without that, an ssh that connects but whose script fails — a read-only `deploy_root`, no `base64` on the target — returns `Ok`, the lease file is never written, and `acquire` mistranslates the whole class into `LockHeld`: "another deploy holds prod" against a target where nothing is running.

  The lease loop and the `.meta` sidecar are written by the **one** target-write builder (`ssh::write_file_script`), which embeds the payload as base64 inside the script. Nothing may be appended to a script for the remote `sh` to `cat` off its own stdin: sh buffers the whole stream, so the file lands **empty** — an empty lease script exits 0 without ever taking the lock, and every deploy then reads that as "the stage is held". That defect shipped and was caught by IT-009, not by any unit test.

  dcd writes a heartbeat line every `getLockHeartbeatSeconds()=10` on that channel's stdin, and a sibling `.dcd.{stage}.lock.meta` records host, pid and ISO start for the human message. **Both death modes were reproduced 2026-08-25:** killing the client closed the channel and the lock was released in **51 ms** (EOF, not the lease); withholding heartbeats while holding the channel open released it after **2 s** against a 3 s lease. While heartbeating, a competing `flock -n` failed as it must.
- **Why this and not the alternatives.** An atomic `mkdir` mutex is exclusive but *inert*: nothing on the target ever releases it, so a killed dcd leaves it forever and recovery becomes a human judgment about a pid on another machine — which is unknowable, and which turns every "is it stale?" into a race between an operator and a slow-but-alive deploy. Age-based auto-reclaim is worse: it steals the lock from a live holder that is merely slow (a large pull, a long migration), and then two deploys race on one state file. The lease removes the judgment entirely — the lock is either heartbeating or gone. `docker create --name` as a mutex is atomic but equally inert.
- **The cost, stated:** this needs `flock(1)` on the target (util-linux). Verified present in `alpine:3.20`, `debian:12-slim` and `ubuntu:24.04`; the target already runs Docker, so it is a real Linux host. `preflight` probes `command -v flock` and fails with a named error rather than silently deploying unlocked.
- **The stage lock does not protect cross-stage concurrency on a shared `deploy_root`.** `dcd-state.json` is explicitly cross-stage (retention subtracts other stages' tags), and v2 adds two more shared files. dcd therefore makes every state write a **read-modify-write of one stage's row**: `persist_state` re-reads `dcd-state.json`, replaces only `stages[{stage}]`, and writes the merged document. A concurrent stage's history — its `cutover_pending` record included — survives a write it did not make, which whole-document writes silently discarded. The per-stage generated files are named `dcd-image-override.{stage}.yml` and `dcd-release-run.{stage}.yml`.

  **Residual, stated:** the merge narrows the lost-update window to the gap between that re-read and the write; it does not close it. Closing it needs a lock the remote transport can hold across a round trip, which the leased-flock machinery could provide at the cost of an ssh session per state write (three or more per deploy). Not taken — the observed exposure is two stages finalising within milliseconds of each other on one `deploy_root`.
- **Signals:** a `signal` handler sets an atomic `Interrupt`. The executor checks it between tasks and inside retry loops. Interrupt **before** cutover → pre-cutover cleanup (remove black) + lock release via normal unwind, exit `130`. Interrupt **during** cutover → finish the in-flight reload-or-restore deterministically (never leave the upstream half-written), then exit. SIGKILL cannot run cleanup; the lock auto-releases in **both** modes (locally the kernel, remotely the leased holder — INV-4); `--resume`/orphan-reaping (§7.1) recover the rest.
- **Over SSH, Ctrl-C does not reach the in-flight remote command.** dcd runs without a PTY (a PTY would mangle the `--format` output dcd parses, so `-tt` is forbidden), so an interrupt kills the local `ssh` client while the remote `docker run` continues. dcd therefore waits for the in-flight command to return and *then* runs `cleanup_black`, rather than racing it.

### 2.6 Container & release naming

`release_id = clock.now_epoch()`, sampled **exactly once** at plan start and stored on `Context.release.id`; black container = `{release.container_prefix}-{release_id}` (matches today's `acme-app-rr-<epoch>`). Every `{release_id}`/`{container}` reference reads the stored value — never re-samples the clock. Preflight orphan-reaping (§7.1) removes any `{container_prefix}-*` container absent from state, covering same-second/crash leftovers.

### 2.7 SSH transport (ADR-014)

`ssh:` is a **scalar**: `deploy@prod.example.internal`, a bare host, or a `Host`
alias from the operator's `~/.ssh/config`. Absent ⇒ everything runs locally and
the v1 behaviour is reproduced exactly (this is what the unit suite and
`tests/e2e.rs` drive). `--ssh <target>` overrides it for CI.

dcd deliberately does **not** model `port`, `identity`, or an options list.
OpenSSH already resolves those per host, and `ProxyJump`, bastions, `IdentityAgent`
and per-host `User` all come free from `ssh_config` — none of which a fixed field
list can express. Re-spelling them would also add four fields to the
four-representation sync burden for zero capability.

**Option baseline** (dcd supplies these on every invocation; getters hold the
defaults per CR5, and the operator's `ssh_config` still wins where OpenSSH says
it does):

| Option | Value | Why |
|--------|-------|-----|
| `BatchMode` | `yes` | a passphrase prompt in CI hangs until the job timeout |
| `ConnectTimeout` | `getConnectTimeoutSeconds()` = **10** | bounds a dead host |
| `ServerAliveInterval` / `ServerAliveCountMax` | `getServerAliveIntervalSeconds()` = **15** / `getServerAliveCountMax()` = **4** | 15×4 = **60 s**, and that number *is* the stale-lock window INV-4 depends on |
| `ControlMaster` / `ControlPath` / `ControlPersist` | `auto` / `%C` under `$XDG_RUNTIME_DIR/dcd/` when set, else a `0700` `~/.dcd/cm/` / `getControlPersistSeconds()` = **60** | see below |
| `StrictHostKeyChecking` | **not set** — OpenSSH's default stands | defaulting to `accept-new` is silent TOFU into production; an unknown key must be a clean `BatchMode` failure |

**Connection reuse is mandatory, and measured.** 60 commands over loopback,
2026-08-25, against a real sshd (**n=1, no variance**): **8879 ms without
multiplexing (148 ms/command) vs 1685 ms with it (28 ms/command)** — about
**120 ms of per-command setup removed**. Read that as a *lower bound on the
benefit*, not as a deploy-time budget: over a real link both figures grow by at
least one RTT and the unmultiplexed one grows by several (TCP handshake plus key
exchange), so loopback is the conservative case for the multiplexing argument and
an unreliable basis for absolute estimates. A deploy issues roughly
`12 + |services| + 3×|workers|` commands (~70 for the worked example: 4 services,
10 workers).

**The master is never torn down explicitly.** `ControlPath` is `%C`, which hashes `%l%h%p%r` and nothing process-specific, so every dcd run on one runner to one `user@host` shares a single master. `ssh -O exit` asks that master to exit and takes **every session multiplexed over it** with it — so `dcd status` could kill a concurrent `dcd deploy` mid-cutover, and the read-only commands take no stage lock to prevent it. `ControlPersist` = 60 reaps the socket instead.

**`ControlPath` is dcd's to choose, and it must be short.** The socket path is
capped near 108 bytes; exceeding it makes ssh **fail outright**, not fall back to
an unmultiplexed connection. This was reproduced live during the measurement
above — every command died with `ControlPath too long`, and because ssh then
fails *fast*, the run initially looked 5.7× faster than the unmultiplexed one.
dcd therefore builds the path itself as `%C` under a short directory and
preflights its length with a named error.

**The script travels on stdin; the login shell sees one bare word.** ssh joins
everything after the destination and hands it to the **login shell of the deploy
user** — which dcd does not choose, and which may be fish, tcsh or ksh, whose
quoting grammars differ from POSIX. Putting a script there means it is parsed
twice, by two grammars. dcd therefore invokes `ssh <opts> <target> -- sh` and
pipes the script to that `sh` on **stdin**: every shell agrees what one bare word
means, and the script is then parsed exactly once, by the POSIX shell dcd named.
`quote()` is the only grammar in play.

**Consequence — stdin carries the script, so it cannot also carry a payload.**
Verified: a shell reading a script from a pipe does not hand the remainder to the
command it runs. So uploads embed their bytes as **base64 inside the script**
(the alphabet contains no shell metacharacter), and the stage lock — whose stdin
*must* stay a live heartbeat channel — is the one command invoked as **bare
words**: `ssh … -- flock -n <lock> sh <lease-file>`, with the lease loop written
to that file beforehand through the ordinary script mechanism. Nothing in that
argv needs quoting, which is why `deploy_root` is validated to contain no
whitespace or shell metacharacters.

**This was found by running a real deploy, not by reading the code.** An earlier
design put `sh`, `-c`, `<script>` on the ssh argv as separate words; the
argv-shape unit tests passed throughout, because the defect lives in what ssh
does *between* the two argv layers. `tests/ssh_shells.rs` now covers it against a
real sshd with nine login shells (busybox sh, dash, bash, zsh, mksh, loksh, yash,
fish, tcsh), asserting byte-identical argument delivery, env delivery with no
argv exposure, and that a failed `cd` aborts instead of running in `$HOME`.

**Quoting is a pure, unit-tested function.** `ssh target -- a b c` does not
`execve`; the remote **login shell re-parses** the joined arguments, so `Argv` is
no longer a syscall-level boundary and its safety rests entirely on quoting
against a shell dcd does not choose. The wrapper therefore lives in `src/ssh.rs`
as a pure `Argv → Argv` builder mirroring `src/docker.rs`, unit-asserted against
hostile fixtures (`$`, backticks, newlines, single quotes, `!`, whitespace-only
arguments). `SystemRunner`'s `current_dir = deploy_root` has no ssh equivalent,
so the wrapper also emits the `cd <deploy_root> &&` prefix — and that prefix is
part of the same tested function, never string-built at a call site.

**Transport failure is distinct from remote failure.** `ssh` exits **255** for
its own errors (auth, DNS, connection reset). Treating that as an ordinary
non-zero would let `Engine::classify` report a severed connection as an
application failure, and the best-effort `try_run` sites (`drain:red`,
`cleanup_black`, `remove_images`) would swallow "the network is gone" and keep
issuing commands. `SshRunner` distinguishes the two and aborts with
`connection to <target> lost after <step>; the target's state may lag — run
dcd status <stage>`.

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
| 3.5 | pin + `sync` | write `dcd-image-override.{stage}.yml` from the target release's recorded `images` map, then run `sync` (§7.0). **This is what makes rollback deterministic**: without the pin, the compose file's `image: ${REGISTRY}:${APP_TAG}` would resolve to whatever the environment says *now* (§5.5) |
| 4 | `preflight` → `ensure_upstream` → `pull` → `infra` → `start:black` | spin a fresh black from the **target** images. Note `infra` is included, so managed services are reconciled from the **current** compose files — rollback replays the previous release's *images*, never its container *spec* (OQ-5) |
| 5 | `healthcheck` → `cutover` | same gates as deploy; record `cutover_pending` at reload=0 |
| 6 | `drain:red` + `workers` | drain the rolled-back-from release; restart workers on the target image |
| 7 | finalize | flip the **fresh black** (recorded `cutover_pending` at step 5) → `active` and set `current` = fresh black; mark the rolled-back-from **serving** release → `rolled_back`; the **target** stays `superseded` (it was only the image source); record a history entry `{from: serving, to: fresh-black, source: target, reason}`. *Same shape as deploy finalize — the serving record is always the fresh black, never the image-source target.* |

**No migrations run** (INV-5). `dcd status` flags releases that ran migrations so the operator knows the schema is ahead of the rolled-back code (expand-contract makes that safe).

### 4.2 `dcd deploy --resume [stage]` (recovers an exit-4 state, INV-3)

If state holds a `cutover_pending` release (a prior deploy died/​failed after cutover), `--resume` re-runs **only** the post-cutover tasks (`drain:red` → `migrate:after` → `workers` → `finalize`) against that recorded live black — it does **not** start a new black or re-cutover. Resume reads `current` and the `cutover_pending` release from the on-disk state; `drain:red` on resume re-checks whether the old `current` container is still running (the failed run likely already removed it) and skips it if gone (§7.10). Because the `drained` flag is per-process, resume re-runs the worker drain (safe: `workers.drain` is best-effort and the `workers` task recreates the set). `migrate:after` re-runs and so must be idempotent (Doctrine version-tracking skips applied migrations). Without `--resume`, a `deploy` that finds a `cutover_pending` release refuses (exit `4`) and tells the operator to `--resume`, `rollback`, or `unlock` (§4.4). As a defensive guard, any run that finds **more than one** `cutover_pending` (which INV-10 forbids) aborts with a clear state-corruption error rather than guessing.

### 4.3 State transitions (authoritative)

`current` = the last finalized release. **`serving`** (the container actually receiving traffic) = the `cutover_pending` release if one exists, else `current`. Exactly one `cutover_pending` exists at any time (INV-10). Each event below is one atomic state write.

| Event | Effect on state |
|-------|-----------------|
| cutover reload=0 (deploy **or** rollback) | append `R_new{ status: cutover_pending }`; **demote any other `cutover_pending` → `rolled_back`** (INV-10) |
| `drain:red` | no state write; drains containers (see §7.10) |
| finalize (deploy) | `R_new → active`; the release that was `current` at run start → `superseded`; `current = R_new` |
| finalize (rollback) | `R_new → active`; the rolled-back-from `serving` → `rolled_back`; the run-start `current` (if different from `serving`) → `superseded`; the image-source **target stays `superseded`**; `current = R_new`. *(superseded vs rolled_back is informational only — both are retained and rollback-eligible, so no conflict when target == run-start current.)* |
| unlock (§4.4) | the newest `cutover_pending` → `active`; any other pending → `rolled_back` (INV-10 repair); the run-start `current` → `superseded`; `current` = promoted release |
| pull (§7.3) | append to `stages[].pulled[]` and persist, before pulling (INV-11); no release/`current` write |
| retention | evict beyond `keep_releases` (§7.13); drop the ledger rows of tags removed; mark each fully-torn-down release `reaped` so the next run does not re-evict it |

Because finalize always demotes the **run-start `current`** (not merely "the previous release"), a recovery run cleanly resolves a stale `current` left by a crashed deploy: no release is left `active`-but-not-`current`, and no container is left running-but-unrecorded (§7.10 reaps it).

### 4.4 `dcd unlock [stage]` (the escape hatch)

The last resort when `--resume` cannot finish and `rollback` is not wanted — typically a post-cutover step that keeps failing (`migrate:after`, `workers`) or a deploy process that hung holding the lock. `unlock` **accepts the release that is already live as the outcome of the deploy** and returns the stage to a clean, deployable state, so the *next* `dcd deploy` can run fresh and fix whatever is wrong. It is a **pure state repair**: it runs no Docker command at all (over SSH it still runs the host guard, the lock read and the lock removal — none of which touch Docker).

| # | Step | Detail |
|---|------|--------|
| 1 | host guard | as deploy/rollback (INV-8, exit `5`) — an escape hatch on the wrong box is still the wrong box |
| 2 | report the lock | **Local:** if a live process holds the flock, warn naming the `.meta` holder. **Remote:** report host/pid/start from `.dcd.{stage}.lock.meta`. A lock that is still held is by definition still heartbeating (INV-4), so unlock's warning is unambiguous: a live deploy IS running. Either way the warning precedes the prompt: clearing the lock lets a second deploy start alongside the first, whose next state write would overwrite this unlock |
| 3 | confirm | `Mark <container> as the successful release on <stage> and clear the lock?`; requires `--yes` when non-interactive (declined → exit `1`) |
| 4 | promote | the newest `cutover_pending` release → `active`/`current` per the §4.3 table; a second pending (corrupt state) is demoted rather than refused — `unlock` is what an operator reaches for *because* state is broken. `--reason` is recorded on the promoted release |
| 5 | persist | one state write, mode `0600`, as any finalize |
| 6 | clear the lock | **Local:** delete `.dcd.{stage}.lock` and its `.meta`, overriding a held flock. **Remote:** delete `.dcd.{stage}.lock` and its `.meta`. Under INV-4 a stale remote lock cannot exist, so this is an operator override of a *live* holder, never a repair — the prompt says so |

**Nothing else happens:** no `drain:red`, no `migrate:after`, no `workers`, no retention/image GC, and **no** hooks (YAML or Lua). A stuck deploy is usually stuck on exactly those, so none of them may stand between the operator and a deployable stage — and a broken or unreachable Docker daemon cannot block the repair. Every leftover is the next deploy's job: `preflight` reaps unrecorded containers (§7.1), `drain:red` reaps every stale app container, and `finalize` applies retention. `unlock` warns by name about each thing it left behind — the after-migration, the workers, and the previous container still running.

With no `cutover_pending` release (a deploy that died *before* cutover, or a lock left behind by a killed process) `unlock` touches no state at all and only clears the lock. `--dry-run` prints the transition and the lock files it would remove, writing nothing. The command is idempotent: a second run reports nothing to promote and no lock present.

---

## 5. Configuration schema (`dcd.yaml`)

`${VAR}`/`${VAR:-default}` interpolate from the **resolved environment** (process env layered over the dotenv chain, §5.2) at load; missing var, no default → `ConfigError` (exit `2`).

**The organising rule (ADR-013): compose declares containers; dcd declares orchestration policy.** Every field v1 had for image, env, env_file, volumes, restart, network alias, entrypoint and command is gone from `dcd.yaml` because the operator already writes it in their compose file, and dcd reads it back resolved. What remains is what compose cannot express: which service is cut over to, how it is health-gated, how traffic is switched, migrations, drain, worker discovery, recreate policy, retention, and hooks.

Below is an annotated tour of every field. The **measured** claim attaches to the worked example, not to this annotated block: `docs/examples/roadrunner_app/dcd.yaml` went **74 → 40 substantive YAML lines** (`grep -cve '^\s*$' -e '^\s*#'`, at fa6f6e4 vs now) (measured, not estimated: the same production deploy, counted with `grep -cve '^\s*$' -e '^\s*#'`).

```yaml
version: 2
project: acme                          # naming prefix for containers/lock/state; default: deploy_root folder name

ssh: deploy@prod.example.internal      # scalar target, or an ~/.ssh/config Host alias. OMIT => run locally (§2.7)
deploy_root: ${DEPLOY_ROOT}            # abs path ON THE TARGET (default: $DEPLOY_ROOT, else cwd)
registry: ${REGISTRY}                  # OPTIONAL, used ONLY by `dcd gc --all` as the ownership proof for
                                       # repositories dcd may prune. Images themselves come from compose.
host: prod.example.internal            # INV-8 guard, checked against the TARGET's hostname; omit to disable

compose:
  files: [docker-compose.prod.yml]     # -f files; stage compose.files APPENDS (§5.1). Uploaded by `sync` (§7.1)
  profiles: [dcd-release]              # profiles enabled when RESOLVING the model (default: [dcd-release]);
                                       # never passed to `up` — see §7's preamble
  env: {}                              # OPTIONAL override map. By default the whole resolved chain is passed
                                       # to compose as process env (§5.2.4) — v1 made you re-list every key

directories:                           # created on the TARGET before the deploy
  - { path: .docker/logs, owner: '1000:1000' }
  - { path: .docker/centrifugo }

release:                               # the red-black app
  service: app                         # a service in compose.files. THE one required release field
  container_prefix: acme-app           # default: {project}-{service}  ->  acme-app-<release_id>
  # Health gate — exactly one of these two. Default: the compose service's OWN
  # `healthcheck:` block; dcd polls docker inspect .State.Health.Status.
  healthcheck:                         # required only when the service declares no healthcheck:
    exec_in: nginx                     #   a compose SERVICE name (v1 wanted a container name)
    cmd: 'curl -sf http://{container}:2114/health'
    retries: 60                        #   default: 60
    interval: 2s                       #   default: 2s
  migrate:
    before: 'php bin/console app:db:migrate before --no-interaction'   # throwaway container
    after:  'php bin/console app:db:migrate after --no-interaction'    # exec in black
  drain: 'bin/graceful-stop.sh'        # graceful stop in the old container
  # run: {...}                         # the no-compose-service fallback; full field list in §5.3

cutover:
  service: nginx                       # a compose service; dcd resolves its container name
  backend_port: 8080
  upstream_file: nginx-upstream.conf   # default
  template: 'set $backend "{backend}";'          # default; {backend} = <black_container>:<backend_port>
  fallback_backend: '127.0.0.1:8080'             # default; INV-9 self-heal target
  validate: { exec_in: nginx, cmd: 'nginx -t' }  # optional pre-reload syntax check
  reload:   { exec_in: nginx, cmd: 'nginx -s reload' }

services:                              # POLICY ONLY. Identity (container name, image, readiness) is DERIVED
  postgres: { on_recreate_drain_workers: true }  # from compose; declare only what compose cannot say.
  valkey:   { recreate: never }                  # recreate: on-image-change (default) | always | never
  # nginx:  { wait: { cmd: '...' } }             # optional readiness override; compose's own healthcheck
                                                 # + `compose up -d --wait` is the default gate

workers:
  service: worker                      # ONE compose service; dcd runs N containers from it
  provider: { command_in_release: 'php bin/console app:worker:list --no-ansi --env=prod' }
  # provider: { static: [async, scheduler] }     # alternative
  drain: 'php bin/console messenger:stop-workers --env=prod'
  name_prefix: 'worker-'               # default; container naming only, NEVER discovery (INV-14)
  stop_timeout: 120s                   # default; `docker stop --timeout`. Compose's stop options are NOT
  stop_signal: SIGTERM                 # default; `docker stop --signal`. baked into a `compose run`
  exclude: [failed]                    # names dropped from whatever the provider returns  (default: none)
  args: ['--memory-limit=256M']        # appended after the service name for every worker  (default: none)

retention: { keep_releases: 1, keep_managed_images: 1, keep_images: {} }   # keyed by SERVICE name (§5.4)

plugins: [plugins/app.lua]             # resolved against THIS FILE's directory; read and run LOCALLY (§2.1)

hooks:                                 # zero-Lua extension path; actions in §6.3
  after_healthcheck:
    - exec_in_release: 'php bin/console app:realtime:config --output=/tmp/cfg.json --env=prod'
    - cp_from_release: { from: '/tmp/cfg.json', to: '.docker/centrifugo/config.json' }
    - compose: ['up', '-d', 'centrifugo']

stages:
  beta:
    host: beta.example.internal
    ssh: deploy@beta.example.internal
    compose: { files: [docker-compose.beta.yml] }   # APPENDS to base files (§5.1)
    retention: { keep_releases: 2 }
  prod: {}
```

### 5.1 Merge, interpolation, override (each tested, §10)

| Rule | Behaviour |
|------|-----------|
| Stage merge — maps | deep-merged; stage keys override base |
| Stage merge — scalars | stage replaces base |
| Stage merge — lists | stage list **replaces** base list — **except `compose.files`, which APPENDS** (compose `-f` is additive; the one ergonomic exception) |
| Interpolation | `${VAR}` / `${VAR:-default}` from the resolved environment (§5.2) at load; unresolved + no default → `ConfigError` |
| `--set path=value` | applied **after** interpolation, **before** validation; existing scalar paths only (a new path → error, preserving no-laundered-defaults) |
| `--image <service>=<ref>` | writes the generated image-override file (§5.5); the argument is now a **compose service name**, not a v1 logical alias |

**Validation** (`dcd check`) — errors:

| Rule | Why |
|------|-----|
| unknown keys → error | typo guard |
| `release.service`, `cutover.service`, `workers.service`, `services.*` keys, and every `exec_in` must name a service that exists in the resolved compose model | compose is the source of truth; a typo must not surface later as a healthcheck timeout |
| every service in `services:`, and the release service, must declare a health gate — a compose `healthcheck:` or a `services.<name>.wait` probe | **mandatory, checked against the resolved model.** `compose up --wait` returns as soon as a container is RUNNING when its service declares no `healthcheck:`, so a missing gate silently turns readiness into "started" — the not-yet-ready-database race that ordered recreate and `on_recreate_drain_workers` exist to prevent. It fails at `check`/model-resolution time, before the lock and before anything is touched |
| `workers.service` must not equal `release.service` | worker discovery is by service label (INV-14); sharing the service would make worker drain `docker stop` the release container mid-traffic. A single-image project must declare a second compose service for its workers (same image, different service) |
| the release service must declare a compose `healthcheck:`, or `release.healthcheck` must be set | otherwise the cutover has no gate (§7.7) |
| `directories[].path` must be relative and contain no `..` | it is interpolated into a root-equivalent `chown` inside a container mounting `deploy_root` (§7.1) |
| `workers.name_prefix` must not be a prefix of `release.container_prefix`, or vice versa | `docker ps --filter name=` is an unanchored regex; overlapping prefixes make the orphan reaper and `drain:red` sweep each other's containers |
| exactly one of `release.service` / `release.run` | two creation paths would be two chances to diverge from the red-black invariants |
| `healthcheck.cmd` (escape-hatch form) must reference `{container}` | red and black share the network alias; the alias resolves to red → false-positive health |
| `retention.keep_images` keys must be services dcd manages, and must not name `release.service` | `keep_releases` is the knob that bounds the app |
| a v1-only key (`docker.images`, `docker.services`, `release.image`, `workers.template`, `workers.compose_file`, `compose.env_file`) | targeted `ConfigError` naming the v2 replacement — a bare `deny_unknown_fields` "unknown field" would misdirect to the typo row of the AGENTS.md §7 table |

**Validation — warnings** (`dcd check` reports, deploy proceeds):

| Warning | Why it is a warning, not an error |
|---------|-----------------------------------|
| the release/worker service lacks any profile listed in `compose.profiles` | dcd never issues a bare `compose up` — every call site is service-qualified — so the profile is *hardening*, not a requirement. It stops a hand-run `docker compose up` from starting a competing app container. Making it mandatory would make dcd's correctness depend on a magic string inside a file dcd does not own, and would break that file for humans and other tooling |
| the `cutover.service` does not bind-mount `{deploy_root}/{cutover.upstream_file}` | dcd writes the upstream file on the target, but only the operator's compose file can put it *inside* the router. `check` scans the router service's `volumes:` for a source matching the path and warns when none matches — otherwise cutover reports success and traffic never moves, the likeliest silent failure in v2 |

| the release service declares `ports:` | dcd never passes `--service-ports`: two release containers cannot share a host port, which is the whole red-black premise. Traffic reaches the release through `cutover`. Silent otherwise, and the likeliest "why doesn't my app respond" question |
| `compose receives: [KEY…]` | §5.2's chain is passed to compose wholesale (a v1 explicit allow-list became a blanket pass); printing the key names keeps it observable |

### 5.2 Environment system (dotenv chain → ephemeral delivery)

> **v2 delivery note (INV-12).** The chain, its resolution and its filters are
> unchanged. What changed is the last hop. v1 relied on the spawned `docker` CLI
> **inheriting dcd's process env** (`Command::envs()`), which is what keeps values
> out of argv. Over SSH the local process env does not cross the wire —
> `SendEnv`/`SetEnv` need the target's `AcceptEnv`, which defaults to `LANG LC_*`
> — so every delivered key would arrive **unset**, silently, because compose
> resolves a bare name to null. Each remote command therefore runs as:
>
> ```
> ssh <opts> <target> -- sh -c 'set -a; . /dev/stdin; exec <quoted argv>'
> ```
>
> with a `KEY='value'` document on **stdin**. Values never enter argv, `ps`, the
> `-v` trace, `--dry-run` output, or disk, so §5.2.4's guarantee holds on both
> machines. Verified 2026-08-25: a document piped into a shell leaves nothing in
> `ps -eo args`.
>
> **Consequence — stdin has exactly one consumer per invocation.** The three ssh
> uses are therefore separate invocations, each with one purpose: a *command*
> (env document on stdin), an *upload* (file bytes on stdin; paths are not
> secret, so no env is needed), and `migrate:before`'s non-detached
> `compose run`, which must pass `-T` — compose's `run` is interactive by
> default, and dcd can never stream stdin to a container because stdin is spoken
> for.

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

**Layers 1–4 exist only when a file source was named.** `--env-stdin` **on its own suppresses implicit discovery entirely** (`chain_base()` in `src/cli.rs` returns `None`): the stdin document is the whole chain, nothing is probed on disk, and the `env: absent …` report is empty. Combining files with a stdin layer is explicit — `--env-dir` or `--env-file`. Rationale: implicit `.env` discovery is a convenience for the file-based workflow, and silently absorbing an application `.env` that happens to sit beside `dcd.yaml` would ship its keys — dev credentials included — into every container of a deploy whose operator asked for stdin-only secrets. This mirrors the `--env-file /dev/null` pin that already disables compose's own implicit `.env` discovery (§5.2.4).

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

A chain layer that defines any of `PATH`, `HOME`, `LD_*`, `DOCKER_*`, `COMPOSE_*`, `BUILDX_*`, `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY` (case-insensitive for the proxy set) → `ConfigError` naming the key **and the defining file**, plus the escape hatch (`release.run.env`). The same guard applies to the `compose.env` map and to `release.run.env` (the §7.6 fallback), with one carve-out: `compose.env` may set `COMPOSE_*` (deliberate config, e.g. `COMPOSE_PROJECT_NAME`, `COMPOSE_IGNORE_ORPHANS`), and `release.run.env` may set the proxy family (a container legitimately needs proxies; the acknowledged cost is that the value then rides that one command's docker-CLI child env — the overlay — during create). Rationale: the chain and overlays ride the child process env of `docker`/`docker compose`, which read their own configuration from it — a `.env` with `DOCKER_HOST` would silently retarget the deploy.

#### 5.2.4 Delivery

- **Runner-level env:** `SystemRunner` carries the **container-env map** (chain-derived, §5.2.2) and applies it via `Command::envs()` to **every** spawned child, and sets `current_dir(deploy_root)` on every child — fixing, for an **absolute** `deploy_root`, the pre-rework bug where compose resolved relative `-f` paths against dcd's cwd (a relative `deploy_root`, default `.`, still resolves against dcd's cwd exactly as before — cwd-launch workflows are unchanged). The `CommandRunner` trait signature is unchanged (`RunOpts` gains an optional per-command env overlay); hooks (`run:`), `ctx.run`, and compose substitution therefore all see one consistent environment. The remaining explicit config env maps are delivered as **per-command overlays** on top of the runner env, overlay wins: `compose.env` on every compose invocation, `release.run.env` on the release/migrate `docker run`, worker containers inherit `compose.env` alone — their env is declared on the compose service (§7.12). Precedence for any consumer is therefore: explicit map (overlay) > chain (runner env) > nothing — and a key in both an overlay and the chain is delivered once with the overlay's value.
- **`docker run` (release + migrate throwaway):** for every delivered key, a bare `-e KEY` flag — the docker CLI reads the value from its own (child) environment; values never appear in argv, `ps`, or on disk (verified live). Explicit `release.run.env` pairs are delivered the same way (bare `-e KEY`, value in a per-command env overlay via `RunOpts`), replacing the pre-rework `-e K=V`-in-argv leak; they override chain keys and `env_file` keys. `run.env_file` (operator-managed, the one sanctioned at-rest file) keeps its `--env-file` flag, emitted before all `-e` flags.
- **Compose:** no `--env-file` flag and no rendered file. `compose.env` + container env ride the child process env (compose substitutes `${VAR}` in compose files from its environment — verified). To keep compose's *implicit* `.env`-discovery from re-introducing filtered-out keys, every compose invocation passes `--env-file /dev/null` (verified live: suppresses discovery, no error); `dcd check` warns when `deploy_root/.env` exists and is not the chain's own `.env`.
- **Workers compose file:** `environment:` is a YAML list of bare key **names** (template `env` keys + filtered chain keys; template values ride the workers-`up` command overlay and win over chain on collision). The generated file contains no values — also closing the pre-rework YAML-injection hazard of unquoted `{key}: {value}` embedding. A bare name whose variable is unset at `compose up` resolves to *unset in the container* (compose `null`, verified), which keeps `env_exclude` honest. Consequence, stated deliberately: the generated workers file is **not operator-usable outside dcd** — a hand-run `docker compose up` without dcd's env resolves every bare name to unset.
- **Delivered set per container** = container env filtered by that container's `env_include` (empty = all) then `env_exclude` (exclude wins), plus its explicit `env` map. Filters are **full-match** regexes (`regex-lite`; an unanchored pattern like `MAILER` does *not* match `MAILER_DSN`). `-e` flags and generated key lists are emitted as a **sorted, deduplicated union** — a key in both the chain set and the explicit map appears once (deterministic argv for tests). Empty-valued keys are delivered as empty (verified: docker passes them; only *unset* keys are omitted).

#### 5.2.5 Observability & lifecycle

- **`dcd check [stage]`** prints: chain files found/skipped (paths), per-layer key counts, the sorted key **names** delivered to each container after filters, keys shadowed by the process env, and reserved-key errors. **Resolved values are never printed** (redaction stays removed — the right response is to never print values at all), and there is **no exception**. A dotenv **parse error** reproduces Symfony's snippet+caret (§5.2.1) in everything except the value bytes, which `mask_values` replaces 1:1 with `*` — key names, `=`, escaped newlines, the caret column and the offset all still match upstream. The earlier byte-exact form was a real leak, not a theoretical one: the window is 20 raw bytes either side of the cursor, so an apostrophe in one value — an ordinary password — printed the NEXT variable's value, and `check` is the CI lint job. Symfony parity loses to the standing promise; the parity oracle now pins the masked form. `--dry-run` prints bare `-e KEY` argv — strictly better than the pre-rework `-e K=V`.
- **State fingerprint:** each `Release` records `env_keys` (sorted container-env key names — names are not secrets; `dcd-state.json` stays `0600`). `rollback` and `--resume` diff the recorded set against the currently-resolved set and print a loud warning naming added/removed keys (env is *not* versioned — a rollback runs old images under **today's** chain; the warning is the guard). Value hashes are deliberately not stored (low-entropy secrets are offline-crackable from a hash).
- **Recovery model:** the chain files live at the launch source (operator machine, CI secrets, or `ssh host 'dcd deploy prod --env-stdin --yes' < .env.prod.local` for a genuinely disk-free path — `--env-stdin` consumes stdin, so interactive prompts error and `-y/--yes` is required for any confirming command). Server loss no longer loses secrets. Residual at-rest copies on the server, named deliberately: Docker's own container config under `/var/lib/docker` (inherent — it is what makes reboot-restart work; root-only), the optional operator-managed `release.run.env_file`, any `env_file:` entries inside user-owned compose files (UPGRADE.md shows the bare-key migration), and — in the default layout, where `--env-dir` is the config directory on the server — the operator's own chain files themselves; `--env-stdin` (or values-over-ssh with a secret-free `.env` manifest) is the path that removes that last class. The claim dcd itself makes is precise: **dcd writes no secret bytes to disk** (IT-007's grep proves it over dcd-written files).

---

### 5.3 `release.run` — the no-compose-service fallback

Declared **instead of** `release.service` (§5.1 makes them exclusive), for a project with no compose service for the app. Every v1 field is carried over unchanged, so no capability is lost:

```yaml
release:
  container_prefix: acme-app
  run:
    image: registry.example.com/acme:app-1a2b3c   # a literal ref (there is no docker.images map in v2)
    network: acme_default
    network_alias: app
    restart: unless-stopped
    entrypoint: ['php', 'bin/console']            # optional
    command: ['messenger:consume']                # optional
    env_file: app.env
    env: { TZ: UTC }
    env_include: []
    env_exclude: []
    volumes: ['/srv/acme/.docker/logs:/var/log/app']
  healthcheck: { exec_in: nginx, cmd: '…{container}…' }
```

dcd renders this into a **one-service compose document** at `dcd-release-run.{stage}.yml`, written **next to the config** at load time (`materialise_release_run`) and **appended to `compose.files`** — so from that point it is an ordinary compose file: it resolves in the model, `sync` uploads it, and it is passed as a `-f` before `dcd-image-override.{stage}.yml`. Writing it at load rather than in `sync` is what lets `docker compose config` see the service at all; a document produced later would leave the model without the release service and every reference to it undeclared.

The rendered service is named `{project}-release`, carries `profiles: ["dcd-release"]`, and that synthetic name is the key used for `Release.images`, `retention.keep_images` and `gc_logicals()` (§5.4), and the name `--image` addresses. The engine then runs the **same** `compose run` primitive against it — one creation path, one test surface.

**The document carries no env.** `run.env` values reach the container as bare `-e KEY` from the per-command overlay exactly as before (INV-12); emitting them into a file on disk would be dcd writing secret bytes. `run.env_file` is emitted as a path — it is the operator-managed at-rest file §5.2 already sanctions, and it must exist on the target.

Note the renderer's direction: this is *dcd-config → compose*, which is total (dcd owns every field it emits). ADR-013 rejects the opposite direction, *compose → docker run*, which is not.

### 5.4 Image identity is the compose service name

v1 keyed release images, the pull ledger and retention on `docker.images` logical
names (`Release.images`, `PulledImage.logical`, `KeepPolicy.per_logical`,
`gc_candidates(keep, logicals)`, and an `Engine::gc_logicals()` that hardcoded
`"app"`). That map is gone, so the whole axis is re-keyed to the **compose
service name** — a 1:1 replacement that leaves every retention algorithm intact,
makes `retention.keep_images: { postgres: 2 }` read better than the v1 form, and
lets `gc_logicals()` derive from the resolved compose services instead of
hardcoding a name. This is a `state.rs` schema rename with real cost: no
migration is written (v1 state is not read), but §10's state tests move with it.

### 5.5 The generated image-override file

`{deploy_root}/dcd-image-override.yml` is appended as the **last** `-f`, so it
wins. It contains nothing but image pins:

```yaml
services:
  app: { image: registry.example.com/acme:app-1a2b3c }
```

It exists because the app's image now comes from the operator's compose file,
typically as `image: ${REGISTRY}:${APP_TAG}` — a variable dcd does not own,
cannot validate, and cannot report a good error for when it is spelled
differently. The override file makes `--image <service>=<ref>` work regardless of
how the compose file names its variables, records an exact resolved ref in the
release ledger, and — decisively — is how **rollback pins the old image exactly**
rather than hoping the environment still reproduces it.

It carries image references only. It must never carry anything else: `docker
compose config` **resolves and inlines env values**, so snapshotting the resolved
model would write secret bytes into a dcd-owned file, breaking §5.2.5 and IT-007.

`--image` is applied by pinning the **resolved compose model** (`ComposeModel::pin_image`,
right after `resolve_compose_model`), never by a config path. `Engine::resolved_images`
is the model's only image reader, so one pin reaches the override file, the pull, the
pull ledger, retention and the release record together — there is no second image path
to keep in step. A pin naming a service the compose files do not declare is a config
error listing the services that exist (IT-018). v1's `--set docker.images.<name>` route
is gone with the key (§UPGRADE); it survived the v2 migration in `cli.rs` and made every
`--image` run fail at config load until IT-017 caught it.

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

**Data:** `ctx.cfg` (resolved config) and `ctx.state` (current stage: `{current, releases:[{id,container,status,images,ran_migrations,reason}]}`) are **live, mutable tables** — the engine refreshes them from the typed config/state before each hook and reads any direct assignment back (no setter function), so a plugin mutation changes the deploy: `cfg` for steps not yet run, `state` read back into deploy state and persisted past cutover. Full power — a `state` rewrite can violate the §4.3 invariants the engine relies on. `ctx.vars` + `ctx.set/get` is a **persistent scratch table shared across all hooks in the run** (not part of cfg/state). Plus `ctx.container`, `ctx.stage`. Structural values fixed at deploy start (the resolved compose model, container name, `deploy_root`) are snapshots, not re-read.

**Dry-run honesty (Clarity):** a Lua hook that branches on `ctx.in_release(...)` output gets the stubbed empty result in `--dry-run` (the black isn't started); the host cannot introspect the branch, so the dynamic worker provider and such data-dependent points emit a `⚠ data-dependent` event rather than a fabricated plan. Pure utilities and `cfg`/`state`/`env`/`read_file` resolve for real in dry-run.

### 6.5 The `configure` hook

Registered with `configure(fn)`; fires **once before the recipe**, with a host offering `run`/`read_file`/`write_file`/`file_exists`/`env`/utilities/`cfg`/`state` (no `in_release`/`docker`/`compose`/`cp_*` — there is no release container yet). Its `run` commands execute under the same §5.2.4 runner env and `deploy_root` cwd as recipe commands, and its `ctx.env` reads the §5.2.2 interpolation env — the configure host and the deploy host see one environment. It adjusts the initial config by **mutating `ctx.cfg` directly** (`ctx.cfg.retention.keep_releases = 5`); the mutated table is read back, re-parsed and re-validated into the typed config the engine then runs against. The **same live read-back applies to every hook mid-deploy** (§6.2), not just `configure`: a `before_`/`after_` hook may mutate `ctx.cfg` (honored for any step not yet run) or `ctx.state` (read back into deploy state, persisted once past cutover). Implementation: before firing a slot's hooks the engine `refresh`es the `cfg`/`state` tables from the typed values; after, it re-reads them and, if changed, re-parses (`cfg` re-validated; a failure aborts the deploy). The round-trip goes through Lua, so an empty map serializes as an empty table and is parsed back as an empty map (`de_lenient_map`); map ordering (`env`/`services`) is not guaranteed across a mutated round-trip but does not affect correctness. `ctx.vars` remains for scratch state that is not part of cfg/state.

### 6.3 YAML hook actions (zero-Lua path)

A `hooks.<slot>` entry is one typed action (a bare string = `run`), each mapping to the same effect as the `ctx` method of the same name: `run` · `exec_in: {service, cmd}` · `exec_in_release` · `docker: [args]` · `compose: [args]` · `cp_from_release: {from,to}` · `cp_to_release: {from,to}`. Slots: `before_<task>` / `after_<task>` for every §3 task.

### 6.4 Sandboxing

mlua with stdlib minus process/file escapes: `os.execute`, `os.exit`, `os.getenv`, `io.popen`, `io.open`, `dofile`, `loadfile`, `require` of arbitrary paths are removed/replaced. All process/file effects must go through `ctx` (so they honour `--dry-run` and the seam). Plugin load/runtime error → exit `10` with the Lua traceback.

---

## 7. Built-in recipe `docker-redblack` (exact commands)

Each task: inputs, the exact argv, the failure rule. `{…}` are resolved values.
`compose(...)` = `docker compose -p {project} --env-file /dev/null -f {compose.files…} -f dcd-image-override.yml`.
Every command is built as `Vec<String>`, then — when `ssh:` is set — wrapped by the
pure builder of §2.7 into `ssh <opts> <target> -- sh -c 'cd {deploy_root} && set -a; . /dev/stdin; exec <argv>'`
with the env document on stdin (INV-12). No shell is involved beyond that wrapper
unless an action explicitly asks for `sh -c`.

**Where the compose model is resolved: on the deploying machine.** `docker compose config --format json` is run **locally**, against the checkout, and it is the single source for service names, images, healthchecks, restart policies and network aliases. Resolving it on the target instead would make `dcd check` on a fresh target validate nothing, make it validate the *previous* deploy's files on a live one, and make `dcd deploy --dry-run` compute its whole plan from stale remote files while presenting it as the plan for the local checkout. Local resolution is the only arrangement in which `check`, `tasks` and `--dry-run` are honest.

**Profiles are enabled for resolution only.** A service carrying `profiles:` is **absent from the resolved model** unless its profile is enabled — verified 2026-08-25 against the worked example: `compose config --services` returns `centrifugo nginx postgres valkey`, and only `--profile dcd-release` adds `app worker`. Model resolution therefore runs `compose --profile {compose.profiles…} config --format json`, or §5.1 would reject the operator's own `release.service` as "not a service in the compose model". The flag is **never** passed to `up`, which would start the release service as an ordinary container; `compose run` needs no flag because it auto-enables its target's profile (verified). This is also why the profile is a configurable name rather than a hardcoded string.

**Consequence — the deploying machine needs `docker` with the compose plugin**, in addition to `ssh`. A CI runner that builds and pushes the images already has it. Only *resolution* is local; every `up`/`run`/`pull`/`inspect` executes on the target. The two can disagree about one thing only — relative bind-mount sources, which compose resolves against the client's project directory — which is exactly what the `check` warning above covers.

**Step order (v2 adds `sync` at the head):**
`sync · preflight · ensure_upstream · pull · infra · migrate:before · start:black · healthcheck · cutover · drain:red · migrate:after · workers · finalize`

### 7.0 `sync` (new in v2)
- **Bullet 1 always runs, local or remote.** The remaining bullets are skipped when `ssh:` is absent — the files are already where they need to be.
- Write `{deploy_root}/dcd-image-override.yml` (§5.5) *(Mutate)*. **Always**, in both modes: the `compose(...)` helper passes `-f dcd-image-override.yml` unconditionally, so skipping it locally would hand compose a `-f` pointing at a missing file and break every local run — and `--image` and rollback image-pinning with it.
- Place every `compose.files` entry under `{deploy_root}` *(Mutate)*, each staged and renamed per §2.3. Each entry is READ from `compose.sources` (the same path resolved against the **config file's** directory) and ADDRESSED by its `deploy_root`-relative name, which is what `-f` receives — the two differ whenever `-c` points elsewhere, and locally whenever `deploy_root` is not dcd's cwd. Local runs place them too: every command runs with `deploy_root` as its cwd, so skipping the copy handed compose a `-f` resolving nowhere.
- `release.run.env_file` is **not** uploaded — §5.3 makes it the operator-managed at-rest file, which must already exist on the target. (This line previously said it was; the code never did, and §5.3 is the intended rule.)
- **Not uploaded: `plugins/*.lua`.** The Lua host runs in dcd's own process on the deploying machine; nothing on the target ever reads a `.lua` file (§2.1).
- **dcd uploads the compose *documents* and nothing they reference.** Relative bind-mount sources (`./router/router.conf`), `env_file:` entries, `build.context`, and `include:`/`extends` targets are **not** uploaded. This is the mirror of the failure ADR-001 used to reject `DOCKER_HOST=ssh://`, and it is stated here rather than discovered in production: Docker silently creates an empty directory for a missing bind source, so a router whose config never arrived starts cleanly and serves nothing. `check` scans the resolved model for relative `volumes:` sources, `env_file:` and `build:` and **warns**, naming each path, that it must already exist on the target or be placed there by the operator (the `directories:` block creates them empty; it does not fill them).
- Uploads are unconditional — no hash-compare. The payloads are compose documents measured in kilobytes; a `sha256sum` round trip per file costs two extra commands and adds a dependency (`sha256sum` is absent on some busybox/BSD targets) to guard nothing.
- **Runs after the host guard and after the lock, and is `Mutate`**, so `--dry-run` prints the uploads and writes nothing.

### 7.1 `preflight`
- Networks come from the resolved compose model; dcd does **not** create them — `compose up` does. `preflight` only runs `docker network inspect` *(Read)* on each, to fail early and clearly rather than mid-deploy. The v1 derived default `<project>_default` is gone: under ADR-013 the compose file names the network (the §15 fixture declares `blogapp_net` while `project: blogapp`), so deriving one would inspect a network nothing uses.
- For each `directories[]`: `mkdir -p`; if `owner` → `docker run --rm -v {deploy_root}:/wd busybox chown {owner} /wd/{path}` (unprivileged-safe chown).
- **One-time v1 worker reap.** v1 generated one compose *service per worker* (`worker-async`, `worker-sched`), so those containers carry `com.docker.compose.service=worker-async` — which v2's discovery filter (`service={workers.service}`) never matches. Left alone they are never drained or removed, and §7.12's `compose run --name worker-async` then collides post-cutover on the first v2 deploy of **every existing installation**. So: when the stage has no v2 release recorded, `docker ps -a --filter label=com.docker.compose.project={project} --filter name=^{escaped workers.name_prefix}` *(Read)* → `docker rm -f` each container whose `com.docker.compose.service` label is not `{workers.service}` *(Mutate)*. Documented in UPGRADE.md's v2 entry.
- **Orphan reaping:** `docker ps -a --filter name=^{escaped release.container_prefix}- --format '{{.Names}}'` *(Read)* — the filter is an **unanchored regex**, so the `^` and regex-escaping are mandatory; without them a decoy like `my-acme-app-sidecar` is swept by `docker rm -f`; for each not present in `state.releases` → `docker rm -f {name}` *(Mutate)*.
- Failure → abort, red untouched.

### 7.2 `ensure_upstream` (INV-9)
- If `state.current` set and `docker ps -q --filter name={current}` *(Read)* is empty (dead), **or** the upstream file is missing → write `render(template, backend=fallback_backend)`.
- Runs before `infra` so a recreated router never points at a corpse.

### 7.3 `pull`
- Resolve every image from the compose model: `compose config --format json` *(Read)* — with the override file last, so `release.service`'s image is the exact ref dcd pinned.
- Record every tag about to be pulled in `stages[].pulled[]` (`{service, tag, pulled_at}`) and persist **before** the first pull (INV-11). Purely additive.
- `compose pull` *(Mutate)*, or per-service pulls where only some services are managed.
- Failure → abort; the ledger already carries whatever landed, so §7.13 can reclaim it later.

### 7.4 `infra` (ordered)
- Service identity comes from the resolved compose model, **not from `dcd.yaml`**: container name, image and readiness are read back from `compose config --format json` and, after start, `docker compose ps --format json` *(Read)* — which is authoritative for the *side* services. (It is **not** used for the release container: compose hides one-off containers from `ps` — INV-13.)
- For each managed service: desired = resolved image; current = `docker inspect {container} --format '{{.Config.Image}}'` *(Read; missing ⇒ `none`)*.
  - `recreate: never` → `compose up -d --no-recreate {service}`.
  - `recreate: always` → `compose up -d {service}`.
  - `recreate: on-image-change` → if current≠desired: if `on_recreate_drain_workers` and not yet `drained` → **worker drain** (§7.9), set `drained`; then `compose up -d {service}`; else `compose up -d --no-recreate {service}`.
- Readiness: `compose up -d --wait --wait-timeout {getWaitTimeoutSeconds()=120} {services…}` uses the services' own compose `healthcheck:` blocks as the gate. A `services.<name>.wait` override falls back to the v1 poll (`docker exec {container} sh -c '{cmd}'` *(Read)* each `interval` up to `retries`).
- **Ordering contract (tested):** worker drain (if any) precedes the first recreate; readiness gates run after all recreates.

### 7.5 `migrate:before`
- Skip if unset. `compose run --rm -T --no-deps --entrypoint {command[0]} {release.service} {command[1..]}` *(Mutate)* — argv passed directly, no `sh -c`. `-T` is mandatory (§5.2 delivery note).
- **`--entrypoint` is mandatory.** `compose run` appends the trailing words as *arguments to the service's entrypoint* (§7.12 relies on exactly that for workers). Without the override, `php bin/console app:db:migrate before` would be handed to a service whose entrypoint is `rr serve`. v1 controlled this through `release.run.entrypoint`; ADR-013 removed that knob, so the migrate command's first token becomes the entrypoint and the rest its arguments.
- The throwaway inherits the release service's image, env, volumes and network from compose; chain keys ride bare `-e KEY` flags exactly as in §7.6.
- Failure → abort (red untouched). Migrations are not atomic — expand-contract discipline must keep a partially-applied `before` migration red-compatible (§9, §11).

### 7.6 `start:black`
- `container = {container_prefix}-{release_id}` (stored id, §2.6); fail if it already exists.
- `compose run -d --name {container} --use-aliases --no-deps {-e KEY…} {release.service}` *(Mutate)*, then `docker update --restart={restart} {container}` *(Mutate)*.
  - `{restart}` is read from the resolved compose model: `services.<s>.restart` verbatim; else `deploy.restart_policy` mapped (`any`→`always`, `on-failure` + `max_attempts: N`→`on-failure:N`, `none`→`no`); if the service declares neither, `docker update` is skipped and `check` warns that the release will not survive a reboot.
  - `--use-aliases` applies the service's declared network aliases — the v1 `network_alias` feature, for free.
  - `--no-deps` is safe because `infra` has already brought the side services up.
  - `-e KEY` flags are bare sorted key names; compose reads each value from its own process env (§5.2.4), so no value enters argv. **Verified 2026-08-25**: `compose run -e KEY` with no value delivers the process-env value to the container with no argv leak.
  - **`docker update` is mandatory, not cosmetic.** Compose forces `restart=no` on one-off containers (verified: `.HostConfig.RestartPolicy.Name` is `no` straight after `compose run -d`). Without the update, reboot-restart silently breaks — a real capability regression.
  - **Known non-atomicity:** between the two commands the black has `restart=no`. A host reboot in that sub-second window leaves it down after a deploy that reported success; `ensure_upstream`'s INV-9 self-heal covers the traffic side on the next run.
- When `release.run` is used instead of `release.service`, dcd renders it into a one-service compose document and runs the **same** `compose run` against it — one creation primitive, one test surface (§5.1 validation forbids declaring both).

### 7.7 `healthcheck`
- **Default — the compose service's own `healthcheck:`.** `docker compose run` has **no** `--health-cmd` flag (verified on compose 5.3.1: `docker compose run --help` offers `--name`, `--no-deps`, `--use-aliases` and nothing health-related), and §5.5 forbids the override file from carrying anything but image refs — so dcd cannot inject a probe. The probe is therefore the operator's, declared on the service in compose, and dcd polls `docker inspect {container} --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}'` *(Read)* until `healthy`, up to `getHealthRetries()` = **60** attempts, `getHealthIntervalSeconds()` = **2** apart (the same defaults the escape-hatch form documents). The probe runs **inside** the new container, so the v1 alias footgun cannot occur; it needs a probe binary in the image.
- **Required fallback (`release.healthcheck`):** the v1 form — substitute the black **container name** into `cmd` (`{container}`), run `docker exec {resolved container of exec_in} sh -c '<cmd>'` *(Mutate)*. Required whenever the probe must originate from the router, or the app image has no shell tools.
- **Never substitute the shared network alias** — red and black both answer to it, so it resolves to red and passes falsely (§9). Validation enforces `{container}` in the escape-hatch `cmd`.
- `check` **errors** when the release service declares no compose `healthcheck:` *and* no `release.healthcheck` is configured — an ungated cutover is the one failure red-black exists to prevent (§5.1).
- Exhaustion → remove black, keep red (INV-1).

### 7.8 `cutover` (INV boundary)
- `prev = read(upstream_file)` (capture for restore).
- `backend = {container}:{cutover.backend_port}`; write `render(template, backend)`.
- Optional `cutover.validate` → `docker exec … nginx -t` *(Read)*; on failure: restore `prev`, abort (red serving).
- Reload: `docker exec {resolved container of reload.exec_in} sh -c '{reload.cmd}'`.
  - reload ≠ 0 → restore `prev`, remove black, abort, exit `1`.
  - reload = 0 → **point of no automatic return.** In one atomic state write: append `Release{ id, container, images: {service → resolved ref}, created_at, status: cutover_pending, ran_migrations }` **and demote any pre-existing `cutover_pending` → `rolled_back`** (INV-10), then persist (INV-3).

### 7.9 worker drain (shared helper)
- `names = docker ps --filter label=com.docker.compose.service={workers.service} --filter label=com.docker.compose.project={project} --format '{{.Names}}'` *(Read)*.
- **The label filter is load-bearing (INV-14).** v1 matched on `com.docker.compose.project` plus an **unanchored `name=` substring** (`docker.rs` `worker_ps_names`), which was safe only because the black was a plain `docker run` (`docker.rs` `run_black`) carrying no compose labels. **ADR-013 introduces this hazard** — it does not exist today. Under ADR-013 the release container will carry the project label, so that substring filter would match it — and the next line issues `docker stop` on every match, post-cutover, against the container serving traffic. Filtering on the worker **service** cannot match the release service, and §5.1 makes `workers.service == release.service` a config error so the two can never coincide. `workers.name_prefix` is container naming only, never discovery.
- For each: `docker exec {name} sh -c '{workers.drain}'` *(Mutate, best-effort)*.
- `docker stop --signal {workers.stop_signal} --timeout {workers.stop_timeout} {names…}` then `docker rm -f {names…}` *(Mutate)*.
- **Removal, not just stopping, is the v2 contract.** Workers are recreated by `compose run --name`, which fails against a *stopped* container still holding the name. This makes "every deploy recreates every worker" explicit; it loses compose's "unchanged worker keeps running" optimisation, which is acceptable because workers are drained and stopped on every deploy anyway.

### 7.10 `drain:red`
- Reap every running app container except the just-cut-over black: `docker ps --filter name=^{escaped release.container_prefix}- --format '{{.Names}}'` *(Read; anchored + escaped, as in §7.1)* minus the black; for each → `docker exec {c} sh -c '{release.drain}'` *(best-effort)*, then `docker rm -f {c}`. Resume-safe.
- Then worker drain (§7.9) unless `drained` was already set in `infra`.

### 7.11 `migrate:after`
- Skip if unset. `docker exec {black} {command…}` (argv direct; shares the running black's env).
- Failure → post-cutover (INV-2): stop, report, exit `4`. Must be idempotent — `--resume` may re-run it.

### 7.12 `workers`
- Names: `provider.static`, or run `provider.command_in_release` in the black *(Mutate; in `--dry-run` there is no black, so it emits `⚠ worker set is dynamic; not resolvable in dry-run`)*, split stdout into non-empty lines, filter names containing `.` and any in `workers.exclude`.
- For each name: `compose run -d --name {name_prefix}{name} --no-deps {-e KEY…} {workers.service} {name} {workers.args…}` *(Mutate)*, then `docker update --restart={restart}`. Per-worker arguments are appended after the service name (compose's `[COMMAND] [ARGS…]`), so the v1 `{name}` template token becomes a plain argument list — **verified**: entrypoint is preserved and the service's `command` is overridden.
- **`workers.compose_file` and its renderer are deleted.** N containers now come from ONE compose service definition, so dcd no longer generates, uploads, or `-f`s a workers file.
- `stop_signal` and `stop_timeout` stay dcd keys (there is no `stop_grace_period` in v2 — `stop_timeout` is the one name for that concept): compose's equivalents are *stop-time options*, not container config, and are **not** baked into a `compose run` container (verified: `.Config.StopSignal` and `.Config.StopTimeout` are empty). dcd applies them at `docker stop`.
- **Cost, stated:** this is N invocations where v1 issued one batched `compose up -d w1 w2 w3`, each carrying its own env document on stdin. For 10 workers that is roughly +20 round trips on a ~70-command deploy — at least one RTT each on top of the ~28 ms local cost (§2.7), and the reason `sync`+multiplexing are mandatory rather than optional.

### 7.13 `finalize`
- Apply the §4.3 finalize transition — flip the `cutover_pending` release → `active`, reconcile prior statuses, set `current = black` — then write `dcd-state.json` (mode `0600`, atomically, §2.3).
- **Retention (state-based):** retain `current`, any `cutover_pending`, and the newest `keep_releases` releases with status in {`superseded`, `rolled_back`}; evict older ones — for each, `docker rm -f` any leftover container and `docker image rm {release.images[release.service]}` **iff** no retained release references it. Never `docker image prune -a`.
- **Eviction is idempotent and does not delete history.** An evicted release keeps its row and gains `reaped: true` once teardown is confirmed; `evictions()` skips reaped rows.
- **Managed-image GC:** retain the newest `keep_managed_images` distinct tags per **service** (`retention.keep_images.<service>` overrides; `release.service` is rejected there — `keep_releases` bounds it); `docker image rm` older unreferenced ones.
- **The pull ledger (INV-11)** makes that census complete; tag order comes from the ledger's own `pulled_at`, never from `docker images --format {{.CreatedAt}}`.
- **Cross-stage protection:** GC subtracts every tag any *other* stage in the same `dcd-state.json` records before removing anything.
- Removal stays best-effort: `docker image rm` (never `-f`, never by ID) refusing a still-referenced tag is the correct outcome.

---

## 8. CLI surface

```
dcd <command> [stage] [flags]
```

| Command | Behaviour |
|---------|-----------|
| `deploy [stage]` | run the recipe; `stage` optional if exactly one exists (or the config has no `stages:` block — the stages-less case §5.2.1 covers). `--resume` recovers a `cutover_pending` state (§4.2); a non-resume deploy that finds one refuses (exit 4). Stage selection is the **positional only** (there is no `--stage` flag) |
| `rollback [stage]` | §4.1; requires `--yes` when non-interactive |
| `unlock [stage]` | §4.4 — accept the incomplete release as deployed and clear the stage lock; requires `--yes` when non-interactive |
| `status [stage]` | current + history from state: each release's status, images, age, `ran_migrations`, and any `cutover_pending` recovery hint |
| `tasks [stage]` | print the resolved, ordered task plan (graph + hooks); no side effects |
| `gc [stage] [--all]` | run §7.13 retention standalone, so a full host does not have to deploy to recover. Takes the stage lock. Default scope is what dcd recorded (release history + pull ledger) — the same authority `finalize` already has, so it just reports and removes. `--all` additionally lists tags Docker reports in the repositories this config resolves to that **no** stage records, prints the rule protecting every survivor, and asks before removing (`-y` to skip). Ownership is proved, never assumed: with `registry:` set that prefix is the operator's own declaration; without it, a repository must name a registry host (a dot, a port, or `localhost` in its first segment) — `bitnami/postgresql` is as much a Docker Hub name as `postgres`. Only services dcd itself pulls are considered. Unprovable repositories are skipped with a warning; the command refuses outright only when none is provable. Pair either form with `--dry-run` to change nothing |
| `check [stage]` | validate config + stage merge + dotenv chain + interpolation + `--set` + plugin load + compose-model resolution; prints the §5.2.5 env observability report and the §5.1/§7.0 shape warnings (un-uploaded bind sources, a release publishing `ports:`, a release outside `compose.profiles`, a router not mounting the upstream file); no side effects, and **no ssh connection** — the stray-`.env` probe is skipped when `ssh:` is set rather than paying a `ConnectTimeout` in a CI lint job |
| `init` | scaffold config — see §8.4 |
| `schema` | print a JSON Schema for `dcd.yaml`, derived from the typed `Config` (§8.4) |

| Global flag | Meaning |
|-------------|---------|
| `-c, --config <path>` | config file (default `./dcd.yaml`) |
| `--resume` | (deploy) recover a post-cutover-incomplete release |
| `--dry-run` | read-pass-through plan; mutations stubbed (§2.3/§2.4) |
| `--json` | newline-delimited JSON events |
| `--image <service>=<ref>` | pin a compose service's image in the generated override file (§5.5) (repeatable) |
| `--ssh <target>` | override `ssh:` for this run (CI) |

**Per-command transport and guard behaviour** (previously unstated):

| Command | Opens a connection | Host guard (INV-8) | Runs `sync` |
|---------|--------------------|--------------------|-------------|
| `deploy`, `rollback` | yes | yes | yes |
| `unlock` | yes | yes (§4.4 step 1) | no |
| `gc` | yes | **yes** — it removes images | no |
| `status` | yes (state read) | no; prints the target name | no |
| `check` | no — the compose model resolves locally (§7) | no | no |
| `tasks` | no | no | no |
| `init`, `schema` | no | no | no |
| `--set <path>=<value>` | override an existing config scalar (repeatable; §5.1 semantics) |
| `--env-dir <path>` | directory of the dotenv chain (default: the config file's directory) (§5.2.1) |
| `--env-stdin` | read one dotenv-format document from stdin; alone it is the whole chain (no implicit `.env` discovery), with `--env-dir`/`--env-file` the highest file layer; interactive prompts then error without `-y/--yes` (§5.2) |
| `-v/--verbose` | trace every spawned command: argv, exit code, elapsed, captured stdout/stderr (§8.2) |
| `-V/--version` | binary version |
| `-y/--yes` | assume yes (rollback / refuse prompts) |
| `--reason <text>` | annotate this deploy/rollback in state |

### 8.1 Exit codes

| Code | Meaning | System state |
|------|---------|--------------|
| 0 | success | black is the new red |
| 1 | pre-cutover failure | **red still serving**; black removed |
| 2 | config / usage error | nothing ran |
| 3 | stage lock held | another deploy in progress; nothing ran (`unlock` overrides it, §4.4) |
| 4 | post-cutover incomplete (`drain:red`/`migrate:after`/`workers`), or a `deploy` found a `cutover_pending` state | **black is live**; recover with `--resume`, `rollback`, or `unlock` (§4.4) |
| 5 | host guard mismatch (INV-8) | nothing ran |
| 6 | ssh transport failure (auth, host key, DNS, connection lost) | pre-cutover: nothing changed or red still serving; if it happened post-cutover the run reports `4` instead |
| 10 | Lua plugin error | reported with traceback |
| 130 | interrupted (SIGINT/SIGTERM) pre-cutover | cleaned up; red serving |

### 8.2 Output modes (ADR-009)

One `Event` stream → reporter renders by environment: **rich** (TTY: per-task status + elapsed + summary), **plain** (no TTY: `[HH:MM:SS] <task>: <status>` — matches today's `log()`), **`--json`** (`{ts,stage,task,status,ms,detail}` per line). All failures print the failing argv + captured stderr.

Under `ssh:`, the reporter prints one `via ssh <target> (cwd <deploy_root>)` header at run start and then the **plain** argv per command — not the ssh-wrapped form. The wrapped form is what actually executes, but 70 lines each prefixed with `ssh -o ControlPath=…` is worse operator output, and the plan line is emitted by the engine (`run_argv`), above the runner where wrapping happens. `-v` traces the real, wrapped argv.

`-v/--verbose` adds a second layer under those task lines: every command the engine spawns is traced at its single choke point (`Engine::run_argv`, plus the `configure` hook's own runner) as `$ <argv>` / `  exit <code> in <ms>ms` with stdout prefixed `  | ` and stderr `  ! `, or `{"exec":…}` + `{"exec_result":…}`/`{"exec_error":…}` under `--json`. Commands stubbed by `--dry-run` are not traced — they already print as `plan` lines, and nothing ran. **argv can never carry a value; captured stdout can.** Chain env is delivered as a bare `-e KEY` (§5.2.4), so no value is ever part of an argv — but `docker compose config` **resolves and inlines env values** into its output (§5.5), and §5 passes the whole resolved chain to compose. Verified 2026-08-25: with `DB_PASSWORD` set, `docker compose --env-file /dev/null config --format json` prints `"DB_PASSWORD": "hunter2-SECRET"`. dcd therefore parses that output internally and **never traces its stdout**: the `-v` line for that one command reads `<compose model, N services — output suppressed (contains resolved env values)>`. The same suppression applies to `dcd check`.

### 8.3 CI integration

v1 required the pipeline to stage dcd and its config on the server first. v2 absorbs that wrapper:

```
# v1 — the pipeline did the copying and the remoting
scp dcd dcd.yaml .env plugins/ → $DEPLOY_ROOT
ssh server "cd $DEPLOY_ROOT && ./dcd deploy prod --env-stdin --yes --image app=$TAG …"

# v2 — one command, from the repo checkout
dcd deploy prod --yes --image app=$TAG
```

- `ssh:` (or `--ssh`) names the target; `deploy_root` says where on it.
- compose files ride along from the checkout via `sync` (§7.0). Nothing is pre-staged.
- The dotenv chain is read **next to the config in the checkout**; `--env-stdin` still works and is still the disk-free path for secrets.
- The runner needs `ssh`, a key the target accepts, and `docker` with the compose plugin (for local model resolution only — see §7's preamble); the target needs `docker` with the compose plugin, `sshd`, and a POSIX shell. **No dcd binary, config, plugin or env file is installed on the target** — only the files `sync` uploads and the state/upstream/override files dcd owns.

### 8.4 `dcd init`

`dcd init` writes a runnable `./dcd.yaml` skeleton; `--with-plugin` also writes `plugins/app.lua`. Refuses if `dcd.yaml` exists unless `--force`. The skeleton is asserted by loading it — `scaffold_parses_and_validates` parses `SCAFFOLD` through the real loader, which is what matters; there is no snapshot test.

**`dcd init --from-compose <file>`** reads an existing compose file as YAML — never through `docker compose config`, so an unresolved `${APP_TAG}` and a missing daemon do not stop it — and emits a filled-in config: services detected, `services:` policy rows stubbed, the release service guessed (the one behind a `dcd-release` profile, else a conventional name, else the single non-router service publishing ports), `cutover.service` guessed from a router image **or a router service name** (`${REGISTRY}:${ROUTER_TAG}` names nothing), `cutover.backend_port` read from the release's `ports:`, and `TODO:` markers left **only** on the genuinely red-black-specific fields (`ssh`, `deploy_root`, `cutover.reload`, and either service that could not be guessed). Every emitted document is valid YAML — a `TODO:` in value position is quoted, or the file will not parse at all. `validate_no_placeholders_left` then makes `dcd check` name each unfilled field. This is the highest-leverage answer to "dcd is hard to set up": the schema shrinking (ADR-013) removes what must be written, and `--from-compose` removes writing most of the rest by hand.

**`dcd schema`** prints a JSON Schema **derived** from the typed `Config` (`schemars`), for editor completion and inline validation. Derived, not written: a hand-authored schema would be a fifth representation to keep in sync.

It describes the **authored document**, which is not the shape of `Config`: `stages:` is stripped before deserialization (§5.1), so the raw derived schema names every key except the one key every config must have, and `deny_unknown_fields` then rejects it. `config::authoring_schema` re-adds `stages` (the sole `required` key, with each stage a same-shaped override) and drops `required` elsewhere — a config whose `release:` lives only under `stages.prod` is valid, so requiring it at the root would be wrong.

### 8.5 Code quality requirements (pre-empts comment churn)

Generated Rust MUST be dumb-simple and readable: intention-revealing names, small single-purpose functions, guard clauses over nesting, exhaustive `match`, `Result` + `?` (no `unwrap`/`expect` outside tests and provably-infallible spots, each carrying a one-line WHY). `///` rustdoc on **public** items only, one line, no WHAT/HOW prose; **no inline narration**. `clippy -D warnings` + `rustfmt` are the floor. If a construct needs a comment to be understood, rewrite it simpler.

---

## 9. Anti-Patterns (DO NOT)

| Don't | Do Instead | Why |
|-------|-----------|-----|
| `docker image prune -a -f` after deploy | state-based eviction beyond `keep_releases` + managed-image GC over the pull ledger (§7.13), with `dcd gc --all` as the operator-driven sweep for tags dcd never recorded | the blanket prune deletes the rollback target — a real bug in the original script (INV-6) |
| `docker compose up -d` with no service list, or `--remove-orphans` | scope every compose `up` to explicit services; never `--remove-orphans` | a bare `up` starts a **second** app container beside the release container (verified), and orphan-removal is a foot-gun near dcd-owned containers |
| Healthcheck through a shared network alias | use `release.health` (the probe runs *inside* the black), or the escape hatch against the black **container name** (§7.7) | `--use-aliases` gives red and black the same aliases; an alias resolves to red → false-positive health, cutting over to a sick black |
| Auto-rollback on a post-cutover failure | stop, report, `exit 4`, `--resume` after fixing | black is already live; tearing it down for a failed worker causes more disruption (INV-2) |
| Advance `current` before cutover succeeds | record `cutover_pending` at reload=0; flip in `finalize` (INV-3) | otherwise an exit-4 leaves the live black unrecorded and breaks rollback math |
| Drain workers via `docker compose stop` against a workers file | discovery by `label=com.docker.compose.service` + `docker stop`/`rm -f` (§7.9) | there is no workers file in v2; and a **name-prefix** filter can now match the release container and stop it mid-traffic (INV-14) |
| Discover workers by container-name prefix | filter on `label=com.docker.compose.service={workers.service}` | under ADR-013 the release container carries the compose project label, so a prefix filter can sweep it into `docker stop` (INV-14) |
| Ask `docker compose ps` where the release container is | address it by the name dcd chose, via plain `docker` (INV-13) | compose **hides** one-off containers from `ps`; it is authoritative for side services only |
| Snapshot `docker compose config` output into state | record resolved image refs only (§5.5) | `compose config` inlines resolved **env values** — snapshotting writes secrets into a dcd-owned file, breaking §5.2.5 and IT-007 |
| `KEY=VALUE` on an ssh command line | the stdin env document (§5.2, INV-12) | it exposes every secret in the **target's** `ps`, strictly worse than running on the server |
| Rely on `compose run` to apply `restart:` | `docker update --restart=…` right after creation (§7.6) | compose forces `restart=no` on one-off containers; skipping this silently breaks reboot-restart |
| Require `profiles: ["dcd-release"]` for correctness | keep every compose call service-qualified; warn if the profile is missing (§5.1) | dcd's correctness must not depend on a magic string inside a file it does not own |
| `ssh -tt` to forward signals | run without a PTY; re-issue cleanup after the in-flight command returns (§2.5) | a PTY mangles the `--format` output dcd parses |
| Let `ControlPath` default, or build it from a long path | dcd picks `%C` under a short dir and preflights the length (§2.7) | over ~108 bytes ssh **fails outright** rather than degrading to an unmultiplexed connection |
| Treat ssh exit 255 as an ordinary command failure | distinguish transport failure and abort (§2.7) | otherwise a severed connection is reported as an application failure and best-effort sites swallow it |
| Trace or print `docker compose config` stdout | parse it internally; print `<compose model, N services — output suppressed>` (§8.2) | it inlines every resolved env value, so `-v` or `check` would dump the entire secret set — the leak §5.2.5 forbids |
| Parse human-readable `docker` output | always `--format` with a Go template | the target's locale is not dcd's to control |
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

Unit tests use the effects seam (no Docker). Integration tests (`IT-*`) run against real Docker and are `#[ignore]`d, so a plain `cargo test` reports them **ignored**; `-- --include-ignored` opts in, and CI does so explicitly. The earlier `DCD_E2E=1` env gate is gone: a gate that returns early reports a green tick while asserting nothing, which is how a whole suite stayed broken through the v2 migration unnoticed.

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
| TC-012 (v2) | `start:black` argv | release config | exact `docker run -d …`; uses stored id | name collision → error |
| TC-013 | healthcheck | fail N then ok / always fail; cmd uses `{container}` | pass on N+1 / abort (red kept, black removed); argv has black **name** | retries=1 |
| TC-014 | cutover reload fail | reload≠0 | upstream **restored** to prev, black removed, exit 1, state unchanged | validate `nginx -t` fail path |
| TC-015 | cutover success | reload=0 | `cutover_pending` appended to state **before** drain:red | — |
| TC-016 | workers gen | dynamic stdout `async\nsched` / static | 2 `worker-*` services + footer; scoped `up -d worker-async worker-sched` | empty → none; `.`-names filtered |
| TC-017 | finalize retention | 1 current + 5 superseded, `keep_releases` 3 | current + newest 3 superseded kept (4 total); oldest 2 evicted (rm container+image) | keep 1 keeps the rollback target; a `rolled_back` release is retained like `superseded` |
| TC-018 | managed-image GC | 3 db tags across `releases[].images`, `keep_managed_images` 2 | oldest unreferenced db image removed | referenced by a retained release → kept |
| TC-018b | pull ledger | a tag pulled by a deploy that never finalized, then two later deploys | reclaimed once past `keep_managed_images`, ledger row dropped | within the keep count → kept |
| TC-018c | cross-stage protection | tag recorded by stage beta, past prod's keep count | prod GC keeps it (INV-6) | recorded nowhere else → removed |
| TC-018d | `gc --all` | host tags: recorded, container-referenced, orphan, `<none>` | only the orphan proposed; each survivor reported with its rule | repository without `/` → refuses to sweep |
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
| TC-041 | `--env-stdin` alone | a `.env`/`.env.local` sitting next to `dcd.yaml` | neither is discovered; `<stdin>` is the only layer and nothing is probed on disk | `--env-dir`/`--env-file` re-enables file layers under the stdin layer |
| TC-042 | `unlock` promote | state w/ `cutover_pending` P and stale `current` C | P → `active`/`current`, C → `superseded`, one state write; **zero commands spawned** (no drain, retention, migration, worker, or hook); leftovers warned by name | no pending → state untouched, lock still cleared |
| TC-043 | `unlock` on broken state | two pendings, no prior `current`, `--reason` given | newest pending promoted, older → `rolled_back`, reason recorded, no "left running" warning | `--dry-run` → plan lines only, nothing written or removed |
| TC-044 | `unlock` lock override | flock held by a live process; stale `.meta` present | `is_held` true → loud warning naming the holder; both lock files removed anyway; second run removes nothing and succeeds | released flock + stale meta → no warning, meta still cleared |

**SSH unit tests (no network, `src/ssh.rs` is pure):**

| Test ID | Component | Input | Expected output | Edge cases |
|---------|-----------|-------|-----------------|------------|
| TC-101 | ssh wrapper | `Argv["docker","ps"]`, target `deploy@h`, root `/srv/a` | exact `ssh` argv incl. the option baseline and `cd '/srv/a' && … exec 'docker' 'ps'` | no `deploy_root`; target with no user |
| TC-102 | POSIX quoting | args containing `$`, backtick, newline, `'`, `!`, `\`, a lone `-`, an empty string, whitespace-only | each round-trips to the identical byte string when parsed by `sh` | UTF-8 multibyte; 4 KB argument |
| TC-103 | env document | delivered keys with values containing `'`, newline, `=`, leading/trailing space | `KEY='…'` lines that a shell sources to the exact original values | empty value; value that is only a quote |
| TC-104 | env document | any key/value | the rendered **argv** contains no value byte (the INV-12 unit-level proof) | — |
| TC-105 | ControlPath | a `deploy_root`/target combination whose socket path exceeds 108 bytes | named error before any connection attempt | exactly 107/108/109 bytes |
| TC-106 | exit classification | 255 with empty remote output vs 255 from a remote command | transport error vs ordinary non-zero | 255 with stderr text |
| TC-107 | local fallback | config with no `ssh:` | argv is unwrapped and identical to v1's | — |

**Recipe unit tests replaced by v2 (the v1 rows they supersede are listed so they are not left behind):**

| Test ID | Component | Input | Expected output | Supersedes |
|---------|-----------|-------|-----------------|------------|
| TC-116 | `start:black` argv | a release service | `compose run -d --name … --use-aliases --no-deps [-e KEY…] <svc>` then `docker update --restart=…` | TC-012 (`docker run -d …`) |
| TC-117 | `workers` argv | 2 discovered names | 2 × `compose run -d --name {prefix}{name} … <svc> {name} {args…}`, each followed by `docker update`; **no** workers compose file is rendered | TC-016, TC-036 |
| TC-118 | worker discovery | a release container carrying the project label **and** the release service label | the filter `label=com.docker.compose.service={workers.service}` does **not** return it; `workers.service == release.service` is a config error (INV-14) | new |
| TC-119 | `migrate:before` argv | a service declaring `entrypoint` | `--entrypoint {command[0]}` present, remaining tokens as args | new |
| TC-120 | restart mapping | `restart:` / `deploy.restart_policy` / neither | verbatim / mapped / skipped + warning | new |
| TC-121 | name filters | a decoy container `my-{prefix}-sidecar` | anchored `^` filter does not return it | new |
| TC-122 | trace suppression | `compose config` in a `-v` run | stdout replaced by `<compose model, N services — output suppressed>`; no value byte in the trace | new |

**Config unit tests (v2 schema):**

| Test ID | Component | Input | Expected output | Edge cases |
|---------|-----------|-------|-----------------|------------|
| TC-110 | v1-key rejection | each of `docker.images`, `docker.services`, `release.image`, `workers.template`, `workers.compose_file` | targeted error naming the v2 replacement, never "unknown field" | all five present at once |
| TC-111 | service resolution | `release.service` absent from the compose model | error listing the known services | compose model empty |
| TC-112 | exclusivity | both `release.service` and `release.run` | error | neither → error |
| TC-113 | derived identity | compose model with `container_name` set / unset | container name taken from compose / from `compose ps` | duplicate `container_name` |
| TC-114 | retention keys | `retention.keep_images` naming `release.service` | rejected | naming an unknown service |
| TC-115 | warnings | release service without the profile, with `ports:`, with a relative bind source, with no router mount, with no healthcheck on a managed service | each warning emitted once, naming the offending path/service; `check` still exits 0 | none present |
| TC-116b | health gate | release service with no compose `healthcheck:` and no `release.healthcheck` | **error**, not a warning | one of the two present |
| TC-117b | worker/release collision | `workers.service == release.service` | error citing INV-14 | differing services |

### 10.2 Integration tests

| Test ID | Flow | Setup | Verification | Teardown |
|---------|------|-------|--------------|----------|
| IT-001 | happy deploy | fake app image serving `/health` + nginx on a scratch network | health passes, upstream swaps, old container gone, state `active` | rm containers/network/images |
| IT-002 | failed healthcheck | app never passes `/health` | aborts pre-cutover, **red serving**, black removed, state unchanged, exit 1 | as above |
| IT-003 | rollback | deploy v1, v2, `dcd rollback` | upstream back to v1, no migration run, `status` shows rollback | **PLANNED — not implemented** |
| IT-004 | resume | kill dcd between cutover and finalize (inject failure in `migrate:after`) | exit 4; `dcd deploy --resume` finishes; state `active` | partly covered by IT-008 (unlock); **the `--resume` path itself is PLANNED** |
| IT-005 | concurrent lock | two `dcd deploy` in parallel | one runs, other exit 3; killed holder's flock reclaimed | as above |
| IT-006 | conditional recreate | redeploy with unchanged managed image | postgres/nginx `--no-recreate` (same container id) | as above |
| IT-007 | env at rest + reboot survival | deploy with a 4-layer chain incl. a secret value; **chain dir outside `deploy_root`, passed via `--env-dir`** | `docker inspect` shows the value in container config; `docker stop`+`start` (env-less shell) preserves it; **no file under `deploy_root` contains the value** (recursive grep over dcd-written files) | rm containers/network; shred chain files |
| IT-009 | **ssh happy deploy** | the §OQ-6 sshd fixture; `ssh:` points at it; `deploy_root` is a scratch dir inside the fixture | identical assertions to IT-001 — health passes, upstream swaps, old container gone, state `active` — proving the transport is behaviour-neutral | fixture drop-guard + rm keypair |
| IT-010 | **no secret reaches the target's argv** (INV-12) | ssh fixture; chain carries a marker secret; an `after_healthcheck` hook writes `ps -eo args` on the target | the snapshot contains a real process list and **no marker**, while `printenv` in the release shows the value — so "no secret in ps" cannot pass by the delivery having failed | fixture drop-guard |
| IT-011 | **the remote lock releases itself** (INV-4) | ssh fixture; start a deploy, `SIGKILL` dcd mid-run | the lock is gone within the lease with **no** `unlock`; a second deploy acquires it. Variant: hold the channel but stop heartbeats → released within `getLockLeaseSeconds()` | **PLANNED — not implemented**; the mechanism was verified by hand (51 ms on kill, 2 s on heartbeat loss, §2.5) |
| IT-011b | **the lock is exclusive while heartbeating** | ssh fixture; two deploys in parallel | one runs; the other exits 3 naming host/pid/start from `.meta` | **PLANNED**; IT-005 covers the local flock |
| IT-012a | **transport loss pre-cutover** | ssh fixture; a `before_pull` hook kills sshd | exit **6** with `connection to <target> lost after pull`; red still serving | **PLANNED**; exit 6 is unit-asserted and was driven by hand against an unreachable host |
| IT-012b | **transport loss post-cutover** | ssh fixture; a `before_migrate_after` hook kills sshd | exit **4** (phase wins over transport); black live | **PLANNED** |
| IT-016 | **path locality** (the fixture's blind spot) | ssh fixture whose `deploy_root` does **not** exist on the deploying machine | every dcd-owned file (state, upstream, override) appears under it *inside* the fixture, and nothing is created locally | as above |
| IT-017 | **`--image` pins the run** | the IT-001 fixture; `deploy prod --image app=<other tag>` | the release container runs the pinned ref, the managed side container keeps its own, and the release record carries the pin so rollback replays it | fixture drop-guard |
| IT-018 | **`--image` for an unknown service refuses** | as IT-017 with a mistyped service | non-zero exit naming the services that exist; no container started | as above |
| IT-013 | **`sync` uploads nothing in `--dry-run`** | ssh fixture; empty `deploy_root` | `dcd deploy --dry-run` prints the upload plan; `deploy_root` on the target stays empty; no lock is taken (§2.4) | as above |
| IT-014 | **release container survives compose reconciliation** (INV-13/14) | deploy, then run `compose up -d --remove-orphans` and a worker drain | the release container is still running and was never `docker stop`ped by worker discovery | **PLANNED**; the discovery filter itself is argv-asserted (`docker.rs`) |
| IT-015 | **restart policy is applied** (§7.6) | deploy, then inspect the black | `.HostConfig.RestartPolicy.Name` == the configured policy, not `no` | fixture drop-guard |
| IT-008 | unlock a stuck stage | deploy, then a deploy that fails in `migrate:after` (exit 4), with the flock held by hand | `dcd unlock it -y` exits 0: black promoted to `current`/`active`, the running container set **unchanged**, both lock files gone despite the live flock; the following plain `deploy` succeeds with no `--resume` and drains the leftover | as above |

### 10.3 Invariant → test map

| INV | Test |
|-----|------|
| INV-1 | TC-013, IT-002 |
| INV-2 | TC-021 + TC-020 (no auto-rollback; exit 4 → resume) |
| INV-3 | TC-015 (record at cutover), TC-020, IT-008 (a stuck stage is recoverable from the record); IT-004 planned |
| INV-4 | TC-023, IT-005 (local flock); IT-011/IT-011b (remote lease) planned — the lease is unit-asserted and was verified by hand |
| INV-5 | TC-019 |
| INV-6 | TC-017, TC-019 |
| INV-8 | TC-022 (local), IT-009 (remote `hostname` guard) |
| INV-9 | TC-011 |
| INV-10 | TC-030 (demote-in-the-same-write) |
| INV-11 | TC-018b (record-before-pull), TC-018c (ledger closure) |
| INV-12 | TC-104 (unit: the value is in the script and absent from the argv the runner spawns — asserted against `SshRunner::spawn_argv`, not against an object the env map was never given), `chain_env_reaches_containers_as_bare_keys_with_overlays` (engine: bare `-e KEY`, no value in state), IT-010 (real: no marker in the target's `ps`, with the delivery asserted alongside so it cannot pass vacuously) |
| INV-13 | `compose ps` is grep-absent from the tree (creation-only use of compose); IT-014 planned |
| INV-14 | TC-118 (unit: label filter + config error); IT-014 (real) planned |

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
| ssh auth / host-key / DNS failure | ssh exits 255 before any remote output, at master open | abort with the target name; never retried silently | nothing changed | pre-cutover / 6 |
| ssh connection lost mid-deploy | 255 with no remote output on a command that had been working | abort immediately: `connection to <target> lost after <step>; the target's state may lag — run dcd status <stage>` | depends on the step: pre-cutover leaves red serving; post-cutover leaves black live | 6 (pre) / 4 (post) |
| `ControlPath` too long | preflight length check, or ssh's own `ControlPath too long` | abort naming the computed path | nothing changed | pre-cutover / 6 |
| upload fails mid-`sync` | non-zero from the atomic write | abort; the `.tmp` file is the only residue, the live file is untouched | nothing changed | pre-cutover / 1 |
| another deploy holds the stage | remote `flock -n` exits non-zero | abort, reporting host/pid/start from `.meta` | nothing changed | / 3 |
| `deploy_root` missing when the lock is taken | `flock` cannot create the lock file (ENOENT) | `deploy_root <path> does not exist on <target>` — **never** exit 3 | nothing changed | pre-cutover / 1 |
| holder dies / link severed | none needed — the lease expires on the target | the lock releases itself (≤ `getLockLeaseSeconds()`); the next deploy simply acquires it. **No stale-lock state exists** (INV-4) | nothing changed | — |
| `flock(1)` missing on the target | `command -v flock` in preflight | `flock is required on <target> for the stage lock; install util-linux` | nothing changed | pre-cutover / 1 |
| `compose run` name collision with a stopped worker | non-zero naming the container | abort; §7.9's `rm -f` should have prevented it | workers degraded | post-cutover / 4 |
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
| lock held | `another deploy holds prod (pid 4123 since 16:40)`; dead pid → reclaimed | 3 | wait/retry; `dcd unlock prod` if the holder is hung |
| host guard mismatch | `stage prod expects host prod.example.internal, the ssh target reports beta-box` | 5 | fix `ssh:`/`host:`, or the ssh_config alias |
| v1-only config key | `docker.services was removed — side-container identity now comes from your compose file; declare policy under services: (see AGENTS.md §2)` | 2 | rewrite the block |
| both `release.service` and `release.run` | `release: declare service: (compose-owned) or run: (dcd-owned), not both` | 2 | drop one |
| `release.service` names no compose service | `release.service 'app' is not a service in docker-compose.prod.yml (known: web, db, worker)` | 2 | fix the name |
| `--image` names no compose service | `--image worker=… : 'worker' is not a service in the resolved compose model` | 2 | fix the name |
| deploy finds `cutover_pending` | `prod has an incomplete release <c>; run 'dcd deploy --resume prod', 'dcd rollback prod', or 'dcd unlock prod' to accept it as-is` | 4 | resume/rollback/unlock |
| `unlock` declined at the prompt | `unlock declined (pass --yes to confirm)` — nothing promoted, lock untouched | 1 | re-run with `-y` |
| rollback no previous / images gone | `no previous release for prod` / `target image <tag> not present and not pullable` | 1 | — |
| Lua error | plugin path + traceback | 10 | fix plugin |

---

## 12. Architecture Decision Records

| ADR | Decision | Rationale | Alternatives rejected |
|-----|----------|-----------|-----------------------|
| 001 (v2, 2026-08-25) | Run **on the deploying machine**; reach the target over SSH | removes the copy-and-run-there setup burden; dcd keeps the decisions, state machine and failure handling where the operator is, php-deployer-style | **v1's on-server model** (superseded); **DOCKER_HOST=ssh://** — native (verified) but gives dcd no shell for the file operations it owns, and compose rewrites relative bind sources to *client* paths (verified), so it silently breaks compose files that work today; **pushing the dcd binary and running the engine remotely** — preserves INV-3/4/8 and env delivery for free at 2 round trips instead of ~70, and was seriously considered, but the deciding binary would run on the remote again, inverting the stated goal; it also needs client-arch artifact machinery |
| 013 | **Compose owns container definition**; dcd owns orchestration policy. Black is created with `compose run -d --name … --use-aliases --no-deps`, then `docker update --restart=…` | ends two dialects for one concept; every compose feature works without dcd knowing about it. Measured: 74 → 40 substantive YAML lines on the production config; the lines that vanish are the hard ones (container names, image logicals, per-service wait probes, the `compose.env` re-listing, the worker template, `release.run`) | **A dcd-side renderer** translating the resolved compose service into `docker run` argv — rejected because every compose feature the renderer does not know is silently dropped, an unbounded and invisible bug class; kept only for the `release.run` fallback, where it is confined to a legacy path (§7.6) |
| 014 | **Shell out to the `ssh` binary**, one ControlMaster-multiplexed connection per run, each command a separate invocation | ADR-004's grain (actions are copy-pasteable); `~/.ssh/config`, `ProxyJump`, agent auth and `known_hosts` come free. Measured: multiplexing removes ~120 ms of setup per command (loopback, n=1) | **In-process ssh (russh/thrussh/libssh2)** — russh drags tokio into a synchronous tool with no async runtime, libssh2 breaks the static-musl story, and both mean re-implementing ssh_config; **a persistent remote shell fed commands on stdin** — marginally faster than multiplexing but destroys per-command `Access`, `--dry-run` and traceability |
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
| **The target has `sshd`, a POSIX shell, `mkdir`, `mv`, `cat`, `rm`, `test`, `hostname`, and a readable `/dev/stdin`** — nothing else. Deliberately *not* assumed: `flock(1)` (which is why §2.5 uses `mkdir`), `sha256sum` (which is why §7.0 does not hash-compare), `bash`, or a login shell dcd chose | a missing tool surfaces as a named preflight error, not a mid-deploy failure. `/dev/stdin` is a Linux-ism rather than POSIX; if a target lacks it, the wrapper becomes `ssh target sh -s` with the env document **and** the command in one stdin script, which needs no `/dev/stdin` — TC-101/102 move with that change |
| **The deploying machine is Linux** (user decision, 2026-08-25: Linux CI runner or Linux workstation; no macOS/Windows) | only relevant if the push-binary model is ever revisited; the per-command model is client-platform agnostic in practice |
| **`ControlMaster` multiplexing is available** on the client's OpenSSH | without it every command pays ~148 ms instead of ~28 ms (measured §2.7) — a ~10 s deploy overhead, slow but not broken |
| **The target's login shell parses the `sh -c` wrapper as POSIX sh** | dcd invokes `sh -c` explicitly rather than trusting the login shell, so csh/fish targets still work; the quoting fixtures in §10 are the proof |
| **Compose keeps `run --name/--use-aliases/--no-deps` semantics and keeps one-off containers out of `up`'s reconciliation** | INV-13 bounds the exposure: dcd uses `compose run` only to *create*, then addresses the result by name through plain `docker`. A compose change to one-off *handling* cannot reach dcd; a change to `run`'s *creation* flags would, and IT-* covers it |
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
| OQ-5 | Does `rollback` need to pin the *whole* container spec, or is pinning the **image** enough? v2 pins the image exactly via §5.5, but the spec (volumes, env, entrypoint) comes from the **current** compose file. | an operator who edits compose and then rolls back gets old code under new config | INV-5/INV-6 wording | **Pin the image only.** This is not a new hazard: `ROLLBACK_STEPS` already includes `infra`, which has always recreated side services from the *current* compose files, so v2 extends a shipped, accepted semantic to the app container. INV-6 is an image guarantee — `rollback()` verifies recorded tags with `docker image inspect` and nothing else. A full spec snapshot is *blocked* anyway: `compose config` inlines secret values (§5.5) |
| OQ-6 | How is the ssh path tested end-to-end? | the transport is the change; unit tests alone cannot prove a real connection | §10.2 | **Adopted (prototyped 2026-08-25, works):** an sshd fixture container (alpine + openssh + docker-cli) with the host's `/var/run/docker.sock` bind-mounted and a throwaway keypair generated into the fixture dir, reusing `tests/e2e.rs`'s existing `Fixture` drop-guard. The "remote" drives the same daemon the assertions inspect, so every existing assertion still holds. **Stated limitation:** because local and remote paths are the same paths, the fixture is structurally blind to path-locality bugs — a relative `deploy_root`, `-f` resolved on the wrong side, an upload landing locally. IT-016 covers that class separately. **Two caveats to encode:** busybox `adduser -D` leaves the account password-locked and sshd refuses it even for pubkey auth; and bind-mounting the socket gives the fixture root-equivalent control of the host daemon — acceptable behind `DCD_E2E=1`, never a pattern to show in docs |
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

A deliberately different project — **no Postgres, static workers, a web-exec healthcheck escape hatch, different ports, no migrations** — proving the recipe is config-driven. TC-028 builds the plan + argv against this with zero core changes.

Under ADR-013 the fixture is now **two** files, because that is the point: the containers are declared in compose, and `dcd.yaml` carries only policy.

`compose.prod.yml` (the operator's file, unchanged by dcd):

```yaml
services:
  web:
    image: ${REGISTRY}:${WEB_TAG}
    container_name: blogapp-web
    healthcheck: { test: ["CMD", "wget", "-qO-", "localhost/up"], interval: 1s, retries: 20 }
  app:
    image: ${REGISTRY}:${APP_TAG}
    profiles: ["dcd-release"]            # recommended hardening (§5.1), not required
    restart: unless-stopped
    networks: { default: { aliases: [app] } }
  worker:
    image: ${REGISTRY}:${APP_TAG}
    profiles: ["dcd-release"]
    entrypoint: ['php', 'artisan', 'queue:work']
    restart: unless-stopped
networks:
  default: { name: blogapp_net }
```

`dcd.yaml`:

```yaml
version: 2
project: blogapp
ssh: deploy@blog.example.internal
host: blog.example.internal
compose: { files: [compose.prod.yml] }
release:
  service: app
  container_prefix: blogapp-app
  healthcheck: { exec_in: web, cmd: 'wget -qO- http://{container}:9000/up', retries: 30, interval: 1s }
  drain: 'php artisan app:shutdown'
  # no migrate: block -> migrate:before / migrate:after are skipped
cutover:
  service: web
  backend_port: 9000
  upstream_file: upstream.conf
  reload: { exec_in: web, cmd: 'nginx -s reload' }
workers:
  service: worker
  provider: { static: [default, mail] }     # static, not a dynamic provider command
  drain: 'php artisan queue:restart'
  stop_timeout: 60s
retention: { keep_releases: 3 }
stages:
  prod: {}
```

Expected plan differences the test asserts: `migrate:before`/`migrate:after` are **skipped** (no `release.migrate`); `infra` reconciles only `web`; `workers` creates exactly `worker-default` + `worker-mail` from the static list via `compose run -d --name`; healthcheck/cutover/reload resolve through the `web` **service**, not a container name or an `nginx`-named host; and the escape-hatch healthcheck form is exercised (this fixture has no `release.health`).

Compared with the v1 form of this same fixture, `dcd.yaml` drops from 48 to 23 substantive lines, and every dropped line moved to a compose file the operator was already writing.

---

## 16. Documentation deliverables (acceptance criteria)

CLAUDE.md requires four representations of the schema to change together. v2 rewrites the schema, so **all of these are acceptance criteria for the implementation, not follow-up work.** Each line is a known contradiction with this spec today.

| File | What still states the v1 model |
|------|-------------------------------|
| `src/config/mod.rs` | the whole typed `Config` — source of truth, changes first |
| `src/cli.rs` — clap help | `long_about` "runs … on the server"; `after_help`; **`--ssh` absent**; `--image` help + `value_name = "LOGICAL=TAG"`; the `"--image \`{image}\` must be logical=tag"` error and its `docker.images.{logical}` `--set`; `Gc` long help; `check_report`'s `&workers.template` and its missing `compose receives: […]` line; the `.env` probe that stats the **local** filesystem though `deploy_root` is now a target path |
| `src/cli.rs` — `SCAFFOLD` | entirely v1: `version: 1`, `docker.images`, `docker.services`, `compose.env` re-listing, `release.image`, `run.network_alias`, `release.healthcheck` as the default. Missing `ssh:`, `release.service`, `cutover.service`, top-level `services:` |
| `src/cli.rs` — `PLUGIN_STUB` | "loaded at startup" → read and executed **locally**, resolved against the config file's directory, never uploaded |
| `docs/examples/all_in_one/dcd.yaml` | every `docker.*`, `release.run.*`, `workers.template.*`, `cutover` without `service:`, `keep_images` keyed by logicals, the hook step list without `sync`, `plugins` "relative to deploy_root". Missing every v2 field |
| `AGENTS.md` | §2 schema walkthrough, §5 rules (incl. the now-**inverted** "top-level `services` is now `docker.services`"), §7 error→fix table, the minimal skeleton, "`{container}` not the alias — the #1 mistake" |
| `README.md` | "runs on the server, talks to the local Docker socket"; flags table (no `--ssh`); hook step list (no `sync`); the `COPY --from=ghcr.io/…/dcd` base-image section |
| `UPGRADE.md` | **no v2 entry exists**; CLAUDE.md mandates one. Must carry the key-by-key v1→v2 map and the one-time v1 worker reap (§7.1) |
| `CLAUDE.md` | "runs on the target server"; "**12 steps**" → 13; module map missing `src/ssh.rs` / `SshRunner` / `SshFs`; the example-config env list needs `DEPLOY_SSH` |
| `docs/examples/fpm_app/*` | `dcd.yaml` is wholly v1; its compose files declare **no** `app` or `worker` service, which v2 requires; its README describes the scp/rsync workflow |
| `docs/examples/roadrunner_app/` | `dcd.yaml` is already v2 — but `docker-compose.prod.yml` **is missing from the repo**, so the worked example cannot be `dcd check`ed, and its README still documents the v1 scp/ssh CI block |

**Known-broken today:** `docs/examples/roadrunner_app/dcd.yaml` is v2 while `src/config/mod.rs` is v1, so CLAUDE.md's "re-run `dcd check` on both example configs" invariant fails on `master` until the implementation lands. This is deliberate — the spec leads the code — and is the first thing the implementation closes.

---
