// SPDX-License-Identifier: GPL-3.0-or-later
// The extension's middle: watches which media links each tab loads, talks to luchd through the native host
// (`luchctl`, see luchctl/src/native.rs) and passes the TV's playback state to the tab that cast.
"use strict";
if (typeof importScripts === "function" && typeof LuchCore === "undefined") importScripts("core.js");
const B = typeof browser !== "undefined" ? browser : chrome;
const HOST = "dev.luch.host";

// ---- media links seen per tab (a blob: player's real source) ----------------------------------------------

const seen = new Map(); // tabId -> [{url, kind, type}], oldest first, at most 40
function remember(tabId, url, type) {
  if (tabId < 0) return;
  const kind = LuchCore.kindOf(url, type);
  if (!kind) return;
  if (kind === "file" && /\.(ts|m4s|aac)(\?|$)/i.test(url)) return; // segments of a playlist, not a file to cast
  const list = seen.get(tabId) || [];
  if (list.some((s) => s.url === url)) return;
  list.push({ url, kind, type: type || "" });
  if (list.length > 40) list.shift();
  seen.set(tabId, list);
}
B.webRequest.onHeadersReceived.addListener(
  (d) => {
    const type = ((d.responseHeaders || []).find((h) => h.name.toLowerCase() === "content-type") || {}).value;
    remember(d.tabId, d.url, type);
  },
  { urls: ["<all_urls>"], types: ["media", "xmlhttprequest", "other"] },
  ["responseHeaders"]
);
B.webNavigation &&
  B.webNavigation.onCommitted.addListener((d) => {
    if (d.frameId === 0) seen.delete(d.tabId);
  });
B.tabs.onRemoved.addListener((tabId) => seen.delete(tabId));

// ---- the native host -----------------------------------------------------------------------------------

let port = null;
let nextId = 1;
const waiting = new Map();
/** receiver id -> {tabId, via, last state, the cast request (for a relayed retry)} */
const casts = new Map();

function connect() {
  if (port) return port;
  port = B.runtime.connectNative(HOST);
  port.onMessage.addListener(onHostMessage);
  port.onDisconnect.addListener(() => {
    const why = (B.runtime.lastError && B.runtime.lastError.message) || "the Luch host went away";
    port = null;
    for (const [, w] of waiting) w.reject(new Error(hostHint(why)));
    waiting.clear();
  });
  return port;
}

function hostHint(why) {
  if (/not found|forbidden|specified native messaging host/i.test(why)) {
    return "Luch is not set up for this browser: run `luchctl browser install` and restart the browser";
  }
  if (/host has exited/i.test(why)) {
    // A Flatpak browser whose wrapper may not leave the sandbox, or luchd not running.
    return "The Luch host stopped at once: is luchd running? A Flatpak browser also needs "
      + "`flatpak override --user --talk-name=org.freedesktop.Flatpak <its id>` (see the extension's README)";
  }
  return why;
}

function ask(op, fields, ms = 15000) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    const t = setTimeout(() => {
      waiting.delete(id);
      reject(new Error("luchd did not answer"));
    }, ms);
    waiting.set(id, {
      resolve: (v) => (clearTimeout(t), resolve(v)),
      reject: (e) => (clearTimeout(t), reject(e)),
    });
    try {
      connect().postMessage(Object.assign({ id, op }, fields));
    } catch (e) {
      waiting.delete(id);
      clearTimeout(t);
      reject(new Error(hostHint(String(e.message || e))));
    }
  });
}

function onHostMessage(m) {
  if (m.event === "state") return onState(m.receiver, m.state);
  const w = waiting.get(m.id);
  if (!w) return;
  waiting.delete(m.id);
  if ("error" in m) w.reject(new Error(m.error));
  else w.resolve(m.ok);
}

function toTab(tabId, msg) {
  if (tabId == null) return;
  B.tabs.sendMessage(tabId, msg).catch(() => {});
}

