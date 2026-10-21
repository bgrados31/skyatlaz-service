# Architecture

This document describes the service as it is, the budgets it must respect, and
the decisions behind it. It is the reference for changes that touch more than one
module. The rules for making those changes live in `AGENTS.md`.

## 1. Context

SkyBlock mods need the Hypixel profile of the player using them. Hypixel does not
allow a mod to ship or share a developer key, so the mod cannot call the API
directly. This service holds one key and answers only the profile of the player
that proves who they are.

The mod never sends a uuid it wants to read. It proves that it holds the private
half of the player's Minecraft profile key pair, and the service derives the uuid
from that proof. There is no request shape that reads somebody else's profile.

```text
┌──────────────┐   1. GET  /v1/auth/challenge        ┌────────────────────┐
│  Minecraft   │ ──────────────────────────────────▶ │                    │
│  mod         │ ◀────────────── nonce ───────────── │  skyatlaz-service  │
│              │                                     │                    │
│  profile key │   2. POST /v1/auth/verify (signed)  │  nonce store       │
│  pair        │ ──────────────────────────────────▶ │  token (HMAC)      │
│              │ ◀────────────── token ───────────── │  profile cache     │
│              │                                     │                    │
│              │   3. GET  /v1/profile (Bearer)      │        │           │
│              │ ──────────────────────────────────▶ │        │           │
└──────────────┘ ◀──── slim profile (JSON) ───────── └────────┼───────────┘
                                                              │ API-Key
                                                              ▼
                                                     ┌────────────────────┐
                                                     │  api.hypixel.net   │
                                                     └────────────────────┘
```

## 2. Components

| Module | State | Responsibility |
| --- | --- | --- |
| `main` | none | Router, middleware layers, error mapping to HTTP, startup, graceful shutdown. |
| `config` | read once | Parses and validates the environment. A bad value is a startup error. |
| `auth` | `Mutex<Challenges>`, `secret`, `token_ttl` | Issues and consumes nonces, verifies Mojang's key signature, issues and verifies HMAC tokens. |
| `keys` | none | DER/base64 decoding and RSA verification. Pure functions. |
| `hypixel` | `reqwest::Client`, `RateLimits` | Upstream call, rate limit header bookkeeping, parsing to `SlimProfile`. |
| `ratelimit` | `Mutex<State>` | Remaining quota and when the window resets, so the service can refuse early. |
| `cache` | `Mutex<State<K, V>>` | Bounded TTL cache with LRU eviction. |
| `singleflight` | `Mutex<HashMap<uuid, Weak<Flight>>>` | One upstream call per player at a time; the concurrent misses wait for its result. |

There is no database and no external cache. Every piece of state is in this
process and is rebuilt on restart, which is the point: a restart costs tokens and
cache hits, nothing else.

## 3. Request flows

### 3.1 Challenge

`GET /v1/auth/challenge` generates 32 random bytes, base64 encodes them, stores
them with an issue time and returns `{nonce, expires_at}`. The store is bounded at
`MAX_CHALLENGES = 10_000` and drops the oldest nonce when it is full. A nonce is
valid for 300 seconds and is removed when it is consumed, so it can be used once.

### 3.2 Verify

`POST /v1/auth/verify` accepts the nonce, a signature over the nonce bytes, the
player's public key, Mojang's signature over that public key, the uuid and the key
expiry. The order matters:

1. The nonce must exist and must not have expired; a single use is enforced by
   removing it before the expensive work.
2. The signature over the nonce is verified with the supplied public key. This
   proves the caller holds the private key.
3. Mojang's signature over the public key is verified for the supplied uuid. This
   proves the key belongs to that player and not to somebody else.
4. The service returns `HMAC-SHA256(secret, "{uuid}:{expires_at}")` as
   `{uuid}:{expires_at}:{signature}`, base64url encoded.

The token carries no state. Any instance that shares the secret can validate it,
which is what makes a restart cheap and a second instance possible later.

### 3.3 Profile

`GET /v1/profile` reads the bearer token, validates the HMAC in constant time,
checks the expiry, and takes the uuid **from the token**. Then:

1. A cache hit returns the slim profile immediately.
2. A miss joins the single flight of the uuid. The first request leads and asks
   Hypixel; any request that arrives while that call is in progress waits for its
   result, success or failure, so a player costs one upstream call however many
   clients poll at once. The leader looks in the cache once more before calling,
   because a flight that ended between the miss and that point has already stored
   the profile.
3. Before the call, the service refuses if the rate limit window is known to be
   exhausted; after the call it records the `RateLimit-*` headers. A `429`
   becomes `RateLimited(wait)`.
4. A transient failure is tried once more, after 200–400 ms of jitter: a `5xx`,
   a connection that did not open, a request that did not go out, a timeout. A
   second look would not change a `4xx`, a used up rate limit or a player without
   a profile, so those are returned at once. The whole upstream call, both
   attempts included, is bounded by the 20 s upstream timeout and the 30 s
   request timeout; a retry after a timeout can therefore be cut short by the
   latter.
