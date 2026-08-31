# The upstream dotenv oracle

`src/dotenv/` is a Rust port of `symfony/dotenv` 8.1. A hand-transcribed test suite
proves only what the transcriber remembered to copy, so the port's oracle is
upstream's own file:

| File | What it is |
|------|------------|
| `DotenvTest.php` | verbatim copy of `symfony/dotenv` **8.1** `Tests/DotenvTest.php` (MIT, header intact) |
| `extract_cases.php` | dumps its `getEnvData` + `getEnvDataWithFormatErrors` providers to JSON |
| `dotenv_cases.json` | the extracted cases — 93 values, 19 errors — committed, so the Rust tests need neither PHP nor network |

`src/dotenv/tests.rs` runs every case out of the JSON. **All of them must pass.** The
one list of deliberate differences is `REFUSED_BY_DESIGN` — the completed `$(…)`
expressions dcd refuses instead of shell-executing (spec §5.2.1) — named by exact
input, so a new upstream command case fails the suite rather than slipping through.

## Refreshing the copy

```bash
curl -sO https://raw.githubusercontent.com/symfony/dotenv/8.1/Tests/DotenvTest.php
docker run --rm -v "$PWD/tests/fixtures/symfony:/work" -w /work php:8.3-cli \
  php extract_cases.php > tests/fixtures/symfony/dotenv_cases.json
cargo test --lib dotenv
```

A case that starts failing is the point of the exercise: either the port has a gap,
or upstream changed behaviour and the spec (§5.2.1) has to say which one dcd follows.
