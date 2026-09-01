# dcd — Strategic Blueprint

**Type:** Strategic
**Status:** Decided (2026-06-11); **v2 revision 2026-08-25** — execution locus and container
dialect changed (ADR-001 superseded, ADR-013/014 added). No backwards compatibility with v1 configs.
**Implementation detail:** [Implementation Spec](implementation-spec.md)

---

## 1. The problem (7Q-1)

A working zero-downtime **red-black** Docker deploy trapped in a 269-line single-purpose Bash
script: untyped, untested, project-hardcoded, unreusable. One-man ops needs to re-run and
evolve it without re-reading 269 lines each time, and to reuse the machinery on future projects.

**Solve:** a **reusable, typed, fully tested CLI** running the red-black Docker deploy from a
YAML config, with PHP-Deployer-style hook/plugin extensibility for project-specific parts. v1
reaches feature-and-behaviour parity with the original script (and fixes its latent bugs),
driven entirely by config — generality proven with a *synthetic second config* in the test
suite, not a second live project.

**Implication:** the core is a fixed linear recipe with before/after hook slots; red-black is
the one built-in recipe; project specifics enter via YAML config and optional Lua plugins
(Implementation Spec §5, §6).

## 2. Success metrics (7Q-2)

| Metric | Target |
|--------|--------|
| Replace the existing deploy script in production | 1 deploy reaches prod via `dcd` with behaviour matching the original script (every step in §3 reproduced), by first adopting commit |
| Config-generality (no second project needed) | a second, **synthetic** project config in the test suite drives the same recipe with **zero core changes** — the generality proof |
| Latent-bug fixes over the original script | the 5 gate-found defects (blanket image prune deleting rollback targets; dropped upstream self-heal; no rollback; no crash-recovery state; no concurrency lock) are all closed |
| Lua needed for the common case | 0 lines (YAML-only path works end-to-end) |
| Test coverage of engine + recipe logic | every task, hook-ordering rule, config-merge rule, invariant (§1), and error path has a test; no untested branch in engine/recipe/config |
| Operator error recovery | every failure mode in the Error Handling Matrix (Implementation Spec §11) leaves the system in a stated, recoverable state, with a defined recovery command |

## 3. Why this wins (7Q-3)

Structural, not cosmetic:

- A **typed, tested Rust recipe + typed config** cures the script's three root sins — untyped,
  untested, hardcoded.
- Shelling out to the `docker` CLI (not a Docker API client) makes the tool's actions exactly
  the commands an operator would run by hand: auditable in `--dry-run`, debuggable by
  copy-paste, immune to Engine-API drift.
- In v2 the binary runs on the **deploying machine**. The server needs only `docker`, `sshd`
  and a POSIX shell, and **nothing dcd-authored is installed there**.
- Container *definition* moves entirely to compose (ADR-013) — dcd stopped re-spelling
  image/env/volume/restart/alias options in a private dialect, so the config carries
  orchestration policy only.
- The PHP-Deployer hook model is proven and liked; keeping it (without an over-built task
  graph) gives extensibility at low weight.

## 4. Core architecture decision (7Q-4)

| Decision | Choice | ADR |
|----------|--------|-----|
| Execution locus | Runs **on the deploying machine**; the target is reached over **SSH** | ADR-001 (v2; supersedes ADR-001 v1) |
| Container dialect | **Compose owns container definition**; dcd owns orchestration policy | ADR-013 |
| SSH mechanics | Shell out to the `ssh` binary, one multiplexed connection per run | ADR-014 |
| Engine shape | **Fixed linear recipe + before/after hook slots** (one built-in: `docker-redblack`) | ADR-002 |
| Extensibility | **Embedded Lua (mlua)**, sandboxed; PHP-Deployer-style globals + `ctx` | ADR-003 |
| Docker interface | **Shell out to `docker` / `docker compose` CLI** | ADR-004 |
| Rollback semantics | **Code-only; migrations forward-only (expand-contract)** | ADR-005 |
| Secrets | **Symfony-style dotenv chain at the launch source; process-env passthrough to containers; nothing dcd-written at rest on the server** (spec §5.2) | ADR-011/012 (supersede ADR-006) |
| Multi-stage | **One config file, stages merged over a shared base** + host guard | ADR-007 |
| Common-case Lua | **Zero Lua required** — YAML drives the recipe | ADR-008 |
| Output | **Adaptive**: rich TTY / plain non-TTY / `--json` opt-in | ADR-009 |
| Naming | binary **`dcd`**, config **`dcd.yaml`**, plugins `plugins/*.lua` | ADR-010 |