5. The response is parsed into `SlimProfile` and cached under the uuid, still
   inside the flight, so the requests waiting for it and the next caller find the
   value.

The raw upstream profile is about a megabyte. Only the mining tree, dungeons and
currencies survive parsing; everything else is dropped before the value reaches
the cache.

## 4. Concurrency and state

- Tokio multi-thread runtime; handlers are async and never block.
- The long-lived shared state (`TtlCache`, `Challenges`, `RateLimits`) uses
  `std::sync::Mutex` for very short critical sections, and none of those guards is
  held across an `.await`.
- A cache miss goes through `singleflight`, which does hold a `tokio::sync::Mutex`
  guard across the upstream call: that is what makes the waiters wait, and it is
  the async mutex because a task has to await on it. The table of flights itself
  is a `std::sync::Mutex<HashMap<uuid, Weak<Flight>>>`; the lock is taken only to
  find or register a flight, never across an await, and the entries are weak, so a
  flight only stays in the table while a request is waiting on it: a finished
  flight removes its own entry, and the next lookup drops the entries that a
  cancelled or timed-out flight left behind. A leader cancelled by the request
  timeout drops its guard, one waiter takes over and the uuid is not wedged. If
  the upstream itself hangs, that happens once per waiter: each takeover starts a
  fresh call until the waiter's own request timeout ends it too, so the calls stay
  sequential and nobody is answered from a flight that is already gone. The
  table is capped at `MAX_IN_FLIGHT_PROFILES` (1 024); past the cap a request
  fetches on its own instead of evicting a flight somebody is waiting on, so the
  bound costs coalescing and never correctness.
- The result, not just the success, is shared: a failure is not retried once per
  waiter. The profile is cached before the flight is released, so a request that
  arrives after the flight is a cache hit.
- The cache, the nonce store and the flight table are the only structures that
  grow with traffic, and all three have hard caps. Memory is a function of the
  caps, not of the request rate.

## 5. Budgets and capacity

| Resource | Limit | Consequence |
| --- | --- | --- |
| Hypixel personal key | 120 requests / 5 minutes conservative, 300 advertised by the key used during the audit | The cache TTL is the throttle. |
| Profile TTL | 300 s → at most 12 requests per player per hour | ~120 players (at 120/5 min) to ~300 players (at 300/5 min) can be served continuously by one key. |
| Cache | 1 024 entries by default, 100 000 hard cap | Tens of megabytes at the default. |
| Nonce store | 10 000 entries | 10 000 × (nonce + timestamp) is a few megabytes. |
| In-flight profile calls | 1 024 uuids, weak entries | One entry per player with a call in progress; a finished flight removes itself and the next lookup drops a cancelled one. |
| Challenge rate | 30 per address per minute, 10 000 addresses tracked | One address cannot flood the endpoint; the map of addresses is capped. |
| Request body | 64 KiB | A verify request is about 1 KiB; the margin is deliberate. |
| Upstream timeout | 20 s | Bounds a stuck Hypixel connection. |
| Request timeout | 30 s | Bounds everything else, including a slow client. |

Measured on the development machine (Windows 11, release build, loopback, one
cached profile). Method: `curl` `time_total` over 100 sequential requests.

| Endpoint | p50 | p95 | max |
| --- | --- | --- | --- |
| `GET /health` | 0.84 ms | 1.15 ms | 11.16 ms |
| `GET /v1/profile` (cache hit, 71.5 KB) | 1.92 ms | 5.42 ms | 12.80 ms |
| `GET /v1/profile` (cache miss) | 1.23 s | – | – |

The release binary is 2.4 MB stripped and links no OpenSSL. The cold answer is
71 541 bytes against roughly 1 MB upstream.

## 6. Failure modes

| Failure | Answer | Notes |
| --- | --- | --- |
| Missing or malformed token | `401 unauthorized` + `WWW-Authenticate: Bearer` | No upstream call. |
| Too many challenges from one address | `429 too_many_requests` + `Retry-After: 60` | Window of 60 s per address. |
| Unknown or reused nonce | `401 unauthorized` | Single use; the error does not say whether it existed. |
| Player has no SkyBlock profile | `404 profile_not_found` | `HypixelError::Missing`. |
| Hypixel rate limit used up | `503 rate_limited` + `Retry-After` | Client should wait, not retry immediately. |
| Hypixel 5xx, network error, bad key | `502 upstream_error` | A `5xx` and a network error are tried once more first; a bad key is not. The cause is logged, not returned. |
| Body over 64 KiB | `413 Payload Too Large` | Rejected before the handler. |
| Request over 30 s | `408 Request Timeout` | Not expected for a healthy upstream. |
| Panic | Process abort | `panic = "abort"`; systemd restarts it. |

## 7. Security model

- **Secret at rest:** only in the environment of the process (`/etc/skyatlaz-service.env`,
  mode 600) or in a gitignored `.env` for development.
