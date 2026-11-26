#!/bin/sh
# Builds the Firefox flavour of the extension into dist/firefox: Chrome's manifest has a service worker, which
# Firefox does not run, and Firefox's has background scripts, which Chrome refuses in Manifest V3.
set -e
cd "$(dirname "$0")"
rm -rf dist/firefox
mkdir -p dist/firefox
cp core.js background.js content.js popup.html popup.js icon-48.png icon-128.png dist/firefox/
cp manifest.firefox.json dist/firefox/manifest.json
echo "$(pwd)/dist/firefox"
