# D-Bus API — `dev.luch.Luch1`

[Русская версия](dbus.ru.md)

`luchd` owns the well-known name **`dev.luch.Luch1`** on the **session bus** and serves one object,
**`/dev/luch/Luch1`**, with one interface, **`dev.luch.Luch1`**. Everything the CLI (`luchctl`) and the
GNOME extension do goes through it, so any other front-end can do the same.

```sh
gdbus introspect --session --dest dev.luch.Luch1 --object-path /dev/luch/Luch1
```

## Receivers

A receiver is a Luch device on the LAN, found through mDNS (`_luch._tcp`, IPv4 only, via Avahi).
Receivers seen only on point-to-point links (VPN tunnels) are ignored. If one appears on several
interfaces, the one on the main LAN interface wins. Every socket to a receiver is bound to the
interface it was discovered on, so a full-tunnel VPN never carries the stream.

## Methods

### `Receivers() → a(sssssqbb)`

Returns every known receiver as a tuple:

| # | Type | Meaning |
|---|------|---------|
| 0 | `s` | receiver id (stable UUID from its TXT record) |
| 1 | `s` | human name, e.g. `XGIMI MoGo 4 Laser` |
| 2 | `s` | model |
| 3 | `s` | IPv4 address |
| 4 | `s` | network interface it was found on |
| 5 | `q` | control port |
| 6 | `b` | a control channel is open |
| 7 | `b` | mirroring is running |

### `Connect(s id) → s`

Opens the control channel (TCP, JSON lines) and keeps it alive with a ping every second.
Returns the receiver's `welcome` message as a JSON string: decoders, maximum size, hardware
passport (`hw`) and decoder benchmark (`bench`). `Mirror` connects by itself, so calling this is
optional.

Errors: `org.freedesktop.DBus.Error.InvalidArgs` for an unknown id, `org.freedesktop.DBus.Error.Failed`
if already connected or the handshake fails.

### `Disconnect(s id)`

Sends `bye` and closes the control channel. Running mirroring stops with it.

### `Mirror(s id, s source)`

Starts mirroring to the receiver, connecting first if needed. Returns as soon as the session is set
up. `source` selects how the screen is captured:

| `source` | Capture | Latency | Notes |
|----------|---------|---------|-------|
| `fast` | KMS scanout via the `luch-kms` helper, EGL→NV12, VA-API encode | lowest (≈5–6 ms on the sender) | whole primary monitor; `luch-kms` needs `CAP_SYS_ADMIN` |
| `monitor` | screencast portal (PipeWire, DMA-BUF) | ≈15–25 ms on the sender | GNOME asks which screen once, then remembers it |
| `window` | screencast portal | as `monitor` | a single window |
| `virtual` | screencast portal, virtual monitor | as `monitor` | extends the desktop with a new display |

The codec follows the receiver's decoder benchmark (`welcome.bench.prefer`): HEVC when it is
faster there, otherwise H.264. Audio (Opus, 5 ms frames) is captured from the default output's
monitor in every mode.

Errors: `InvalidArgs` for a bad `source` or unknown id, `Failed` if already mirroring.

### `StopMirror(s id)`

Stops mirroring and tells the receiver (`stop`). The control channel stays open.

### `Stats(s id) → s`

Live numbers as JSON, refreshed every second while mirroring, `{}` otherwise:

```json
{
  "est_ms": 13.5,
  "sender":   {"rtt_ms": 3.7, "host_ms": 5.6, "fps": 45, "mbps": 2.1, "encoder": "hevc_vaapi", "res": "1920x1080"},
  "receiver": {"decode_ms": 6.0, "fps": 47, "lost": 0, "dropped": 3, "decoder": "OMX.MS.HEVC.Decoder"}
}
```

- `rtt_ms` — round trip of the control-channel ping.
- `host_ms` — capture → encoded packet on the sender (scanout flip time in `fast` mode).
- `decode_ms` — the receiver's decoder time, as it reports it.
- `est_ms` — `host_ms + rtt_ms / 2 + decode_ms`; the display's own latency is not included.

The same sender numbers are sent to the receiver (`sender_stats`) for its on-screen overlay.

## Signals

### `ReceiversChanged()`

Emitted when a receiver appears or disappears, or a connection or mirroring session starts or ends.
Call `Receivers()` again to get the new state.

## Properties

### `SenderId` (`s`, read)

This machine's id (UUID kept in `~/.config/luch/id`). It is also announced as `_luchsrc._tcp`, so
receivers can show which senders are nearby.

## Examples

```sh
# list
gdbus call --session --dest dev.luch.Luch1 --object-path /dev/luch/Luch1 --method dev.luch.Luch1.Receivers

# mirror the whole screen with the lowest latency, then stop
gdbus call --session --dest dev.luch.Luch1 --object-path /dev/luch/Luch1 --method dev.luch.Luch1.Mirror "$ID" fast
gdbus call --session --dest dev.luch.Luch1 --object-path /dev/luch/Luch1 --method dev.luch.Luch1.StopMirror "$ID"

# watch for changes
gdbus monitor --session --dest dev.luch.Luch1 --object-path /dev/luch/Luch1
```

From GJS (as the GNOME extension does):

```js
const proxy = Gio.DBusProxy.new_for_bus_sync(Gio.BusType.SESSION, Gio.DBusProxyFlags.NONE, null,
    'dev.luch.Luch1', '/dev/luch/Luch1', 'dev.luch.Luch1', null);
const [rows] = proxy.call_sync('Receivers', null, Gio.DBusCallFlags.NONE, -1, null).deepUnpack();
proxy.call('Mirror', new GLib.Variant('(ss)', [rows[0][0], 'fast']), Gio.DBusCallFlags.NONE, -1, null, null);
proxy.connectSignal('ReceiversChanged', () => { /* refresh */ });
```

## Files and environment

| Path / variable | Purpose |
|-----------------|---------|
| `~/.config/luch/id` | sender id |
| `~/.config/luch/restore-token` | screencast portal restore token (no picker after the first time) |
| `LUCH_KMS_SOCKET` | a privileged `luch-kms --serve SOCKET [--uid N]` that `luchd` uses for the `fast` mode instead of running `luch-kms` itself (default `$XDG_RUNTIME_DIR/luch/kms.sock`, used when it exists): for a `luchd` in a sandbox (Flatpak), where file capabilities do not work. The socket is owned by `--uid`, mode 0600 |
| `LUCH_KMS_CARD`, `LUCH_KMS_RENDER` | DRM card and render node for `luch-kms` (default `/dev/dri/card1`, `/dev/dri/renderD128`) |
| `LUCH_PROBE=1` | per-stage latency probes in the `luchd` log |
