# Roadmap

Priorities: **P0** must be true before the service is reachable from the
internet, **P1** is the next round of work, **P2** is later or depends on
traffic. This file is the source of truth for what is done and what is next; see
`AGENTS.md` for the rules.

## Done

- [x] Bounded profile cache with LRU eviction and a configurable capacity.
- [x] Bounded nonce store (`MAX_CHALLENGES`), single use enforced on consume.
- [x] Per-address limit on the challenge endpoint: 30 per minute, `429` with
      `Retry-After: 60`, and a capped map of tracked addresses (`MAX_TRACKED_IPS`).
- [x] The limit keys on the last `X-Forwarded-For` entry when the peer is the
      local proxy, and on the socket address otherwise, so a proxy cannot
      collapse every player into one bucket and a client cannot pick its own.
- [x] Environment validation: the process refuses to start on a zero ttl, a bad
      port or a cache size out of range, and names the variable.
- [x] `PORT` support for containers and PaaS, `BIND` still wins.
- [x] Per-request hardening: 64 KiB body limit, 30 s timeout, `nosniff`,
      `Cache-Control: no-store` under `/v1`.
- [x] Honest error semantics: `404 profile_not_found`, `503 rate_limited` with
      `Retry-After`, `WWW-Authenticate` on 401, JSON for unknown routes.
- [x] Hypixel `429` mapped to a wait instead of a bad gateway.
- [x] Upstream retry: one extra attempt for a `5xx`, a connection that did not
      open or a timeout, with 200–400 ms of jitter. Never for a `4xx`, a rate
      limit or a player without a profile, because none of those change on a
      second look.
- [x] Single-flight on cache misses: concurrent requests for one uuid share one
      upstream call, success or failure, through a bounded table of weak flights.
      A finished flight removes its entry, a cancelled one is dropped by the next
      lookup and cannot wedge the uuid. No new dependency: the change only turns
      on the `sync` feature of the `tokio` already in the tree. Fifty concurrent
      requests for one player produce one upstream call in a test with a fake
      upstream.
- [x] Tests against a fake upstream on a loopback port, written with `axum`
      (already a dependency, no mock crate): `429` becomes a wait and is not
      retried, a `5xx` is tried once more and then reported, a second call that
      succeeds answers the profile, a `403` and a missing profile are not
      retried.
- [x] Graceful shutdown on SIGTERM and Ctrl-C.
- [x] Structured request logging with method, path, status and latency.
- [x] `/health` reports the upstream rate limit state and the cache size.
- [x] `/metrics` in the Prometheus text format: request counts per route and
      status class, a latency histogram per route, cache hits and misses, profile
      fetches by outcome, retries, and three gauges. Fixed label sets, atomics
      only, no new dependency, and refused from the internet in the Caddyfile.
- [x] HTTP tests through the real router (`oneshot`), contract tests, and unit
      tests for every new rule: 76 tests, no network.
- [x] One RSA key pair per test binary (`OnceLock`) instead of one per test,
      which took the suite from about 45 s to under 5 s in a debug build.
- [x] CI (fmt, clippy `-D warnings`, tests on Linux and Windows, release build),
      pinned toolchain, rustfmt and clippy configuration, Dependabot.
- [x] Container image, `.dockerignore`, MIT licence, `.editorconfig`,
      `.gitattributes`.
- [x] `AGENTS.md`, `docs/ARCHITECTURE.md`, this file, README updates.
- [x] Repository material that answers a stranger's questions without asking
      anybody: README, `CONTRIBUTING.md`, `SECURITY.md`, `CHANGELOG.md`,
      `CODE_OF_CONDUCT.md`, issue and pull request templates, a weekly
      `cargo audit` job and a tag triggered release job.
- [x] Published privately as `bgrados31/skyatlaz-service`. The history was
      rewritten before the first push to drop a development
      `SKYATLAZ_TOKEN_SECRET` that had been committed in an environment
      template; the value had already been rotated and no deployment used it.
- [x] Tagged release artifacts: static musl binaries for x86_64 (the runner's own
      compiler) and aarch64 (through `cross`, because Ubuntu's aarch64 gcc is a
      glibc compiler and `ring` cannot compile its C for a musl target with it),
      a `SHA256SUMS`, and a check that both binaries really are static before
      they are published. `v0.1.0` exercised the job end to end.

## Measurements

Taken on the development machine (Windows 11, release build, loopback, one
instance, one cached player profile). Method: `curl` `time_total` over 100
sequential requests.

| Case | p50 | p95 | max |
| --- | --- | --- | --- |
| `GET /health` | 0.84 ms | 1.15 ms | 11.16 ms |
| `GET /v1/profile` (cache hit, 71.5 KB) | 1.92 ms | 5.42 ms | 12.80 ms |
| `GET /v1/profile` (cache miss) | 1.23 s | – | – |

Other numbers from the same run:

- Release binary: 2.4 MB (2 525 184 bytes), stripped, no OpenSSL.
- Upstream headers reported by the key: `limit=300`, `remaining=299` after one
  call. The default personal window was assumed to be 120/5 min; the key
  advertises 300, so capacity is better than the estimate in
  `docs/ARCHITECTURE.md`. Re-check with your own key before sizing.
- Cold profile answer: 71 541 bytes, down from roughly 1 MB raw.
- Second identical request returns the same `fetched_at`: the cache is what
  serves it.

