//! Screen mirroring: xdg-desktop-portal ScreenCast -> PipeWire -> VA-API H.264 + Opus -> Luch v1 UDP.
//!
//! Latency rules borrowed from Sunshine/Moonlight: newest frame only, no B-frames, one reference,
//! one-frame CPB, IDR only on request, packets sent straight from the encoder thread.

use std::{
    net::{Ipv4Addr, SocketAddrV4, UdpSocket},
    os::fd::{AsRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use ashpd::desktop::{
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions},
    PersistMode,
};
use futures_util::StreamExt;
use gstreamer::{self as gst, prelude::*};
use gstreamer_app as gst_app;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1080;
pub const FPS: u32 = 60;
pub const VIDEO_KBPS: u32 = 20_000;
pub const AUDIO_BPS: u32 = 128_000;
pub const AUDIO_FRAME_MS: u32 = 5;

const HEADER: usize = 24;
const SHARD: usize = 1376; // HEADER + SHARD = 1400, safe under a 1500 MTU

pub const KIND_VIDEO: u8 = 1;
pub const KIND_AUDIO: u8 = 2;
pub const F_IDR: u8 = 1;
pub const F_CONFIG: u8 = 2;

/// Latest numbers for one mirroring session, shared with D-Bus `Stats`.
pub type Stats = Arc<Mutex<Value>>;

/// Where and what to mirror.
pub struct Target {
    pub name: String,
    pub address: Ipv4Addr,
    pub ifname: String,
    pub source: SourceType,
    /// Full screen straight from KMS scanout via the privileged luch-kms helper (lowest latency).
    pub kms: bool,
}

/// Messages to the control-channel task.
pub enum Cmd {
    Bye,
    Send(Value),
}

/// Luch v1 datagram header (24 bytes, big-endian).
pub fn header(kind: u8, flags: u8, idx: u16, shards: u16, seq: u32, pts_us: u32, len: u32, host_lat: u16) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    h[0] = kind;
    h[1] = flags;
    h[2..4].copy_from_slice(&idx.to_be_bytes());
    h[4..6].copy_from_slice(&shards.to_be_bytes());
    // 6..8 parity_shards = 0 (no FEC yet)
    h[8..12].copy_from_slice(&seq.to_be_bytes());
    h[12..16].copy_from_slice(&pts_us.to_be_bytes());
    h[16..20].copy_from_slice(&len.to_be_bytes());
    h[20..22].copy_from_slice(&host_lat.to_be_bytes());
    h
}

/// One UDP socket per media kind, pinned to the LAN interface.
struct Tx {
    sock: UdpSocket,
    kind: u8,
    seq: AtomicU32,
    warned: AtomicBool,
    bytes: AtomicU64,
    lat_sum: AtomicU64, // host latency, 0.1 ms units, summed over frames
}

/// Shards per burst before a short pause, so a big IDR doesn't overflow the Wi-Fi queue.
const BURST: usize = 16;
const BURST_GAP: Duration = Duration::from_micros(250);

impl Tx {
    fn open(kind: u8, ifname: &str, to: SocketAddrV4, dscp: u32) -> std::io::Result<Self> {
        use socket2::{Domain, Protocol, Socket, Type};
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        s.bind_device(Some(ifname.as_bytes()))?;
        s.set_tos_v4(dscp << 2)?; // CS5 video / CS6 audio -> Wi-Fi WMM video/voice queues
        s.set_send_buffer_size(1 << 20)?;
        s.connect(&to.into())?;
        Ok(Tx { sock: s.into(), kind, seq: AtomicU32::new(0), warned: AtomicBool::new(false), bytes: AtomicU64::new(0), lat_sum: AtomicU64::new(0) })
    }

    // ponytail: one send() per shard; sendmmsg/UDP_SEGMENT if the syscall count ever shows up in profiles.
    fn send_frame(&self, data: &[u8], flags: u8, pts_us: u64, host_lat: u16) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        self.lat_sum.fetch_add(host_lat as u64, Ordering::Relaxed);
        if seq == 0 {
            eprintln!("[luchd] first {} frame: {} bytes", if self.kind == KIND_VIDEO { "video" } else { "audio" }, data.len());
        }
        let shards = data.len().div_ceil(SHARD).max(1);
        let mut pkt = Vec::with_capacity(HEADER + SHARD);
        for (i, chunk) in data.chunks(SHARD).enumerate() {
            if i > 0 && i % BURST == 0 {
                std::thread::sleep(BURST_GAP); // ponytail: fixed pacing, ~3 ms for a 200 KB IDR
            }
            pkt.clear();
            pkt.extend_from_slice(&header(self.kind, flags, i as u16, shards as u16, seq, pts_us as u32, data.len() as u32, host_lat));
            pkt.extend_from_slice(chunk); // last shard is not padded; receiver uses frame_len
            // A lost datagram is recovered by the receiver's keyframe request; just report the first failure.
            if let Err(e) = self.sock.send(&pkt) {
                if !self.warned.swap(true, Ordering::Relaxed) {
                    eprintln!("[luchd] udp send (kind {}): {e}", self.kind);
                }
            }
        }
    }
}

