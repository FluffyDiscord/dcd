# dcd — Strategic Blueprint (Strategic)

**Document type:** Strategic
**Status:** Decided (2026-06-11), revised post-gate-review
**Implementation detail lives in:** [Implementation Spec](implementation-spec.md)

---

## 1. The problem (7Q-1)

An existing production deploy script performs a working zero-downtime **red-black** Docker
deploy, but it is a 269-line single-purpose Bash script: untyped, untested,
project-hardcoded, and impossible to reuse safely. A one-person operation needs
to evolve and re-run this deploy without re-reading 269 lines of Bash each time,
and to reuse the same machinery on future projects.

**Solve:** give a single operator (one-man ops) a **reusable, typed, fully
tested CLI** that runs the red-black Docker deploy from a YAML config, with
PHP-Deployer-style hook/plugin extensibility for the project-specific parts. v1
must reach feature-and-behaviour parity with the original script (and fix its latent
bugs), driven entirely by config — proving generality with a *synthetic second
config* in the test suite, not a second live project.

**Implementation Implication:** the core is a fixed linear recipe with
before/after hook slots; the red-black flow is the one built-in recipe; project
specifics enter via YAML config and optional Lua plugins (Implementation Spec
§5, §6).

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

Structural, not cosmetic: a **typed, tested Rust recipe + typed config** cures
the three root sins of the script (untyped, untested, hardcoded) directly.
Shelling out to the `docker` CLI (not a Docker API client) means the tool's
actions are exactly the commands an operator would run by hand — auditable in
`--dry-run`, debuggable by copy-paste, immune to Engine-API drift. A single
static binary deploys with zero runtime dependencies on the server beyond the
`docker` CLI already present. The PHP-Deployer hook model is proven and liked;
keeping it (without an over-built task graph) gives extensibility at low weight.

## 4. Core architecture decision (7Q-4)

| Decision | Choice | ADR |
|----------|--------|-----|
| Execution locus | Runs **on the target server**, local Docker socket | ADR-001 |
| Engine shape | **Fixed linear recipe + before/after hook slots** (one built-in: `docker-redblack`) | ADR-002 |
| Extensibility | **Embedded Lua (mlua)**, sandboxed; PHP-Deployer-style globals + `ctx` | ADR-003 |
| Docker interface | **Shell out to `docker` / `docker compose` CLI** | ADR-004 |
| Rollback semantics | **Code-only; migrations forward-only (expand-contract)** | ADR-005 |
| Secrets | **Symfony-style dotenv chain at the launch source; process-env passthrough to containers; nothing dcd-written at rest on the server** (spec §5.2) | ADR-011/012 (supersede ADR-006) |
| Multi-stage | **One config file, stages merged over a shared base** + host guard | ADR-007 |
| Common-case Lua | **Zero Lua required** — YAML drives the recipe | ADR-008 |
| Output | **Adaptive**: rich TTY / plain non-TTY / `--json` opt-in | ADR-009 |
| Naming | binary **`dcd`**, config **`dcd.yaml`**, plugins `plugins/*.lua` | ADR-010 |

**On ADR-002 (engine weight):** the Design Challenge argued a generic task-DAG
with cycle detection is heavier than one linear recipe needs. Adopted in part:
Lua + the `before`/`after` hook model is kept (an explicit requirement), but the
engine is a **fixed ordered recipe with hook slots**, not a user-definable DAG —
removing the general-graph machinery while preserving the PHP-Deployer feel.

## 5. Tech-stack rationale (7Q-5)

| Choice | Rationale | Constraint it satisfies |
|--------|-----------|-------------------------|
| Rust | Single static binary, no runtime, strong typing, exhaustive error handling | "enterprise grade", "handle errors gracefully", ship one file |
| `mlua` (Lua 5.4, vendored) | Mature embedded-Lua binding, sandboxable, no system Lua dependency | "extensible like PHP Deployer", static binary |
| `clap` (derive) | De-facto Rust CLI framework: help, errors, completions | "PERFECT DX" |
| `serde` + `serde_yaml` (or `serde_yml`) | Typed config parse with precise error spans | typed config, good errors |
| `signal-hook` + `fs2`/`rustix` flock | Caught signals + OS-released advisory lock | INV-4 (lock survives SIGKILL) |
| Shell out to `docker` | Parity with the current script; `docker compose` has no API | ADR-004 |
| `musl` static target | Runs on any x86-64 Linux server without glibc concerns | ship one file, "on the server" |

## 6. MVP features (7Q-6)

In v1: `deploy` (+ `--resume`, `--dry-run`), `rollback`, `status`, `tasks`,
`config check`, `init`; the built-in `docker-redblack` recipe; YAML-only common
path; sandboxed Lua plugins; dynamic + static workers; multi-stage + host guard;
concurrency lock with crash recovery; adaptive output.

## 7. NOT building in v1 (7Q-7)

| Excluded | Rationale |
|----------|-----------|
| **A second live project** | Decided 2026-06-11: the candidate second project is symlink/PHP-FPM/MariaDB/build-on-server/SSH-remote — every axis dcd excludes. v1 targets parity with the current production deploy + a synthetic generality test; a second project is revisited post-v1 |
| Image **build/push** | Stays in CI where BuildKit secrets + content-hash caching live (the existing CI pipeline); building on the server needs source there |
| **SSH-remote transport** | `dcd` runs on the server; server selection is CI's SSH target. Deferred as a future `Executor` impl |
| **Down-migration rollback** | Expand-contract makes DB rollback unnecessary and usually lossy (ADR-005) |
| **Non-Docker deploy** (symlink, k8s, swarm) | Out of scope; the recipe is Docker-CLI + compose specific |
| **A user-definable task DAG** | One fixed recipe + hook slots covers the need; a general graph is unjustified weight (ADR-002) |
| **A daemon / web UI / scheduler** | A CLI invoked by CI or by hand is the whole surface |
| External secrets managers (Vault/SSM) | Heavy dependency for one-man ops; the dotenv-chain path covers the need (ADR-011) |

## 8. References

### Implementation detail lives here
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

*Strategic overview only. Implementation specs live in the linked document.*
