# luch-linux

Linux sender for Luch v0: `luchd` (daemon) + `luchctl` (CLI).

- Announces itself as `_luchsrc._tcp` (TXT `v=0 id=<uuid> name=<host>`; id kept in `~/.config/luch/id`).
- Browses `_luch._tcp` receivers through Avahi's D-Bus API (IPv4 only).
- Control channel: TCP, one JSON line per message (≤ 64 KiB): `hello`/`welcome`, `ping`/`pong` every 10 s,
  30 s of silence closes the channel, `bye`, unknown `t` → `error`.
- Every socket to a receiver is bound (`SO_BINDTODEVICE`) to the interface Avahi saw it on, so a
  full-tunnel VPN default route is never used.
- D-Bus (session): `dev.luch.Luch1` at `/dev/luch/Luch1` — `Receivers() → a(sssssqb)`,
  `Connect(s) → s`, `Disconnect(s)`, signal `ReceiversChanged`, property `SenderId`.

## Run

```sh
cargo build --release
./target/release/luchd &
./target/release/luchctl list
./target/release/luchctl connect "XGIMI MoGo 4 Laser"   # or the id
./target/release/luchctl watch
```

Requires a running `avahi-daemon`, nothing else from the system.

## As a user service

```sh
cargo install --path luchd && cargo install --path luchctl
cp contrib/luchd.service ~/.config/systemd/user/
systemctl --user enable --now luchd
```