async function onState(receiver, state) {
  const c = casts.get(receiver);
  if (!c) return;
  c.state = state;
  if (LuchCore.shouldRetryRelayed(c.via, state) && !c.retried) {
    // The TV could not fetch it (a VPN, a login): this computer passes it on instead, from the same moment.
    c.retried = true;
    c.via = "sender";
    toTab(c.tabId, { luch: "note", text: "relay" });
    const options = LuchCore.castOptions(Object.assign({}, c.request, { via: "sender", cookie: await cookieFor(c.request.url) }));
    try {
      await ask("cast", { receiver, items: [c.request.url], options });
    } catch (e) {
      toTab(c.tabId, { luch: "error", text: e.message });
    }
    return;
  }
  toTab(c.tabId, { luch: "state", receiver, state, at: Date.now() });
  if (state.state === "stopped") casts.delete(receiver);
}

async function cookieFor(url) {
  try {
    const all = await B.cookies.getAll({ url });
    return all.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch (e) {
    return "";
  }
}

async function receivers() {
  const list = await ask("receivers", {});
  // Receivers that said they cannot play media (an older Luch) are left out; unknown ones stay.
  return list.filter((r) => !/^web/i.test(r.model || "") && r.media !== false);
}

async function chosenReceiver(list) {
  const { receiver } = await B.storage.local.get("receiver");
  if (list.some((r) => r.id === receiver)) return receiver;
  return list.length === 1 ? list[0].id : null;
}

/** Casts the video a tab describes ({video, page}) to `receiver` (or the chosen one). */
async function castFromTab(tabId, req) {
  const list = await receivers();
  const receiver = req.receiver || (await chosenReceiver(list));
  if (!receiver) return { choose: list };
  const src = LuchCore.pickSource(req.video, seen.get(tabId));
  if (!src) throw new Error("no link to this video (it may be protected)");
  const { relay, trusted = [] } = await B.storage.local.get(["relay", "trusted"]);
  const via = LuchCore.chooseVia(src.url, relay);
  const trust = trusted.includes(receiver);
  const request = Object.assign({}, req.page, { url: src.url, mime: src.mime, via, trusted: trust });
  const cookie = via === "sender" || trust ? await cookieFor(src.url) : "";
  const options = LuchCore.castOptions(Object.assign({}, request, { cookie }));
  casts.set(receiver, { tabId, via, request, state: null, retried: false });
  await ask("cast", { receiver, items: [src.url], options });
  await B.storage.local.set({ receiver });
  const name = (list.find((r) => r.id === receiver) || {}).name || "TV";
  return { receiver, name, via, kind: src.kind };
}

B.runtime.onMessage.addListener((msg, sender, reply) => {
  const tabId = sender.tab ? sender.tab.id : msg.tabId;
  const run = async () => {
    switch (msg.op) {
      case "receivers":
        return Object.assign({ list: await receivers() }, await B.storage.local.get(["receiver", "trusted"]));
      case "choose":
        await B.storage.local.set({ receiver: msg.receiver });
        return {};
      case "trust": {
        const { trusted = [] } = await B.storage.local.get("trusted");
        const next = trusted.filter((id) => id !== msg.receiver);
        if (msg.on) next.push(msg.receiver);
        await B.storage.local.set({ trusted: next });
        return {};
      }
      case "cast":
        return castFromTab(tabId, msg);
      case "cmd":
        await ask("cmd", { receiver: msg.receiver, cmd: msg.cmd });
        return {};
      case "state":
        return { state: await ask("state", { receiver: msg.receiver }) };
      case "stop":
        casts.delete(msg.receiver);
        await ask("stop", { receiver: msg.receiver });
        return {};
      case "active": {
        for (const [receiver, c] of casts) if (c.tabId === tabId) return { receiver, state: c.state };
        return {};
      }
      case "seen":
        return { seen: seen.get(tabId) || [] };
      default:
        throw new Error("unknown op " + msg.op);
    }
  };
  run().then(
    (v) => reply(Object.assign({ ok: true }, v)),
    (e) => reply({ ok: false, error: String(e.message || e) })
  );
  return true; // answered later
});
