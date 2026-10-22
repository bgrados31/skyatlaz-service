# AGENTS.md

Rules for anyone — human or agent — changing this repository. Read it before the
first edit. If a rule here conflicts with a global agent configuration, this file
wins for this project.

## 1. What this project is

A single Rust process that holds one Hypixel developer key and answers the
SkyBlock profile of the **authenticated player**, nothing else. The Minecraft mod
signs a nonce with the player's profile key pair, gets an HMAC signed token, and
uses that token to read its own profile.

Non-goals, by design:

- no account system, no database, no multi-tenancy;
- no endpoint that reads a profile for a uuid taken from the request;
- no fan-out of the Hypixel key to clients, ever.

## 2. Map of the repository

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | Router, layers, handlers, `ApiError`, HTTP tests, startup and shutdown. |
| `src/auth.rs` | Nonce store (bounded), Mojang key check, HMAC session tokens. |
| `src/keys.rs` | DER/base64 decoding and RSA signature verification. |
| `src/hypixel.rs` | Upstream client, rate limit headers, slim profile parsing. |
| `src/ratelimit.rs` | What the key's rate limit window currently allows. |
| `src/cache.rs` | Bounded TTL cache (LRU eviction) of slim profiles. |
| `src/singleflight.rs` | Coalesces concurrent cache misses for one player into one upstream call. |
| `src/config.rs` | Environment parsing and validation; fails fast on bad values. |
| `openapi.json` | The public contract. Embedded at compile time, served at `/openapi.json`. |
| `deploy/` | systemd unit, Caddyfile, environment template, Oracle runbook. |
| `docs/` | Architecture and roadmap. |

## 3. Commands

There is no Makefile or justfile; cargo is the interface.

```powershell
# Windows / PowerShell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test
& "$env:USERPROFILE\.cargo\bin\cargo.exe" fmt --all
& "$env:USERPROFILE\.cargo\bin\cargo.exe" clippy --all-targets -- -D warnings
& "$env:USERPROFILE\.cargo\bin\cargo.exe" build --release
```

```bash
# Linux / CI
cargo test --all-targets --locked
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
```

Run locally with a `.env` (see `.env.example`), then `curl http://127.0.0.1:8080/health`.

The toolchain is pinned in `rust-toolchain.toml`. Do not bump it in a feature
commit; bump it in its own commit after the full matrix passes.

## 4. Invariants

Breaking any of these is a bug, not a trade-off.

1. **The Hypixel key never leaves the process.** It is not logged, serialized,
   returned in an error, or put in a URL.
2. **Identity comes from the token only.** `validate_token` is the single source
   of the uuid. Never read a player uuid from a query string, body or header.
3. **Nonces are single use and bounded.** `Challenges` has a hard cap, and the
   per-address window is capped too (`MAX_TRACKED_IPS`). Every new unbounded map
   reachable from a public endpoint is a memory exhaustion bug.
4. **Every cache and queue has a capacity.** `TtlCache` evicts by least recent
   use; never replace it with a plain `HashMap` that grows with traffic.
5. **No `std::sync::Mutex` guard is held across an `.await`.** Lock, read or
   write, drop, then await. Clippy will not catch this for you.
6. **No panic on client input.** Parse errors become `ApiError` with a JSON body.
   The release profile uses `panic = "abort"`, so a panic is a process restart.
7. **The contract and the code change together.** A new field, status code or
   header goes into `openapi.json` and into a test in the same commit.
8. **The service listens on loopback in production.** TLS and the public address
   belong to Caddy. `BIND=0.0.0.0:…` is for containers only, deliberately.
9. **Tokens and errors carry no secrets.** Error messages describe what the
   client can fix, not the state of the key or the upstream account.

## 5. Testing policy

- Unit tests live in a `#[cfg(test)] mod tests` next to the code they cover.
- HTTP tests live in `src/main.rs` and send real requests through the real
  router with `tower::ServiceExt::oneshot`, layers included.
- **No test touches the network.** Hypixel answers are `serde_json::json!`
  fixtures; RSA keys are generated in the test.
- **A new behaviour needs a new test.** A bug fix starts with the test that
  reproduces it.
- Tests must be deterministic and fast: no `sleep` longer than a few tens of
  milliseconds, no dependency on wall clock ordering.
- Cover the failure path, not only the happy path: expired, missing, malformed,
  oversized, unauthorised.
- Before finishing, run `fmt`, `clippy -D warnings`, `test` and `build --release`.
  Report the real result; never claim a command passed without running it.

What is not covered by tests, and must be checked by hand when touched: the live
Hypixel API shape, TLS and Caddy, systemd. The runbook in `deploy/README.md` is
the manual test for those.

## 6. Performance budgets