/// 19-byte OpusHead (RFC 7845) for 48 kHz stereo: MediaCodec CSD-0.
pub fn opus_head() -> [u8; 19] {
    let mut h = [0u8; 19];
    h[..8].copy_from_slice(b"OpusHead");
    h[8] = 1; // version
    h[9] = 2; // channels
    h[10..12].copy_from_slice(&312u16.to_le_bytes()); // pre-skip (opusenc lookahead)
    h[12..16].copy_from_slice(&48_000u32.to_le_bytes());
    h
}

async fn portal(source: SourceType) -> Result<(Screencast, ashpd::desktop::Session<Screencast>, OwnedFd, u32), String> {
    let e = |e: ashpd::Error| format!("portal: {e}");
    let token_path = crate::config_dir().map(|d| d.join("restore-token")).map_err(|e| e.to_string())?;
    let token = std::fs::read_to_string(&token_path).ok().filter(|t| !t.trim().is_empty());
    let sc = Screencast::new().await.map_err(e)?;
    let session = sc.create_session(Default::default()).await.map_err(e)?;
    let opts = SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::EMBEDDED)
        .set_sources(source)
        .set_multiple(false)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(token.as_deref().map(str::trim));
    sc.select_sources(&session, opts).await.map_err(e)?;
    let streams = sc.start(&session, None, StartCastOptions::default()).await.map_err(e)?.response().map_err(e)?;
    if let Some(t) = streams.restore_token() {
        let _ = std::fs::write(&token_path, t);
    }
    let stream = streams.streams().first().ok_or("portal: no stream selected")?;
    let node = stream.pipe_wire_node_id();
    let fd = sc.open_pipe_wire_remote(&session, Default::default()).await.map_err(e)?;
    Ok((sc, session, fd, node))
}

