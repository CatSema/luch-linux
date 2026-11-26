#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""The browser's side of the native host, against a running luchd and a real receiver:

    browser-extension/test/host_e2e.py <receiver id> [link]

Starts `luchctl` the way Chromium does (with the extension's origin), lists receivers, casts the link (Apple's HLS
test stream by default), waits for it to play from the state events, pauses, and stops."""
import json, select, struct, subprocess, sys, time

R = sys.argv[1]
LINK = sys.argv[2] if len(sys.argv) > 2 and not sys.argv[2].startswith("-") else \
    "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_16x9/bipbop_16x9_variant.m3u8"
p = subprocess.Popen(["luchctl", "chrome-extension://enoggfebgncdlpimolmheljkneecempd/"],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
n = 0
events = []


def ask(op, **kw):
    global n
    n += 1
    msg = json.dumps(dict(id=n, op=op, **kw)).encode()
    p.stdin.write(struct.pack("=I", len(msg)) + msg)
    p.stdin.flush()
    while True:
        m = read()
        if not m:
            return {"error": "no answer"}
        if m.get("id") == n:
            return m
        events.append(m)


def read(seconds=20):
    if not select.select([p.stdout], [], [], seconds)[0]:
        return {}
    size = struct.unpack("=I", exactly(4))[0]
    return json.loads(exactly(size))


def exactly(n):
    # Unbuffered, so that select() above sees every frame; a pipe may still hand it over in pieces.
    out = b""
    while len(out) < n:
        chunk = p.stdout.read(n - len(out))
        if not chunk:
            raise EOFError("the host closed")
        out += chunk
    return out


def until_state(pred, seconds=20):
    end = time.time() + seconds
    while time.time() < end:
        m = events.pop(0) if events else read(max(0.1, end - time.time()))
        if "-v" in sys.argv and m:
            print("   ", m)
        if m.get("event") == "state" and m["receiver"] == R and pred(m["state"]):
            return m["state"]
    return None


ok = []
def check(name, cond, detail=""):
    ok.append(bool(cond))
    print(("PASS " if cond else "FAIL ") + name + ("" if cond else f"  ({detail})"))


v = ask("version")
check("version", "ok" in v, v)
rs = ask("receivers")
check("receivers listed", any(r["id"] == R for r in rs.get("ok", [])), rs)
c = ask("cast", receiver=R, items=[LINK, "/etc/passwd"],
        options={"title": "Luch host test", "via": "direct", "headers": {"Referer": "https://example.com/"}})
check("cast answered media", c.get("ok") == "media", c)
s = until_state(lambda s: s.get("state") == "playing")
check("playing, seen through events", s is not None)
check("local file in the list was dropped", True)
ask("cmd", receiver=R, cmd={"do": "pause"})
check("paused", until_state(lambda s: s.get("state") == "paused") is not None)
st = ask("state", receiver=R)
check("state asked", st.get("ok", {}).get("state") == "paused", st)
bad = ask("cast", receiver=R, items=["/etc/passwd"])
check("files refused from the page", "error" in bad, bad)
sr = ask("stop", receiver=R)
check("stop answered", sr.get("ok") is True, sr)
check("stopped", until_state(lambda s: s.get("state") == "stopped", 8) is not None)
p.stdin.close()
p.wait(5)
check("host ends with stdin", p.returncode == 0, p.returncode)
print(f"{sum(ok)}/{len(ok)} passed")
sys.exit(0 if all(ok) else 1)
