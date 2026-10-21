# Changelog

Notable changes to this service, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), the
versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html), and
while the version is `0.x` a change to `openapi.json` may land in a minor
release. Each section says what the mod has to do about it.

## [Unreleased]

### Added

- A weekly `cargo audit` job, so a crate that gains an advisory shows up as an
  issue even when nothing in this repository changed.

### Security

- The weekly audit ignores `RUSTSEC-2023-0071` (the Marvin attack on the `rsa`
  crate, unpatched) because the service verifies signatures with a public key
  and never signs, so it holds no private key to leak. The reason is written
  next to the ignore in `.cargo/audit.toml` and in `SECURITY.md`, so the entry
  cannot be mistaken for noise.

### Known gaps

- A cache miss is not deduplicated: two requests for the same player in the same
  instant both call Hypixel.

## [0.1.0] - 2026-10-21

The first version that talks to the mod: a player authenticates as themselves and
reads their own SkyBlock profile, while the Hypixel key stays in the service.

### Added

- **Authentication without a password.** `GET /v1/auth/challenge` hands out a
  single use nonce; `POST /v1/auth/verify` checks the signature over that nonce
  (the caller owns the private key) and Mojang's signature over the public key
  (the key belongs to the uuid in the request). Only then does a token exist.
- **Stateless session tokens.** An HMAC over `uuid:expires_at`, valid for one
  hour by default, verified in constant time. A restart with the same secret
  keeps sessions alive; a leaked token cannot be edited without breaking the MAC.
- **`GET /v1/profile`**, the slim profile of the authenticated player: mining,
  dungeons, currencies and the levels the mod draws. The upstream answer is about
  1 MB, the cached one about 70 KB.
- **A bounded cache** with a TTL of 5 minutes and least-recently-used eviction,
  1 024 entries by default and a hard cap of 100 000, so one player asks Hypixel
  at most twelve times an hour.
- **Rate limit awareness.** The state of the key's window is read from the
  upstream headers; when it is spent the service answers `503 rate_limited` with
  a `Retry-After` instead of burning the last request.
- **One retry, only when it can help.** A `5xx`, a refused connection or a
  timeout is tried once more after 200–400 ms of jitter. Never a `4xx`, never a
  spent rate limit, never a player without a profile.
- **Honest errors.** `401 unauthorized`, `404 profile_not_found`,
  `429 too_many_requests`, `502 upstream_error`, `503 rate_limited`, every one of
  them JSON with an `error` code, and the cause of an upstream failure only in
  the log.
- **Bounded unauthenticated surface.** 10 000 live nonces, 30 challenges per
  address per minute across at most 10 000 addresses, a 64 KiB body limit, a 20 s
  upstream timeout and a 30 s request timeout.
- **`GET /metrics`** in the Prometheus text format: request counts per route and
  status class, a latency histogram, cache hits and misses, profile fetches by
  outcome, retries and gauges for the cache size and the remaining budget. Labels
  come from closed sets, so a caller cannot widen the table.
- **`GET /health`** with the version, the upstream budget and the cache size.
- **`GET /openapi.json`**, the contract (OpenAPI 3.1), embedded at compile time,
  with tests that fail when the code and the document disagree.
- **Configuration from the environment** with validation at startup: a bad value
  is a refusal to start that names the variable, not a surprise at runtime.
- **Deployment material**: a systemd unit, a Caddyfile that terminates TLS and
  answers a public `/metrics` with `404`, an environment template and a runbook
  for an Oracle Cloud Always Free instance.
- **A tagged release publishes both Linux artifacts**: static musl binaries for
  `x86_64` and `aarch64`, a `SHA256SUMS` and the deploy pointer, so a deploy is
  a download instead of a compile on a small VM.

### Security

- The Hypixel key never leaves the process: not logged, not serialized, not
  returned in an error and never put in a url.
- The identity of a request comes from its token only. There is no endpoint that
  reads the profile of a uuid taken from the request.
- The repository history was rewritten before the first publication to drop a
  development `SKYATLAZ_TOKEN_SECRET` that had been committed in an environment
  template. The value had already been rotated; no deployment ever used it.

[Unreleased]: https://github.com/bgrados31/skyatlaz-service/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/bgrados31/skyatlaz-service/releases/tag/v0.1.0