Re-run after any change that touches the cache, the router or serialization. A
regression of more than 2× in the cached path is a bug.

## P0 — before the URL is public

1. **Rate limit at the edge as well.** The service now limits the challenge
   endpoint per address (30 per minute, 10 000 addresses tracked, `429` with
   `Retry-After`), which stops one client. A distributed flood still reaches the
   process, so add a per-IP limit in `deploy/Caddyfile` as the first line of
   defence.
   *Done when:* a loop of 200 requests from one IP is throttled at the edge, and
   a normal client still gets a nonce.
2. **Set a real token secret.** The development `.env` has a 14 byte secret, and
   the service warns about it. Generate 32 random bytes, put them in
   `/etc/skyatlaz-service.env` (mode 600), restart.
   *Done when:* the startup log has no warning and a token still validates after
   `systemctl restart`.
3. **Verify the public path.** TLS certificate issued, HTTP redirected to HTTPS,
   `curl https://host/health` works from outside, port 8080 is not reachable from
   outside.
4. **Survive a reboot.** `systemctl enable` was run, the instance reboots, the
   service is up and `/health` answers without manual intervention.
5. **Cap the logs.** `journalctl --vacuum-size=100M` or a `SystemMaxUse` in
   `journald.conf`, plus Caddy log rotation. A full disk takes the service down.
6. **Size the cache for the key.** Confirm the advertised rate limit on `/health`
   and choose `PROFILE_CACHE_ENTRIES` / `PROFILE_TTL_SECONDS` for the expected
   player count. With a 300 s ttl each player costs 12 requests per hour.
7. **Record the deployment.** Keep a copy of the env file (offline, not in git)
   and the exact commit that is running.

## P1 — next round

1. **Malformed and slow upstream, end to end.** The client side is covered: a
   truncated body is not retried and a bad answer becomes an `ApiError`. What no
   test does is drive the whole router against an upstream that answers with
   broken JSON or too slowly. *Done when:* a `oneshot` request through the router
   against a fake upstream that sends `not json`, or sleeps past the timeout,
   answers `502`.
2. **The rest of the supply chain check.** The weekly job runs `cargo audit` for
   advisories now. `cargo deny` would add licences and duplicate crates; add it to
   the same workflow when there is time to triage the first report.
3. **Fuzz the parser.** `cargo-fuzz` targets for `parse_profile` and
   `parse_mining`. Both consume untrusted JSON that changes without notice.
4. **Split liveness and readiness** if a load balancer is added: `/health` stays
   dependency free, `/ready` can report whether the upstream is usable.
5. **Turn on private vulnerability reporting if the repository goes public.**
   `SECURITY.md` points at the Security tab, and GitHub refuses the feature while
   the repository is private: `PUT .../private-vulnerability-reporting` answers
   `404`.
   *Done when:* the advisory form is reachable from a public repository.

## P2 — later

1. **Multiple instances.** Tokens already work across instances that share the
   secret; the cache does not. Add Redis only when a second instance exists and
   the hit rate actually matters. Write the ADR in `docs/ARCHITECTURE.md` first.
2. **Distributed tracing.** `tracing-opentelemetry` plus an OTLP collector, if the
   service ever runs more than one process.
3. **Edition 2024 and a declared MSRV.** Mechanical, but it needs a full matrix
   run; do it in its own commit.
4. **Move the RustCrypto family with `rsa`.** hmac 0.13, sha1 0.11, sha2 0.11 and
   digest 0.11 do not build against `rsa` 0.9, which is on `digest` 0.10; the
   `ignore` rules in `.github/dependabot.yml` keep the bot from proposing the
   jump until `rsa` moves. base64 0.23 and tower-http 0.7 were checked locally
   and do compile, so they are left to the bot.
5. **Token revocation.** Today the only way to invalidate a token is to rotate the
   secret, which invalidates all of them. A short token ttl plus a per-player
   "not before" timestamp would be enough if it is ever needed.
6. **More endpoints.** Only if the mod needs them, and only with the same rule:
   the token decides which player, never the request.

## Known risks

| Risk | Impact | Mitigation now | Mitigation later |
| --- | --- | --- | --- |
| One key, one rate limit | Service refuses new players at peak | Cache ttl of 300 s | Second key/instance, or a production key |
| Single process, single VM | A restart or a dead VM is downtime | systemd `Restart=always` | Health check + second instance |
| No alerts on top of the counters | Failures are found by users, unless somebody watches `/metrics` | `/metrics` + `/health` + logs | Alerting rules (P2) |
| Public challenge endpoint | Memory or CPU abuse | Bounded store, body limit | P0 per-IP limit |
| Dependency drift | A vulnerable crate ships | Dependabot + weekly `cargo audit` | `cargo deny` for licences and duplicates (P1) |
| `RUSTSEC-2023-0071` in `rsa` | None today: the service verifies signatures with a public key and never signs, so there is no private key to leak | Ignored in `.cargo/audit.toml` with the reason, so the weekly job stays meaningful | Drop the ignore if the service ever signs, or when `rsa` becomes constant time |
| `panic = "abort"` | A panic is a restart, not a 500 | No panics on input, tests | Keep the rule |

## Not planned

- A database. There is no state worth storing; the cache and the tokens are
  derived data.
- Accounts, passwords or email. Authentication is a Minecraft key pair.
- A queue or a worker. Every request is answered in one hop.
- Caching the raw upstream profile. The slim profile is the whole point.
