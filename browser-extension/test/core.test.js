// SPDX-License-Identifier: GPL-3.0-or-later
// node --test browser-extension/test
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const c = require("../core.js");

test("links are told apart by extension and Content-Type", () => {
  assert.equal(c.kindOf("https://cdn.example/v/master.m3u8?t=1"), "hls");
  assert.equal(c.kindOf("https://cdn.example/play", "application/vnd.apple.mpegurl"), "hls");
  assert.equal(c.kindOf("https://cdn.example/m.mpd"), "dash");
  assert.equal(c.kindOf("https://cdn.example/a/film.MP4"), "file");
  assert.equal(c.kindOf("https://cdn.example/stream", "video/webm"), "file");
  assert.equal(c.kindOf("https://cdn.example/app.js", "text/javascript"), null);
  assert.equal(c.kindOf("blob:https://site.example/4f0c"), null);
  assert.equal(c.kindOf("data:video/mp4;base64,AAAA"), null);
});

test("hosts the TV cannot reach by itself", () => {
  for (const h of ["localhost", "intranet", "wiki.corp", "nas.local", "printer.lan", "10.1.2.3", "172.16.0.9",
    "192.168.1.20", "100.64.3.1", "169.254.0.5", "127.0.0.1", "[::1]", "fd12::5", "fe80::1", "media.home.arpa"]) {
    assert.equal(c.isPrivateHost(h), true, h);
  }
  for (const h of ["www.youtube.com", "cdn.example", "93.184.216.34", "172.32.0.1", "100.128.0.1", "2001:db8::1"]) {
    assert.equal(c.isPrivateHost(h), false, h);
  }
});

test("relayed when private, direct otherwise, or as the person chose", () => {
  assert.equal(c.chooseVia("https://video.corp/a.mp4"), "sender");
  assert.equal(c.chooseVia("http://192.168.1.5:8080/a.mp4"), "sender");
  assert.equal(c.chooseVia("https://cdn.example/a.mp4"), "direct");
  assert.equal(c.chooseVia("https://cdn.example/a.mp4", "always"), "sender");
  assert.equal(c.chooseVia("http://nas.local/a.mp4", "never"), "direct");
});

test("a video's own link wins over what the page fetched", () => {
  const v = { currentSrc: "https://cdn.example/a.webm", sources: [{ src: "https://cdn.example/a.webm", type: "video/webm" }] };
  assert.deepEqual(c.pickSource(v, [{ url: "https://cdn.example/x.m3u8", kind: "hls" }]),
    { url: "https://cdn.example/a.webm", kind: "file", mime: "video/webm" });
});

test("a blob: player falls back to the playlist the page loaded, master first", () => {
  const v = { currentSrc: "blob:https://site.example/123", src: "blob:https://site.example/123", sources: [] };
  const seen = [
    { url: "https://cdn.example/v/720p.m3u8", kind: "hls" },
    { url: "https://cdn.example/v/master.m3u8", kind: "hls" },
    { url: "https://cdn.example/v/seg1.ts", kind: "file" },
  ];
  assert.deepEqual(c.pickSource(v, seen), { url: "https://cdn.example/v/master.m3u8", kind: "hls", mime: c.MIME.hls });
  assert.equal(c.pickSource(v, [{ url: "https://cdn.example/d.mpd", kind: "dash" }]).kind, "dash");
  assert.equal(c.pickSource(v, []), null);
});

test("cookies stay on this computer unless it relays or the TV is trusted", () => {
  const base = { title: "Film", currentTime: 61.37, pageUrl: "https://site.example/watch/1", userAgent: "UA",
    cookie: "session=s3cret", playbackRate: 1.5 };
  const direct = c.castOptions({ ...base, via: "direct" });
  assert.equal(direct.headers.Cookie, undefined);
  assert.equal(direct.headers.Referer, "https://site.example/watch/1");
  assert.equal(direct.headers.Origin, "https://site.example");
  assert.equal(direct.start, 61.3);
  assert.equal(direct.rate, 1.5);
  const relayed = c.castOptions({ ...base, via: "sender" });
  assert.equal(relayed.headers.Cookie, "session=s3cret");
  const home = c.castOptions({ ...base, via: "direct", trusted: true });
  assert.equal(home.headers.Cookie, "session=s3cret");
});

test("a failed direct cast is retried through this computer, once", () => {
  assert.equal(c.shouldRetryRelayed("direct", { state: "error" }), true);
  assert.equal(c.shouldRetryRelayed("direct", { code: "bad_link" }), true);
  assert.equal(c.shouldRetryRelayed("sender", { state: "error" }), false);
  assert.equal(c.shouldRetryRelayed("direct", { state: "playing" }), false);
});

test("clock and position between reports", () => {
  assert.equal(c.clock(75), "1:15");
  assert.equal(c.clock(3723), "1:02:03");
  const s = { state: "playing", pos: 10, rate: 2, dur: 12 };
  assert.equal(c.positionNow(s, 1000, 1500), 11);
  assert.equal(c.positionNow(s, 1000, 9000), 12);
  assert.equal(c.positionNow({ state: "paused", pos: 5 }, 0, 99999), 5);
});

test("the quality a link names, and its siblings", () => {
  assert.equal(c.qualityOf("https://cdn.example/v/720/1234.mp4"), 720);
  assert.equal(c.qualityOf("https://cdn.example/v/1234_480p.mp4"), 480);
  assert.equal(c.qualityOf("https://cdn.example/v/1234.mp4?q=1080&t=1"), 1080);
  assert.equal(c.qualityOf("https://cdn.example/v/episode-7201.mp4"), 0);
  assert.equal(c.qualityOf("https://cdn.example/v/1234.mp4"), 0);
  const v = c.variantLinks("https://cdn.example/v/360/1234.mp4?token=a");
  assert.ok(v.some((x) => x.url === "https://cdn.example/v/720/1234.mp4?token=a" && x.h === 720));
  assert.ok(v.some((x) => x.url === "https://cdn.example/v/1080/1234.mp4?token=a"));
  assert.ok(!v.some((x) => x.h === 360));
  const p = c.variantLinks("https://cdn.example/v/1234_360p.mp4");
  assert.ok(p.some((x) => x.url === "https://cdn.example/v/1234_720p.mp4"));
  assert.deepEqual(c.variantLinks("https://cdn.example/v/1234.mp4"), []);
});

test("sources go best first with their sizes", () => {
  const s = c.sourcesOf("https://cdn.example/360/a.mp4", "video/mp4",
    [{ url: "https://cdn.example/720/a.mp4", h: 720 }, { url: "https://cdn.example/480/a.mp4", h: 480 }]);
  assert.deepEqual(s.map((x) => x.h), [720, 480, 360]);
  assert.equal(s[0].w, 1280);
  assert.equal(s[0].mime, "video/mp4");
});

test("subtitle files are told apart", () => {
  assert.ok(c.isSubtitle("https://x.example/subs/ep1.ass"));
  assert.ok(c.isSubtitle("https://x.example/s?id=1", "text/vtt; charset=utf-8"));
  assert.ok(!c.isSubtitle("https://x.example/a.mp4"));
});
