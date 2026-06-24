# Deploying the relay

The relay is a tiny, ciphertext-blind WebSocket pipe. Two devices that can't reach
each other directly (both behind NAT/CGNAT) each dial *out* to it, and it copies
bytes between them. It never sees plaintext: the agent encrypts every frame
end-to-end before it leaves the device, so the relay only ever forwards opaque
ciphertext. Its only job is to be a publicly reachable meeting point.

This directory deploys it on a small Linux box with TLS, using **Caddy** as a
reverse proxy in front of the relay:

```
device  ──wss:// (TLS, :443)──▶  Caddy  ──ws:// (plain, loopback)──▶  relay
        └──────────── end-to-end-encrypted payload, untouched ───────────┘
```

Caddy owns TLS, certificates, and the public hostname; the relay stays an
unchanged `ws://` pipe on loopback. Because the relay sits behind TLS *and*
outside the end-to-end encryption, it sees neither your traffic nor your keys.

## Prerequisites

- A small always-on Linux VPS (~$5/mo is plenty — this needs a reachable address,
  not horsepower).
- A **domain name** with a DNS `A`/`AAAA` record pointing at the box's public IP.
  Caddy needs a real hostname to get a certificate; a bare IP won't do.
- Inbound TCP **80** and **443** open (80 is used once for the ACME certificate
  challenge; 443 carries the `wss://` traffic).

## 1. Build the relay binary

On the box (or build locally for the box's architecture and copy it over):

```sh
cargo build --release --bin relay
sudo install -m 755 target/release/relay /usr/local/bin/nudge-relay
```

The relay shares no code with the agent, so you only need this repo to build it.

## 2. Install Caddy

Use Caddy's official package for your distro — see
<https://caddyserver.com/docs/install>. On Debian/Ubuntu it's an apt repository;
the short version once the repo is added:

```sh
sudo apt update && sudo apt install caddy
```

## 3. Configure Caddy

Copy the `Caddyfile` from this directory to `/etc/caddy/Caddyfile` and replace
`relay.example.com` with your domain:

```sh
sudo cp deploy/Caddyfile /etc/caddy/Caddyfile
sudo $EDITOR /etc/caddy/Caddyfile   # set your hostname
sudo systemctl reload caddy
```

On reload Caddy fetches a Let's Encrypt certificate automatically and starts
serving HTTPS. (Watch progress with `journalctl -u caddy -f`.)

## 4. Run the relay as a service

```sh
sudo cp deploy/relay.service /etc/systemd/system/nudge-relay.service
sudo systemctl daemon-reload
sudo systemctl enable --now nudge-relay
```

Check it's listening on loopback and healthy:

```sh
systemctl status nudge-relay
ss -ltnp | grep 9000        # should show 127.0.0.1:9000
```

## 5. Connect through it

Host a session on your computer and show a pairing QR:

```sh
nudge --daemon --relay wss://relay.example.com --pair
```

Then attach from another device with the pairing code it printed:

```sh
nudge --connect --pair-code 'nudge:...'
```

The pairing code carries the relay address, a one-time rendezvous id, and the
end-to-end encryption key — so anyone you hand it to can join, and no one else
can. Treat it as a secret.

## Notes

- **The relay binds loopback only.** Nothing reaches it except Caddy on the same
  box, so it is never directly exposed to the internet.
- **No state, no logs of content.** The relay holds two connections open and
  copies bytes; it keeps no transcript and could not read one if it tried.
- **Lifting it elsewhere.** The relay is self-contained (`src/bin/relay.rs`); if
  you'd rather run it from its own repository, copy that file and a minimal
  `Cargo.toml` — it pulls in only `tokio`, `tokio-tungstenite`, `clap`, `anyhow`,
  and `futures`.