/// Video from the portal (`Some((fd, node))`) plus audio, or audio only when luch-kms does video.
fn pipeline(portal: Option<(i32, u32)>) -> Result<gst::Pipeline, String> {
    let keepalive = 2000 / FPS; // resend the last frame at fps/2 when the screen is static
    // ponytail: the Intel tiled NV12 modifier is what vapostproc offers here; make it a negotiated
    // list when other GPUs matter.
    let scale = std::env::var("LUCH_SCALE").unwrap_or_else(|_| "fast".into());
    let drm = std::env::var("LUCH_DRM_FORMAT").unwrap_or_else(|_| "NV12:0x0100000000000009".into());
    // Zero copy end to end: compositor DMA-BUF -> vapostproc (GPU scale + NV12) -> DMA-BUF -> h264_vaapi.
    let video = portal.map(|(fd, node)| format!(
        "pipewiresrc fd={fd} path={node} do-timestamp=true always-copy=false keepalive-time={keepalive} \
           ! video/x-raw(memory:DMABuf) ! queue max-size-buffers=1 max-size-bytes=0 max-size-time=0 leaky=downstream \
           ! vapostproc scale-method={scale} \
           ! video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={drm},width={WIDTH},height={HEIGHT} \
           ! appsink name=v sync=false max-buffers=1 drop=true "
    )).unwrap_or_default();
    let desc = format!(
        "{video} pulsesrc device=@DEFAULT_MONITOR@ do-timestamp=true buffer-time=10000 latency-time={lat} \
           ! audioconvert ! audioresample ! audio/x-raw,rate=48000,channels=2 \
           ! opusenc audio-type=restricted-lowdelay frame-size={AUDIO_FRAME_MS} bitrate={AUDIO_BPS} bitrate-type=cbr \
           ! appsink name=a sync=false",
        lat = AUDIO_FRAME_MS * 1000,
    );
    gst::parse::launch(&desc)
        .map_err(|e| format!("pipeline: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "pipeline: not a pipeline".to_string())
}

/// Microseconds of pipeline running time for `pts`, and capture->now latency in 0.1 ms.
fn timing(sink: &gst_app::AppSink, pts: Option<gst::ClockTime>) -> (u64, u16) {
    // GstVideoEncoder shifts output PTS by 1000 h so DTS can't go negative; undo it so video and
    // audio share the pipeline running-time base again.
    const ENC_OFFSET: gst::ClockTime = gst::ClockTime::from_seconds(1000 * 3600);
    let pts = pts.unwrap_or(gst::ClockTime::ZERO);
    let pts = if pts >= ENC_OFFSET { pts - ENC_OFFSET } else { pts };
    let now = sink.current_running_time().unwrap_or(pts);
    let lat = now.saturating_sub(pts).useconds() / 100;
    (pts.useconds(), lat.min(u16::MAX as u64) as u16)
}

/// fd, modifier, plane offsets and pitches of an NV12 DMA-BUF sample.
fn dmabuf_of(sample: &gst::Sample) -> Result<crate::enc::Nv12Dmabuf, String> {
    let buf = sample.buffer().ok_or("no buffer")?;
    let caps = sample.caps().ok_or("no caps")?;
    let drm = caps.structure(0).and_then(|s| s.get::<String>("drm-format").ok()).ok_or("no drm-format")?;
    let (fourcc, modifier) = drm.split_once(':').unwrap_or((&drm, "0x0"));
    if fourcc != "NV12" {
        return Err(format!("expected NV12, got {drm}"));
    }
    let modifier = u64::from_str_radix(modifier.trim_start_matches("0x"), 16).map_err(|e| e.to_string())?;
    if buf.n_memory() != 1 {
        return Err(format!("{} memories, expected 1", buf.n_memory()));
    }
    let mem = buf.peek_memory(0);
    let dmabuf = mem.downcast_memory_ref::<gstreamer_allocators::DmaBufMemory>().ok_or("not a DMA-BUF")?;
    let meta = buf.meta::<gstreamer_video::VideoMeta>().ok_or("no video meta")?;
    let (off, stride) = (meta.offset(), meta.stride());
    Ok(crate::enc::Nv12Dmabuf {
        fd: dmabuf.fd(),
        modifier,
        offsets: [off[0] as u32, off[1] as u32],
        pitches: [stride[0] as u32, stride[1] as u32],
    })
}

fn render_node() -> String {
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| p.contains("renderD"))
        .collect();
    nodes.sort();
    nodes.into_iter().next().unwrap_or_else(|| "/dev/dri/renderD128".into())
}

/// Hooks both appsinks to the UDP senders; returns the "force an IDR" flag.
/// Hooks the appsinks to the UDP senders; returns the "force an IDR" flag and the "resend OpusHead"
/// flag (set on every video IDR). Without a "v" sink only audio is wired.
fn wire(p: &gst::Pipeline, video: Arc<Tx>, audio: Arc<Tx>) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
    let need_head = Arc::new(AtomicBool::new(true));
    let want_idr = Arc::new(AtomicBool::new(false));
    let encoder: Arc<Mutex<Option<crate::enc::Encoder>>> = Arc::default();
    let warned = AtomicBool::new(false);
    let dbg = Mutex::new((0f64, 0f64, 0u32));
    let asink = p.by_name("a").unwrap().downcast::<gst_app::AppSink>().unwrap();
    let nh_audio = need_head.clone();

    let nh = need_head.clone();
    let wi = want_idr.clone();
    if let Some(vsink) = p.by_name("v").and_then(|v| v.downcast::<gst_app::AppSink>().ok()) {
    vsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            // vapostproc only hands out DMA-BUFs to a peer that understands VideoMeta (plane offsets/strides).
            .propose_allocation(|_, query| {
                query.add_allocation_meta::<gstreamer_video::VideoMeta>(None);
                true
            })
            .new_sample(move |s| {
                // Runs on the capture thread: encode and send right here, no queue in between.
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let report = |e: String| {
                    if !warned.swap(true, Ordering::Relaxed) {
                        eprintln!("[luchd] video: {e}");
                    }
                };
                let frame = match dmabuf_of(&sample) {
                    Ok(f) => f,
                    Err(e) => {
                        report(e);
                        return Ok(gst::FlowSuccess::Ok);
                    }
                };
                let pts_rt = sample.buffer().and_then(|b| b.pts());
                let at_sink = timing(s, pts_rt).1;
                let mut enc = encoder.lock().unwrap();
                if enc.is_none() {
                    match crate::enc::Encoder::open(&render_node(), WIDTH, HEIGHT, FPS, VIDEO_KBPS) {
                        Ok(e) => *enc = Some(e),
                        Err(e) => {
                            report(e);
                            return Err(gst::FlowError::Error);
                        }
                    }
                }
                let force = wi.swap(false, Ordering::Relaxed);
                let (pts, _) = timing(s, pts_rt);
                let r = enc.as_mut().unwrap().encode(&frame, pts as i64, force, |bytes, idr| {
                    if idr {
                        nh.store(true, Ordering::Relaxed); // receiver may have (re)started its audio decoder
                    }
                    let (_, lat) = timing(s, pts_rt); // capture -> encoded, measured after encode
                    video.send_frame(bytes, if idr { F_IDR } else { 0 }, pts, lat);
                });
                if let Err(e) = r {
                    report(e);
                }
                if std::env::var_os("LUCH_PROBE").is_some() {
                    let mut a = dbg.lock().unwrap();
                    a.0 += at_sink as f64 / 10.0;
                    a.1 += timing(s, pts_rt).1 as f64 / 10.0;
                    a.2 += 1;
                    if a.2 == 60 {
                        eprintln!("[probe] appsink entry {:.1} ms, after encode+send {:.1} ms", a.0 / 60.0, a.1 / 60.0);
                        *a = (0.0, 0.0, 0);
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    }
    asink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buf = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                let (pts, lat) = timing(s, buf.pts());
                if nh_audio.swap(false, Ordering::Relaxed) {
                    audio.send_frame(&opus_head(), F_CONFIG, pts, 0);
                }
                audio.send_frame(&map, 0, pts, lat);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    (want_idr, need_head)
}

/// Lowest-latency video: spawns luch-kms (KMS scanout -> EGL NV12 -> h264_vaapi) and forwards its
/// packets as they come. Returns the child; write b"k" to its stdin to force an IDR.
fn spawn_kms(video: Arc<Tx>, need_head: Arc<AtomicBool>) -> Result<std::process::Child, String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("luch-kms")));
    let exe = exe.filter(|p| p.exists()).unwrap_or_else(|| "luch-kms".into());
    let mut child = Command::new(&exe)
        .args(["--size", &format!("{WIDTH}x{HEIGHT}"), "--fps", &FPS.to_string(), "--kbps", &VIDEO_KBPS.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", exe.display()))?;
    let mut out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut hdr = [0u8; 13];
        let mut buf = Vec::new();
        let mut base: Option<u64> = None;
        while out.read_exact(&mut hdr).is_ok() {
            let len = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as usize;
            let idr = hdr[4] & 1 != 0;
            let cap = u64::from_be_bytes(hdr[5..13].try_into().unwrap());
            buf.resize(len, 0);
            if out.read_exact(&mut buf).is_err() {
                break;
            }
            let now = mono_us();
            let base = *base.get_or_insert(cap);
            let lat = (now.saturating_sub(cap) / 100).min(u16::MAX as u64) as u16; // scanout flip -> encoded
            if idr {
                need_head.store(true, Ordering::Relaxed);
            }
            video.send_frame(&buf, if idr { F_IDR } else { 0 }, cap - base, lat);
        }
    });
    Ok(child)
}

fn mono_us() -> u64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid out-pointer for clock_gettime.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec as u64 * 1_000_000 + t.tv_nsec as u64 / 1000
}

