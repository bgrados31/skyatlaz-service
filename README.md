# skyatlaz-service

A small self-hosted service that lets the [skyatlaz](https://github.com/tnozamas/skyatlaz)
mod read the Hypixel profile of the player using it.

The Hypixel developer key lives **only** in this service. The mod never sees it and
never has to ask the player for one. The mod authenticates with its Minecraft profile
key pair, exactly like Skyblocker does.

## Why

The Hypixel API policy states that API keys are not meant to be used by mods:

> API Keys are not intended to be used and shared with 3rd party mods or services,
> we will not grant applications for this purpose.
> — [Hypixel API policy](https://support.hypixel.net/hc/en-us/articles/4416373008788-Information-for-Developers)

A service that serves the profile of the authenticated player only is the same model
Skyblocker uses (`hysky.de`), and it keeps the key on a machine you control.

## The contract

The service publishes its own contract at `/openapi.json` (OpenAPI 3.1). A client is
written against that file, and the mod keeps a copy of it in `docs/api/skyatlaz-service.json`
that `tools/service/sync.ps1` refreshes:

```powershell
cd ..\skyatlaz
.\tools\service\sync.ps1
```

The script prints the endpoints and the fields of the profile, so the mod and the service
can be checked at a glance. The mod also has a test (`ServiceContractTest`) that fails
when the service adds a field the mod does not read yet: a change in the API breaks the
build instead of the game.

## Endpoints

| Method | Path                 | Auth            | Description                              |
| ------ | -------------------- | --------------- | ---------------------------------------- |
| GET    | `/health`            | none            | Liveness probe.                          |
| GET    | `/openapi.json`      | none            | The contract of the service.             |
| GET    | `/v1/auth/challenge` | none            | Hands out a single use nonce.            |
| POST   | `/v1/auth/verify`    | signed nonce    | Verifies the signature, returns a token. |
| GET    | `/v1/profile`        | `Bearer <token>`| Returns the slim profile of the caller.   |

### The authentication in short

1. The mod asks for a challenge and receives a nonce.
2. The mod signs the nonce with the private half of its Minecraft profile key pair
   (`SHA256withRSA`) and sends:
   - `uuid`: the player uuid,
   - `public_key`: the base64 DER public key of the profile key pair,
   - `public_key_signature`: the base64 signature Mojang produced over that key,
   - `expires_at`: the expiry of the profile key in milliseconds,
   - `nonce` and `signature`: the signed nonce.
3. The service checks the signature of the nonce (the caller owns the private key)
   and the signature of Mojang over the public key (the key belongs to that uuid).
4. The service answers with a session token that is valid for one hour.

The profile endpoint only ever serves the profile of the authenticated player.

### Status codes

| Status | Body `error`        | When                                                   |
| ------ | ------------------- | ------------------------------------------------------ |
| `401`  | `unauthorized`      | Missing, malformed or expired token; bad signature.    |
| `404`  | `profile_not_found` | The player has no SkyBlock profile to read.            |
| `404`  | `not_found`         | No route matches the path.                             |
| `413`  | –                   | The body is larger than 64 KiB.                        |
| `429`  | `too_many_requests` | More than 30 challenges from one address in a minute.  |
| `502`  | `upstream_error`    | Hypixel returned an error or could not be reached.     |
| `503`  | `rate_limited`      | The key used up its rate limit; see `Retry-After`.     |

Every error is JSON with `error` and `message`, including the ones axum would
answer by itself. Responses under `/v1` carry `Cache-Control: no-store`.

## Configuration

All configuration comes from the environment.

| Variable                | Required | Default                    | Description                                    |
| ----------------------- | -------- | -------------------------- | ---------------------------------------------- |
| `HYPIXEL_API_KEY`       | yes      | –                          | Your Hypixel developer key.                    |
| `SKYATLAZ_TOKEN_SECRET` | no       | random on every start      | Secret that signs the session tokens (32+ bytes). |
| `BIND`                  | no       | `127.0.0.1:8080`           | Address the server listens on.                 |
| `PORT`                  | no       | –                          | Used when `BIND` is unset; listens on `0.0.0.0`. |
| `HYPIXEL_API_BASE`      | no       | `https://api.hypixel.net`  | Base url of the Hypixel API.                   |
| `PROFILE_TTL_SECONDS`   | no       | `300`                      | How long a profile stays cached.               |
| `PROFILE_CACHE_ENTRIES` | no       | `1024`                     | How many profiles the cache may hold (max 100000). |
| `TOKEN_TTL_SECONDS`     | no       | `3600`                     | How long a session token stays valid.          |

A value the service cannot work with stops it at startup instead of failing later:
a zero ttl, a cache size out of range, a `PORT` that is not a port. The message
names the variable and the value it read.

## Running it locally

```powershell
# PowerShell
$env:HYPIXEL_API_KEY = "your-key"
$env:SKYATLAZ_TOKEN_SECRET = "some-long-random-string"
& "$env:USERPROFILE\.cargo\bin\cargo.exe" run --release
```

Then:

```powershell
curl.exe http://127.0.0.1:8080/health
```

The mod talks to `http://127.0.0.1:8080` by default, see the `service.url` setting.

## Development

The toolchain is pinned in `rust-toolchain.toml`, so `rustup` installs the same
compiler that CI uses on first run.

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" fmt --all
& "$env:USERPROFILE\.cargo\bin\cargo.exe" clippy --all-targets -- -D warnings
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test --all-targets
& "$env:USERPROFILE\.cargo\bin\cargo.exe" build --release
```

CI runs those commands on Linux and Windows (`.github/workflows/ci.yml`). The
tests need no network and no key: the Hypixel answers are fixtures.

- Rules for changing the code: `AGENTS.md`.
- Design, budgets and failure modes: `docs/ARCHITECTURE.md`.
- What is done and what is next: `docs/ROADMAP.md`.
- Deployment on Oracle Cloud: `deploy/README.md`.

## Notes

- The cache holds at most `PROFILE_CACHE_ENTRIES` profiles and evicts the least
  recently used one, so memory is bounded by configuration rather than traffic.
- A profile stays cached for `PROFILE_TTL_SECONDS`: one player polling the mod
  costs at most twelve upstream requests per hour, and one key serves about 120
  continuously active players.
- The raw Hypixel profile is around a megabyte. Only the mining tree, the dungeon
  statistics and the currencies are kept, the rest is dropped before the answer is cached.
- Never commit the developer key. Keep it in the environment of the process.
- `/v1/auth/challenge` allows 30 requests per minute per address. Behind a local
  proxy (the Caddy setup in `deploy/`) the address is the last `X-Forwarded-For`
  entry; from anywhere else the socket address wins and the header is ignored.
- Requests are logged with method, path, status and latency; tokens, keys and
  signatures never reach the logs.
- The process finishes the open requests and exits on SIGTERM, which is what
  `systemctl restart` sends.
