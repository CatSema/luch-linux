// SPDX-License-Identifier: GPL-3.0-or-later
// Each browser loads only its own manifest: Chrome refuses background scripts in Manifest V3, Firefox does not run
// a service worker. Both must list the same files and permissions.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const dir = path.join(__dirname, "..");
const chrome = JSON.parse(fs.readFileSync(path.join(dir, "manifest.json"), "utf8"));
const firefox = JSON.parse(fs.readFileSync(path.join(dir, "manifest.firefox.json"), "utf8"));

test("Chrome: a service worker, no background scripts, the key that fixes its id", () => {
  assert.deepEqual(chrome.background, { service_worker: "background.js" });
  assert.ok(chrome.key && chrome.key.length > 300);
  assert.equal(chrome.browser_specific_settings, undefined);
});

test("Firefox: background scripts with core.js first, and the id the host manifest allows", () => {
  assert.deepEqual(firefox.background, { scripts: ["core.js", "background.js"] });
  assert.equal(firefox.browser_specific_settings.gecko.id, "luch@toxblh.github.io");
});

test("both ask for the same and ship the same scripts", () => {
  assert.deepEqual(chrome.permissions, firefox.permissions);
  assert.deepEqual(chrome.host_permissions, firefox.host_permissions);
  assert.deepEqual(chrome.content_scripts, firefox.content_scripts);
  assert.equal(chrome.version, firefox.version);
  for (const f of ["core.js", "background.js", "content.js", "popup.html", "popup.js", "icon-48.png", "icon-128.png"]) {
    assert.ok(fs.existsSync(path.join(dir, f)), f);
  }
});
