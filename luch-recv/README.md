# luch-recv

A Luch receiver for Linux: it announces itself on the LAN like a TV does (`_luch._tcp`), takes a sender's screen (a phone
with the Android sender, another computer running `luchd`) and shows it in a window of its own. The window title is
`luch-recv`; a compositor can float it.

    luch-recv            # runs until stopped; senders list "<host> · Luch"

What it does today: the control channel (hello/welcome, ping, start/ready, keyframe, stats, stop) and H.264 or HEVC video
over UDP, reassembled as PROTOCOL.md section 4 says (a loss drops frames until the next IDR, which it asks for), decoded
by GStreamer (hardware when the system has it) and shown with `autovideosink`.

Not yet: sound (the audio port is opened and ignored), pairing, binding sockets to the interface the sender was found on
(section 2), a window that follows the stream's proportions.

## A floating window in niri

```kdl
window-rule {
    match title="^luch-recv$"
    open-floating true
    default-column-width { fixed 420; }
}
```

## Trying it without a sender

`LUCH_RECV_DEBUG=1 luch-recv` prints every datagram. A short script that plays the sender (hello, start, the video as
24-byte-header datagrams) is enough to see the picture; the test stream is `ffmpeg -f lavfi -i testsrc ... -x264-params aud=1`.