| Thing | Budget | Where |
| --- | --- | --- |
| Upstream requests | 120 per 5 minutes for a personal key (some advertise 300) | `ratelimit.rs` |
| Profile cache | `PROFILE_TTL_SECONDS` = 300 s, 12 upstream requests per player per hour | `cache.rs` |
| Cache memory | `PROFILE_CACHE_ENTRIES` = 1024 entries, hard cap 100 000 | `config.rs` |
| Cached `GET /v1/profile` | under 10 ms p95 | measured with `curl`, see below |
| Unauthenticated surface memory | bounded by `MAX_CHALLENGES` and the body limit | `auth.rs`, `main.rs` |
| Upstream timeout | 20 s | `hypixel.rs` |
| Whole request timeout | 30 s | `main.rs` |
| Request body | 64 KiB | `main.rs` |

Rules:

- Do not optimize without a number. Measure before and after, and record the
  result in `docs/ROADMAP.md`.
- Prefer removing work over making work faster: cache hits beat micro-tuning.
- The slim profile exists to keep the cache small. Do not start passing whole
  upstream sections through "for later"; each field costs memory per entry.

Quick measurement:

```powershell
# p50/p95 of the cached path. curl reports the request time without the process
# start, which is the number that matters.
$times = 1..100 |
  ForEach-Object { [double](curl.exe -s -o NUL -w "%{time_total}" http://127.0.0.1:8080/health) } |
  Sort-Object
"p50={0:N2}ms p95={1:N2}ms" -f ($times[49] * 1000), ($times[94] * 1000)
```

## 7. Code style

- Rust identifiers, comments, commit messages and documentation in English.
  Talk to the user in Spanish; write to the repository in English.
- `cargo fmt` decides formatting. `clippy::all` at `-D warnings` decides lints.
  Both are configured in `Cargo.toml` and `rustfmt.toml`; do not silence a lint
  with `#[allow]` without a comment explaining why.
- Comments explain **why**. The code already says what it does.
- Errors: keep the manual `Display` implementations. Do not add `thiserror` or
  `anyhow` for a project this size.
- Public items carry a doc comment. `missing_docs` is not enabled, so this is on
  you.
- No new dependency without answering: can std or an existing dependency do it,
  is it maintained, what is its licence, and how much does it add to the binary?
  Write the answer in the commit message. Prefer `rustls` over OpenSSL.

## 8. Security

- Secrets come from the environment. `.env` is gitignored; `deploy/*.env` files
  with real values are never committed.
- Never log a token, a nonce signature, an API key, or a whole `Authorization`
  header. `tracing` spans include method, path and status; that is enough.
- Compare secrets in constant time (`hmac::Mac::verify_slice`), never with `==`.
- `/v1/auth/challenge` is unauthenticated and writes to memory. Before the
  service is reachable from the internet, it needs a per-IP limit at the edge
  (see `deploy/Caddyfile` and `docs/ROADMAP.md`).
- TLS terminates at Caddy. Do not expose plain HTTP to the internet.
- Keep dependencies current through Dependabot. Run `cargo audit` before a
  release; a vulnerable dependency is a release blocker.

## 9. Definition of done

A change is finished when all of these are true:

- [ ] `cargo fmt --all --check` passes.
- [ ] `cargo clippy --all-targets -- -D warnings` passes.
- [ ] `cargo test --all-targets` passes, with new tests for the new behaviour.
- [ ] `cargo build --release` succeeds.
- [ ] `openapi.json` and its tests match the handlers.
- [ ] New environment variables appear in `.env.example`, in
      `deploy/skyatlaz-service.env.example` and in the README table.
- [ ] `README.md` and `docs/ROADMAP.md` describe the current state.
- [ ] No secret, key, token or personal path in the diff.
- [ ] If routing, auth or headers changed: a manual `curl` against a running
      binary confirms it.
- [ ] If deployment changed: `deploy/README.md` has the new step.

## 10. Do not

- Do not rewrite a module when a focused patch is enough.
- Do not add a database, a message queue, Redis or a second process to solve a
  problem this process can solve. Write an ADR in `docs/ARCHITECTURE.md` first if
  you believe it is needed.
- Do not relax the body limit, the timeouts or the cache bounds to make a test
  or a client pass.
- Do not change the default `BIND` to a public interface.
- Do not commit `target/`, `.env`, `.idea/` or `.codebase-memory/`.
- Do not bump dependencies and the toolchain in the same commit as a feature.
- Do not trust a cached value for identity: the token decides, the cache only
  stores.

## 11. Continuity

- `docs/ROADMAP.md` is the single place that says what is done and what is next.
  Update it when you finish or discover work.
- If a task is left half done, write the state to `HANDOFF.md` at the repository
  root: objective, what was done, what is missing, files touched, useful
  commands.
- Commit messages: `area: what changed and why`, imperative, one concern per
  commit (`cache: bound the map so a burst cannot grow memory`).
