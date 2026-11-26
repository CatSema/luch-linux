# Luch for the browser

Sends a video from any web page to a TV with Luch. The TV plays the video itself, in the quality the site offers up
to what its screen can show. The page gets the TV's controls over its own player: play/pause, position, speed,
sound track and subtitles. The TV's remote works too, and both stay in step.

## Setup

1. luchd from this repository is running (`luchctl list` shows the TV).
2. `luchctl browser install` lets the browser reach luchd. It writes a native messaging host manifest for every
   browser it finds, pointing at `luchctl`.
3. Add the extension:
   - **Firefox**: `about:debugging` → *This Firefox* → *Load Temporary Add-on…* → `browser-extension/manifest.json`.
     Then in `about:addons` → Luch → *Permissions*, allow *Access your data for all websites*: Firefox asks for it
     separately, and without it the extension cannot see a page's videos.
   - **Chrome, Chromium, Brave, Edge**: `chrome://extensions` → *Developer mode* → *Load unpacked* →
     `browser-extension/`. The `key` in the manifest keeps the extension's id
     (`enoggfebgncdlpimolmheljkneecempd`), which the host manifest allows.
   - A browser installed as a Flatpak cannot start `luchctl` outside its sandbox. Use the system package, or Firefox.

## Use

Move the pointer over a video: a **To TV** button appears in its corner. The toolbar button picks the TV and sends
the page's biggest video.

How the video gets to the TV:

- **Direct.** The TV fetches the video itself, with the page's address as `Referer`. This needs nothing from this
  computer, which may even go to sleep.
- **Through this computer** (*relay*). luchd fetches the video and passes it on, for links the TV cannot open by
  itself: a company site behind a VPN, a local address, a login. This happens by itself for local addresses, and
  again whenever a direct cast fails. The toolbar popup can force either way.
- **Cookies** (the site's login) never leave this computer, unless you mark a TV as your own in the popup: then they
  go to that TV for direct fetching. Otherwise a link that needs a login is relayed, and the cookies stay here.

## What works

- A `<video>` with its own link: MP4, WebM and the like.
- Players that stream with HLS: the playlist the page loaded is cast, so the TV picks its own quality.
- DASH (`.mpd`) is found, but Android receivers cannot play it yet.
- Sites with DRM (Netflix and the like) cannot be cast this way.
- On YouTube the player hides its links. Opening the TV's own YouTube app instead is planned.

## Tests

`node --test browser-extension/test/core.test.js` checks the decisions (`core.js`).
`browser-extension/test/host_e2e.py <receiver id>` runs the native host the way a browser does, against a running
luchd and a real TV.