- **Transport:** TLS terminates at Caddy. The service binds `127.0.0.1:8080` in
  production, so plain HTTP is never reachable from the network.
- **Identity:** the uuid comes from a token signed with a secret the client never
  sees, after a challenge signed with a key Mojang vouched for.
- **Token theft:** a token is valid up to `TOKEN_TTL_SECONDS` (1 hour) and only
  grants that player's own profile.
- **Abuse surface:** `/health`, `/openapi.json` and `/v1/auth/challenge` are
  unauthenticated. The first two are constant work; the third allocates one nonce
  per call and is bounded in both directions: 30 calls per minute per address,
  10 000 addresses tracked, 10 000 live nonces. An edge limit in
  `deploy/Caddyfile` is still recommended as defence in depth (`docs/ROADMAP.md` P0).
  Because the service sees Caddy's loopback address, the limit keys on the last
  `X-Forwarded-For` entry when the peer is loopback and on the socket address
  otherwise, so a client cannot pick its own bucket.
- **Logs:** no token, key or nonce signature. Request spans carry method, path,
  status and latency.

## 8. Deployment topology

```text
internet ── 443 ──▶ Caddy (TLS, HSTS, rate limit, access log)
                       │ reverse_proxy
                       ▼
                  127.0.0.1:8080  skyatlaz-service (systemd, non-root, hardened unit)
                       │ HTTPS (rustls)
                       ▼
                  api.hypixel.net
```

One ARM VM on Oracle Cloud Always Free (`VM.Standard.A1.Flex`), Ubuntu, systemd
unit from `deploy/skyatlaz-service.service`, Caddy for certificates. The full
runbook is `deploy/README.md`. The binary is self-contained: no OpenSSL, no
runtime data files, no writable directory.

Scaling path, in order, only when needed:

1. One instance is enough for ~120 continuously active players per key.
2. Add a per-IP rate limit and a second key/instance if the limit is the
   bottleneck; tokens already work across instances that share the secret.
3. Add a shared cache (Redis) only if multiple instances make the hit rate
   matter. That is a new dependency and needs an ADR here.

## 9. Decisions

| Decision | Why | Alternative rejected |
| --- | --- | --- |
| In-memory cache with TTL | Kills the upstream rate limit problem with no infrastructure. | Redis from day one: operational cost without need. |
| Stateless HMAC tokens | Survive restarts and scale horizontally with a shared secret. | Server-side sessions: state that has to be stored and expired. |
| Player key-pair auth | The mod already has it; no accounts, no passwords, no email. | OAuth or account system: more surface, more data to protect. |
| Slim profile | The raw profile is ~1 MB; the cache would be hundreds of MB. | Caching the raw JSON. |
| `rustls` | No OpenSSL on the host, one less thing to patch. | `native-tls`. |
| `panic = "abort"`, `opt-level = "z"` | Small binary, fast deploy to a small VM; no unwinding to catch. | `panic = "unwind"`: bigger binary, unused. |
| Oracle Always Free + Caddy | Zero cost and automatic certificates. | PaaS with a card requirement. |
| Single-flight with a bounded weak table and `tokio::sync::Mutex` | Removes the duplicate upstream call without a new dependency, shares failures too, and a cancelled leader cannot wedge a uuid. | `DashMap` of in-flight futures: another dependency and a table still needs its own bound. |

## 10. Observability

Structured logs through `tracing` (one INFO span per request with method, path,
status and latency), `RUST_LOG` to control verbosity, `/health` with the version,
the upstream rate limit state and the number of cached profiles, and `/metrics`
in the Prometheus text format.

`/metrics` publishes request counts per route and status class, a latency
histogram per route, cache hits and misses, profile fetches by outcome and the
retries they needed, and three gauges: the cache size, the request budget of the
key and what is left of it. Two rules keep it safe: every label value comes from
a closed set defined in `src/metrics.rs` (`other` for an unknown path) and never
from a string a caller sent, so the table has a fixed width no client can widen;
and the counters are plain atomics, so a scrape never takes a lock a request
needs. A scrape that overlaps a request may therefore see a slightly older value
for it, which is normal for this kind of endpoint.

The counters are aggregates: no token, no uuid and no player data crosses the
answer, and `/metrics` is meant to be reachable only from the machine that
scrapes it — through Caddy, add a rule that refuses it from the internet if the
service is exposed.

Cache misses count every request the cache could not answer, while the fetch
counters count upstream calls, which single-flight shares. The gap between a
burst of misses and the one fetch that answered it is what coalescing saved;
`/health` and `/metrics` do not expose the flight table because its size is
per-request state, not a number worth alerting on.

Not yet: tracing export and alerting. The roadmap has P2 items for
OpenTelemetry. The practical signals are `journalctl -u skyatlaz-service`, the
Caddy access log, a periodic `curl /health` and a Prometheus scrape of
`/metrics`.
