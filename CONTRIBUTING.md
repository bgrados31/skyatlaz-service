# Contributing to skyatlaz-service

Thanks for wanting to help. This is a small service that holds somebody's Hypixel
developer key and answers the profile of the player who authenticates, so the bar
here is less "clever" and more "obviously correct".

By taking part you agree to the [code of conduct](CODE_OF_CONDUCT.md).

## Before you write code

- **Open an issue first** for anything that changes the contract
  (`openapi.json`: a field, a status code, a header), the authentication model,
  the name or the meaning of an environment variable, or the dependency list.
  Those changes affect the mod, which reads the contract.
- Fixes, tests, comments and documentation can go straight to a pull request.
- Security problems do **not** go to the issue tracker: see
  [`SECURITY.md`](SECURITY.md).

## Setting up

You need a Rust toolchain: `rustup` reads `rust-toolchain.toml` and installs the
exact version CI uses, so there is nothing to pin by hand.

```bash
git clone https://github.com/bgrados31/skyatlaz-service.git
cd skyatlaz-service
cp .env.example .env
```

Put a key from [developer.hypixel.net](https://developer.hypixel.net) in `.env` to
run the service. The tests never read it and never touch the network: Hypixel
answers are `serde_json::json!` fixtures, RSA keys are generated in the test, and
the one test that needs an upstream starts a fake one on a loopback port.

```bash
cargo run --release     # then: curl http://127.0.0.1:8080/health
```

## The four gates

A change is not finished until all four pass, on your machine, before you push:

```bash
cargo fmt --all
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

CI runs the same four on Linux and Windows
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)). Report the real output
in the pull request; a claim that a command passed without running it is worse
than a failure.

## Finding your way around

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | Router, layers, handlers, `ApiError`, HTTP tests, startup and shutdown. |
| `src/auth.rs` | Nonce store (bounded), Mojang key check, HMAC session tokens. |
| `src/keys.rs` | DER/base64 decoding and RSA signature verification. |
| `src/hypixel.rs` | Upstream client, rate limit headers, slim profile parsing. |
| `src/ratelimit.rs` | What the key's rate limit window currently allows. |
| `src/cache.rs` | Bounded TTL cache (LRU eviction) of slim profiles. |
| `src/config.rs` | Environment parsing and validation; fails fast on bad values. |
| `src/metrics.rs` | Counters and the Prometheus text rendering. |
| `openapi.json` | The public contract, embedded at compile time. |
| `deploy/` | systemd unit, Caddyfile, environment template, Oracle runbook. |
| `docs/` | Architecture (design, budgets, failure modes) and roadmap. |

[`AGENTS.md`](AGENTS.md) is the rulebook for this repository and it wins over any
general convention you may be used to. Read it before your first change; the
short version is in the next section.

## How a change should look

- **A new behaviour needs a new test, and a bug fix starts with the test that
  reproduces it.** Failure paths count: expired, missing, malformed, oversized,
  unauthorised.
- **The contract and the code change together.** A field, a status code or a
  header goes into `openapi.json` and into a test in the same commit. The mod
  copies that file, so a silent divergence ships a broken mod.
- **Comments explain why.** The code already says what it does.
- **Nothing unbounded.** Every cache, queue and map reachable from a public
  endpoint has a capacity, and the reason is written next to it.
- **No secret reaches a log, an error or a url.** Not the key, not the token, not
  a nonce signature, not a whole `Authorization` header.
- **No `std::sync::Mutex` guard held across an `.await`.** Lock, read or write,
  drop, then await. Clippy will not catch this for you.
- **A new dependency has to answer four questions** in the commit message: can
  std or an existing dependency do it, is it maintained, what is its licence, and
  how much does it add to the binary? Prefer `rustls` over OpenSSL.
- **Errors are honest.** A client learns what it can fix; the state of the key
  and of the upstream account stays in the log.

## Commit messages

`area: what changed and why`, imperative, one concern per commit.

```
cache: bound the map so a burst cannot grow memory
hypixel: retry a transient upstream failure once, with jitter
docs: the counters already exist, the gap is alerting
```

When a commit is about speed, put the measurement in it. Do not mix a toolchain
or dependency bump with a feature.

## Pull requests

The template has the checklist; this is what it is asking for:

- [ ] The four gates pass, and the pull request says what they printed.
- [ ] New behaviour has tests, including the failure path.
- [ ] `openapi.json` and its tests match the handlers.
- [ ] New environment variables appear in `.env.example`,
      `deploy/skyatlaz-service.env.example` and the README table.
- [ ] `README.md` and `docs/ROADMAP.md` describe the current state.
- [ ] No secret, key, token or personal path in the diff.
- [ ] If routing, auth or headers changed: a manual `curl` against a running
      binary confirms it.

Keep a pull request to one concern. A large pull request that does three things
is three pull requests that nobody can review.
