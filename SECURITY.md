# Security policy

This service exists to keep one Hypixel developer key out of every player's
computer, so a bug here can cost somebody their key, their account or their
uptime. If you find one, thank you for telling us before telling everybody else.

## Supported versions

Only the latest commit on `main` and the latest tagged release get fixes. The
service is young (0.1.x) and it is designed to be redeployed, not patched in
place, so backports are not offered.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: **Security** tab → **Report a
vulnerability** on
<https://github.com/bgrados31/skyatlaz-service/security/advisories/new>.

- **Do not open a public issue for a vulnerability.**
- **Never paste a key, a session token, a nonce, a signature or a whole
  `Authorization` header** into an issue, a log, a screenshot or this repository.
  If you already did, rotate the Hypixel key at
  [developer.hypixel.net](https://developer.hypixel.net) and generate a new
  `SKYATLAZ_TOKEN_SECRET`; assume both are burned the moment they are public.

A useful report has:

1. the version or commit you tested,
2. how the service was configured (which variables were set, no values),
3. exactly what you sent and what you got back,
4. what you think an attacker gains,
5. a proof of concept if you have one, as small as possible.

This is a hobby project with one maintainer: expect an acknowledgement in a few
days and an honest answer about whether the report is in scope. There is no
bounty and no service level agreement. If you ask for it, you get credit in the
release notes when it is fixed.

## What is in scope

- Anything that lets a caller read a profile that is not its own, or forge,
  extend or replay a session token.
- Anything that puts the Hypixel key, the token secret, a token or a nonce
  signature into a response, a log line or an error message.
- Anything that makes the key leave the process: a url, a header, a body.
- Unbounded memory growth reachable without a token. `GET /v1/auth/challenge` is
  the only endpoint in that category, and it is capped on purpose; a way around
  the cap is a bug.
- A panic reachable from client input. The release profile is `panic = "abort"`,
  so a panic is a process restart.
- Reading or reusing a nonce more than once, or a nonce that never expires.
- A dependency we ship with a known advisory that has a fix.

## What is not in scope

- Running the service without TLS, on a public interface, or exposing `/metrics`
  or `/v1/auth/challenge` to the internet without a rate limit at the edge.
  [`deploy/README.md`](deploy/README.md) says not to, and the Caddyfile answers a
  public `/metrics` with `404`.
- Anyone who already holds the Hypixel key.
- Spending the key's own rate limit. The owner's traffic is not an attack.
- Hypixel's API, Mojang's API, and the client side of the mod.

## How the service defends itself

- **Identity comes from the token and from nothing else.** There is no endpoint
  that takes a uuid from the request.
- **Nonces are single use, expire, and the store has a hard cap.** The table of
  per-address challenge counts is capped too, so the unauthenticated endpoint
  cannot be used to grow memory.
- **Every cache and queue has a capacity.** The profile cache evicts by least
  recent use; it is never a `HashMap` that grows with traffic.
- **Secrets are compared in constant time** (`hmac::Mac::verify_slice`), never
  with `==`.
- **No secret is logged.** Traces carry method, path, status and latency; the
  cause of an upstream failure goes to the log, never into the response body.
- **Nothing is unbounded on the way in**: a 64 KiB body limit, a 20 s upstream
  timeout and a 30 s request timeout.
- **`unsafe_code = "forbid"`**, and the release profile aborts instead of
  unwinding, so a panic cannot leave a half-updated cache behind.
- **Dependencies are watched**: Dependabot weekly and a weekly `cargo audit`
  ([`.github/workflows/audit.yml`](.github/workflows/audit.yml)). A vulnerable
  dependency is a release blocker.

The long version of the design and its failure modes is in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); the invariants that must never be
broken are in [`AGENTS.md`](AGENTS.md).
