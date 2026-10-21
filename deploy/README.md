# Deploying on Oracle Cloud Always Free

Target: one `VM.Standard.A1.Flex` instance (ARM, Ubuntu) running the release
binary under systemd, with Caddy in front for TLS. The service listens on
`127.0.0.1:8080`; only Caddy is exposed.

## 0. Before you start

* Oracle asks for a card to verify your identity. Always Free resources are not
  charged, but the card check is not optional.
* Pick a home region near you. **It cannot be changed later.**
* The repository has no remote yet. Push it to GitHub first, or copy the sources
  to the instance with `scp`.

## 1. Create the instance

Console → Compute → Instances → Create instance.

| Field | Value |
| --- | --- |
| Image | Ubuntu 24.04 (or 22.04) |
| Shape | Ampere → `VM.Standard.A1.Flex`, 1 OCPU / 6 GB (up to 2 / 12 in total) |
| Networking | Create a VCN with a public subnet, assign a public IPv4 |
| SSH keys | Upload or paste your public key |

`Out of capacity` is common for A1 in busy regions. Retry with 1 OCPU, try
another availability domain, or fall back to `VM.Standard.E2.1.Micro`, which is
slower but always available and needs a swap file to compile.

## 2. Open the ports (twice)

Oracle blocks traffic in two independent places. Missing either one is the
usual reason "it works locally but not from outside".

**a) OCI console** — Networking → VCN → Security Lists → Default Security List
→ Add ingress rules: source `0.0.0.0/0`, TCP, destination ports `80` and `443`.
(Port 22 is already open.)

**b) The instance itself** — the Ubuntu images ship an iptables chain that ends
in `REJECT`:

```bash
sudo iptables -I INPUT 6 -m state --state NEW -p tcp --dport 80 -j ACCEPT
sudo iptables -I INPUT 6 -m state --state NEW -p tcp --dport 443 -j ACCEPT
sudo netfilter-persistent save
sudo iptables -L INPUT --line-numbers -n
```

## 3. Build

```bash
sudo apt update && sudo apt install -y build-essential git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
git clone <your-repo-url> && cd skyatlaz-service
cargo build --release
```

Do not use the `rustc` from apt: it is older than what `axum 0.8` requires.

## 4. Install

```bash
sudo useradd --system --home /opt/skyatlaz-service \
  --shell /usr/sbin/nologin skyatlaz

sudo install -d -o skyatlaz -g skyatlaz /opt/skyatlaz-service
sudo install -o skyatlaz -g skyatlaz -m 755 \
  target/release/skyatlaz-service /opt/skyatlaz-service/

sudo install -m 600 -o root -g root \
  deploy/skyatlaz-service.env.example /etc/skyatlaz-service.env
sudo nano /etc/skyatlaz-service.env      # fill in the key and the secret
```

Generate the token secret with:

```bash
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
```

```bash
sudo install -m 644 deploy/skyatlaz-service.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now skyatlaz-service
sudo systemctl status skyatlaz-service
curl -s http://127.0.0.1:8080/health
```

The unit fails to start without `HYPIXEL_API_KEY`, so a red status right after
install usually means an empty variable.

## 5. TLS

```bash
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https curl
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
  | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
  | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update && sudo apt install -y caddy
```

Edit `deploy/Caddyfile` and replace `skyatlaz.example.com` with:

* a DNS name you own and pointed at the instance's public IP with an `A` record, or
* `<public-ip-with-dashes>.sslip.io`, for example `203-0-113-10.sslip.io`, which
  needs no domain at all.

```bash
sudo cp deploy/Caddyfile /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

Caddy requests the certificate on the first request, so port 80 has to be open
before this step.

The `Caddyfile` answers the public `GET /metrics` with `404` on purpose: the
counters are for you, not for the internet. To scrape them from the host:

```bash
curl -s http://127.0.0.1:8080/metrics
```

## 6. Verify from outside

```bash
curl -s https://<your-host>/health
curl -s https://<your-host>/v1/auth/challenge
```

`curl -s -o /dev/null -w '%{http_code}\n' https://<your-host>/metrics` should
answer `404`: if it answers `200`, the Caddyfile that is running is not the one
in this repository.

Then point the mod at it: `service_url` in the SkyAtlaz config.

## 7. Before you share the URL

The service bounds its own memory and its own abuse surface:

- at most 10 000 live nonces (`MAX_CHALLENGES`), each one single use;
- at most 30 challenges per minute per address, answered with `429` and
  `Retry-After: 60`;
- at most 10 000 tracked addresses.

Behind Caddy the connections arrive from loopback, so the limit keys on the last
`X-Forwarded-For` entry, which Caddy sets by default. From any other peer the
socket address wins and the header is ignored, so a client cannot pick its own
bucket.

What is left is a distributed flood: thousands of addresses that each stay under
their own 30 per minute. Close that at the edge. Stock Caddy has no rate limit
directive, so build the binary with the `caddy-ratelimit` module
(`xcaddy build --with github.com/mholt/caddy-ratelimit`), add a zone keyed on the
remote host, and check the module's README for the current directive syntax
before editing the `Caddyfile`.

## Maintenance

```bash
sudo journalctl -u skyatlaz-service -f     # logs
sudo systemctl restart skyatlaz-service    # after a new binary
sudo tail -f /var/log/caddy/skyatlaz-service.log
```

To deploy a new version: `git pull`, `cargo build --release`, `install` the
binary again, `systemctl restart`.
