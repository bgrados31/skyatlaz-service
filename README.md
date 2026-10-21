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

## Configuration

All configuration comes from the environment.

| Variable                | Required | Default                    | Description                                    |
| ----------------------- | -------- | -------------------------- | ---------------------------------------------- |
| `HYPIXEL_API_KEY`       | yes      | –                          | Your Hypixel developer key.                    |
| `SKYATLAZ_TOKEN_SECRET` | no       | random on every start      | Secret that signs the session tokens.          |
| `BIND`                  | no       | `127.0.0.1:8080`           | Address the server listens on.                 |
| `HYPIXEL_API_BASE`      | no       | `https://api.hypixel.net`  | Base url of the Hypixel API.                   |
| `PROFILE_TTL_SECONDS`   | no       | `300`                      | How long a profile stays cached.               |
| `TOKEN_TTL_SECONDS`     | no       | `3600`                     | How long a session token stays valid.          |

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

## Tests

```powershell
& "$env:USERPROFILE\.cargo\bin\cargo.exe" test
```

## Notes

- The cache keeps one profile per player for `PROFILE_TTL_SECONDS`, so a mod that polls
  every minute does not spend any rate limit.
- The raw Hypixel profile is around a megabyte. Only the mining tree, the dungeon
  statistics and the currencies are kept, the rest is dropped before the answer is cached.
- Never commit the developer key. Keep it in the environment of the process.