**On ADR-002 (engine weight):** the Design Challenge argued a generic task-DAG with cycle
detection is heavier than one linear recipe needs. Adopted in part — Lua + the `before`/`after`
hook model is kept (an explicit requirement), but the engine is a **fixed ordered recipe with
hook slots**, not a user-definable DAG.

## 5. Tech-stack rationale (7Q-5)

| Choice | Rationale | Constraint it satisfies |
|--------|-----------|-------------------------|
| Rust | Single static binary, no runtime, strong typing, exhaustive error handling | "enterprise grade", "handle errors gracefully", ship one file |
| `mlua` (Lua 5.4, vendored) | Mature embedded-Lua binding, sandboxable, no system Lua dependency | "extensible like PHP Deployer", static binary |
| `clap` (derive) | De-facto Rust CLI framework: help, errors, completions | "PERFECT DX" |
| `serde` + `serde_norway` | Typed config parse with precise error spans; the maintained fork of `serde_yaml`, same API and same `Value`/`Mapping` (OQ-3) | typed config, good errors |
| `signal-hook` + std's `File::try_lock` | Caught signals + OS-released advisory lock, locally and (leased, over ssh) on the target. The lock moved to std in Rust 1.89, retiring the `fs2` dependency | INV-4 (the lock cannot outlive its holder, in either mode) |
| Shell out to `docker` | Parity with the current script; `docker compose` has no API | ADR-004 |
| `musl` static target | Runs on any x86-64 Linux server without glibc concerns | ship one file, "on the server" |

## 6. MVP features (7Q-6)

v1: `deploy` (+ `--resume`, `--dry-run`), `rollback`, `status`, `tasks`, `config check`,
`init`; the built-in `docker-redblack` recipe; YAML-only common path; sandboxed Lua plugins;
dynamic + static workers; multi-stage + host guard; concurrency lock with crash recovery;
adaptive output.

## 7. NOT building in v1 (7Q-7)

| Excluded | Rationale |
|----------|-----------|
| **A second live project** | Decided 2026-06-11: the candidate second project is symlink/PHP-FPM/MariaDB/build-on-server/SSH-remote — every axis dcd excludes. v1 targets parity with the current production deploy + a synthetic generality test; revisited post-v1 |
| Image **build/push** | Stays in CI where BuildKit secrets + content-hash caching live; building on the server needs source there |
| ~~**SSH-remote transport**~~ | **Adopted in v2** (ADR-001/014). v1 deferred it as "a future `Executor` impl"; that is what v2 builds |
| **Non-Linux deploying machines** | Decided 2026-08-25: deploys run from a Linux CI runner or Linux workstation only. No macOS/Windows client |
| **A persistent remote shell** (one `ssh` session fed commands on stdin) | Connection *reuse* is mandatory (ADR-014), but each command stays a separate, individually traceable, dry-run-gated `ssh` invocation. Measured: multiplexing alone removes 81% of per-command overhead |
| **Pushing the dcd binary to the server** | Considered and rejected 2026-08-25 (ADR-001 alternatives): it preserves more invariants for free, but the deciding binary would run on the remote again — inverting the stated goal |
| **Down-migration rollback** | Expand-contract makes DB rollback unnecessary and usually lossy (ADR-005) |
| **Non-Docker deploy** (symlink, k8s, swarm) | Out of scope; the recipe is Docker-CLI + compose specific |
| **A user-definable task DAG** | One fixed recipe + hook slots covers the need; a general graph is unjustified weight (ADR-002) |
| **A daemon / web UI / scheduler** | A CLI invoked by CI or by hand is the whole surface |
| External secrets managers (Vault/SSM) | Heavy dependency for one-man ops; the dotenv-chain path covers the need (ADR-011) |

## 8. References

| Content | Location |
|---------|----------|
| Architecture, engine, lifecycle, state | [Implementation Spec §2–4](implementation-spec.md#2-architecture) |
| Config schema | [Implementation Spec §5](implementation-spec.md#5-configuration-schema-dcdyaml) |
| Lua plugin API | [Implementation Spec §6](implementation-spec.md#6-lua-plugin-api) |
| Built-in red-black recipe (every task) | [Implementation Spec §7](implementation-spec.md#7-built-in-recipe-docker-redblack) |
| CLI surface | [Implementation Spec §8](implementation-spec.md#8-cli-surface) |
| Anti-patterns | [Implementation Spec §9](implementation-spec.md#9-anti-patterns-do-not) |
| Test specifications | [Implementation Spec §10](implementation-spec.md#10-test-case-specifications) |
| Error handling matrix | [Implementation Spec §11](implementation-spec.md#11-error-handling-matrix) |
| ADRs | [Implementation Spec §12](implementation-spec.md#12-architecture-decision-records) |
