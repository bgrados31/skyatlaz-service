## What this changes

<!-- One concern per pull request. What behaviour is different afterwards? -->

## Why

<!-- The problem, not the patch. Link the issue if there is one. -->

## How it was checked

<!-- The real output, not "it works". Paste the four commands and their result. -->

```
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --release --locked
```

## Checklist

- [ ] New behaviour has tests, and the failure path is covered too.
- [ ] No existing test was weakened or deleted to make this pass.
- [ ] `openapi.json` matches the handlers, with its test, in this same commit.
- [ ] New environment variables are in `.env.example`,
      `deploy/skyatlaz-service.env.example` and the README table.
- [ ] `README.md` and `docs/ROADMAP.md` describe the current state.
- [ ] No secret, key, token or personal path in the diff or in the commit message.
- [ ] Nothing new is unbounded: every cache, queue or map reachable from a public
      endpoint still has a cap.
- [ ] A new dependency answers the four questions in the commit message (std or an
      existing crate, maintained, licence, size).
- [ ] If routing, auth or headers changed, a manual `curl` against a running
      binary confirmed it.