/// Debug: average (running time - capture pts) at the src pad of each video element, printed every second.
fn probe_stages(p: &gst::Pipeline) {
    const ENC_OFFSET: gst::ClockTime = gst::ClockTime::from_seconds(1000 * 3600);
    let mut it = p.iterate_recurse();
    while let Ok(Some(el)) = it.next() {
        let name = el.name().to_string();
        if !["pipewiresrc", "queue", "capsfilter", "vapostproc", "vah264enc", "h264parse"].iter().any(|k| name.starts_with(k)) {
            continue;
        }
        let Some(pad) = el.static_pad("src") else { continue };
        let acc = std::sync::Mutex::new((0f64, 0u32, std::time::Instant::now()));
        let el2 = el.clone();
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buf) = info.buffer() {
                if let (Some(pts), Some(now)) = (buf.pts(), el2.current_running_time()) {
                    let pts = if pts >= ENC_OFFSET { pts - ENC_OFFSET } else { pts };
                    let mut a = acc.lock().unwrap();
                    a.0 += now.saturating_sub(pts).nseconds() as f64 / 1e6;
                    a.1 += 1;
                    if a.2.elapsed().as_secs() >= 1 {
                        eprintln!("[probe] {:<14} {:6.1} ms  ({} bufs)", el2.name(), a.0 / a.1 as f64, a.1);
                        *a = (0.0, 0, std::time::Instant::now());
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Runs one mirroring session until stopped (by us, the receiver, or an error).
pub async fn run(
    t: Target,
    cmd: mpsc::Sender<Cmd>,
    mut events: broadcast::Receiver<Value>,
    mut stop: mpsc::Receiver<()>,
    stats: Stats,
) -> Result<&'static str, String> {
    let portal = if t.kms { None } else { Some(portal(t.source).await?) };

    let start = json!({"t": "start", "mode": "mirror",
        "video": {"codec": "video/avc", "width": WIDTH, "height": HEIGHT, "fps": FPS},
        "audio": {"codec": "audio/opus", "rate": 48000, "channels": 2, "frame_ms": AUDIO_FRAME_MS}});
    cmd.send(Cmd::Send(start)).await.map_err(|_| "control channel closed")?;
    let ready = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(v) if v["t"] == "ready" => return Ok(v),
                Ok(v) if v["t"] == "error" || v["t"] == "stop" => return Err(format!("receiver: {v}")),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return Err("control channel closed".to_string()),
            }
        }
    })
    .await
    .map_err(|_| "receiver did not answer start".to_string())??;

    let port = |k: &str| ready[k].as_u64().filter(|p| *p > 0 && *p < 65536).map(|p| p as u16);
    let (vp, ap) = port("video_port").zip(port("audio_port")).ok_or(format!("bad ready: {ready}"))?;
    let io = |e: std::io::Error| format!("udp: {e}");
    let video = Arc::new(Tx::open(KIND_VIDEO, &t.ifname, SocketAddrV4::new(t.address, vp), 40).map_err(io)?);
    let audio = Arc::new(Tx::open(KIND_AUDIO, &t.ifname, SocketAddrV4::new(t.address, ap), 48).map_err(io)?);

    let p = pipeline(portal.as_ref().map(|(_, _, fd, node)| (fd.as_raw_fd(), *node)))?;
    let (want_idr, need_head) = wire(&p, video.clone(), audio);
    let mut kms = if t.kms { Some(spawn_kms(video.clone(), need_head)?) } else { None };
    if std::env::var_os("LUCH_PROBE").is_some() {
        probe_stages(&p);
    }
    if let Err(e) = p.set_state(gst::State::Playing) {
        let detail = p.bus().and_then(|b| b.timed_pop_filtered(gst::ClockTime::from_mseconds(500), &[gst::MessageType::Error]));
        let _ = p.set_state(gst::State::Null);
        let _ = cmd.send(Cmd::Send(json!({"t": "stop"}))).await;
        return Err(match detail.as_ref().map(|m| m.view()) {
            Some(gst::MessageView::Error(err)) => format!("pipeline: {} ({:?})", err.error(), err.debug()),
            _ => format!("pipeline: {e}"),
        });
    }
    eprintln!("[luchd] mirroring to {} ({}: video {vp}, audio {ap})", t.name, t.address);

    let mut bus = p.bus().unwrap().stream();
    let mut second = tokio::time::interval(Duration::from_secs(1));
    let (mut rtt, mut rx_stats) = (Value::Null, Value::Null);
    let (mut last_frames, mut last_bytes, mut last_lat) = (0u64, 0u64, 0u64);
    let why = loop {
        tokio::select! {
            _ = second.tick() => {
                // Sender-side numbers, once a second: to the receiver (for its on-screen overlay) and to D-Bus.
                let frames = video.seq.load(Ordering::Relaxed) as u64;
                let bytes = video.bytes.load(Ordering::Relaxed);
                let lat = video.lat_sum.load(Ordering::Relaxed);
                let df = frames - last_frames;
                let host_ms = if df > 0 { (lat - last_lat) as f64 / df as f64 / 10.0 } else { 0.0 };
                let mbps = (bytes - last_bytes) as f64 * 8.0 / 1e6;
                (last_frames, last_bytes, last_lat) = (frames, bytes, lat);
                let sender = json!({"t": "sender_stats", "rtt_ms": rtt, "host_ms": round1(host_ms),
                                    "fps": df, "mbps": round1(mbps), "encoder": "h264_vaapi", "res": format!("{WIDTH}x{HEIGHT}")});
                let _ = cmd.send(Cmd::Send(sender.clone())).await;
                // Rough glass-to-glass estimate without the display: capture+encode + one-way network + decode.
                let decode = rx_stats["decode_ms"].as_f64().unwrap_or(0.0);
                let est = host_ms + rtt.as_f64().unwrap_or(0.0) / 2.0 + decode;
                *stats.lock().unwrap() = json!({"sender": sender, "receiver": rx_stats, "est_ms": round1(est)});
            }
            _ = stop.recv() => {
                let _ = cmd.send(Cmd::Send(json!({"t": "stop"}))).await;
                break Ok("stopped");
            }
            ev = events.recv() => match ev {
                Ok(v) if v["t"] == "keyframe" => {
                    want_idr.store(true, Ordering::Relaxed);
                    if let Some(k) = kms.as_mut().and_then(|k| k.stdin.as_mut()) {
                        use std::io::Write;
                        let _ = k.write_all(b"k");
                    }
                }
                Ok(v) if v["t"] == "stats" => rx_stats = v,
                Ok(v) if v["t"] == "_rtt" => rtt = json!(round1(v["ms"].as_f64().unwrap_or(0.0))),
                Ok(v) if v["t"] == "stop" => break Ok("stopped by receiver"),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break Ok("control channel closed"),
            },
            msg = bus.next() => match msg.as_ref().map(|m| m.view()) {
                Some(gst::MessageView::Error(err)) => {
                    let _ = cmd.send(Cmd::Send(json!({"t": "stop"}))).await;
                    break Err(format!("pipeline: {} ({:?})", err.error(), err.debug()));
                }
                Some(gst::MessageView::Eos(_)) | None => break Ok("capture ended"),
                _ => {}
            },
        }
    };
    let _ = p.set_state(gst::State::Null);
    if let Some(mut k) = kms {
        let _ = k.kill();
        let _ = k.wait();
    }
    if let Some((_sc, session, fd, _)) = portal {
        let _ = session.close().await;
        drop(fd);
    }
    why
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout() {
        let h = header(KIND_VIDEO, F_IDR, 2, 5, 0x01020304, 0x0a0b0c0d, 7000, 33);
        assert_eq!(&h[..8], &[1, 1, 0, 2, 0, 5, 0, 0]);
        assert_eq!(&h[8..12], &[1, 2, 3, 4]);
        assert_eq!(&h[12..16], &[0x0a, 0x0b, 0x0c, 0x0d]);
        assert_eq!(u32::from_be_bytes(h[16..20].try_into().unwrap()), 7000);
        assert_eq!(u16::from_be_bytes(h[20..22].try_into().unwrap()), 33);
    }

    #[test]
    fn opus_head_is_rfc7845() {
        let h = opus_head();
        assert_eq!(&h[..8], b"OpusHead");
        assert_eq!((h[8], h[9]), (1, 2));
        assert_eq!(u32::from_le_bytes(h[12..16].try_into().unwrap()), 48_000);
    }

    #[test]
    fn frame_is_sharded_and_reassembles() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let to = match rx.local_addr().unwrap() { std::net::SocketAddr::V4(a) => a, _ => unreachable!() };
        let tx = Tx { sock: UdpSocket::bind("127.0.0.1:0").unwrap(), kind: KIND_VIDEO, seq: AtomicU32::new(7), warned: AtomicBool::new(false), bytes: AtomicU64::new(0), lat_sum: AtomicU64::new(0) };
        tx.sock.connect(to).unwrap();
        let frame: Vec<u8> = (0..SHARD * 2 + 100).map(|i| i as u8).collect();
        tx.send_frame(&frame, F_IDR, 1234, 5);
        let mut got = Vec::new();
        let mut buf = [0u8; 2000];
        for i in 0..3u16 {
            let n = rx.recv(&mut buf).unwrap();
            assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), i);
            assert_eq!(u16::from_be_bytes([buf[4], buf[5]]), 3);
            assert_eq!(u32::from_be_bytes(buf[8..12].try_into().unwrap()), 7);
            got.extend_from_slice(&buf[HEADER..n]);
        }
        assert_eq!(got, frame);
    }
}
