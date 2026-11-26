//! CaptureSession: the single capture source of a group. Owns the GStreamer pipeline
//! and everything tied to its lifetime: the portal/PipeWire (or Niri virtual, or KMS)
//! capture, the screen-awake inhibitor and the sender's local mute.
//!
//! It knows nothing about receivers: encoded frames leave through the EncoderSession.

use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};

use ashpd::desktop::screencast::SourceType;
use gstreamer as gst;

use crate::display::Size;

use super::{portal, switch_to_16x9, Grabber, ModeGuard, MuteSession, NiriVirtual, ScreenAwake};

/// Live CaptureSession instances process-wide: makes "one capture session per group" a
/// runtime number (§15.3), not only a property of the type.
static CAPTURES: AtomicUsize = AtomicUsize::new(0);

pub fn live_captures() -> usize {
    CAPTURES.load(Ordering::SeqCst)
}

/// Everything a running capture needs kept alive, and nothing else.
/// Dropping it tears down the KMS grabber and the screen-awake inhibitor synchronously;
/// `close()` additionally performs the async portal/Niri cleanup.
pub struct CaptureSession {
    /// GStreamer pipeline: pipewiresrc (portal or Niri stream) or audio-only, plus Opus.
    pub pipeline: gst::Pipeline,
    portal: std::sync::Mutex<
        Option<(
            ashpd::desktop::screencast::Screencast,
            ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>,
            OwnedFd,
            u32,
        )>,
    >,
    niri: std::sync::Mutex<Option<NiriVirtual>>,
    kms: std::sync::Mutex<Option<Grabber>>,
    last_idr: std::sync::Mutex<Option<std::time::Instant>>,
    /// Present only while `mirror169` switched a monitor mode for the session.
    _mode: Option<ModeGuard>,
    _awake: ScreenAwake,
    _mute: MuteSession,
}

impl CaptureSession {
    /// Set up capture exactly like a single mirror session used to: portal (monitor/window/virtual),
    /// Niri virtual output, or the KMS grabber; then build the pipeline. One call per group —
    /// this is where "capture happens exactly once" is enforced by construction.
    pub async fn begin(
        source: SourceType,
        size: Size,
        kms: bool,
        mirror169: bool,
        niri_virtual: bool,
        audio_only: bool,
        niri_name: &str,
    ) -> Result<Self, String> {
        let awake = ScreenAwake::start().await;
        let mute = MuteSession::start();
        let portal = if kms || niri_virtual || audio_only {
            None
        } else {
            Some(portal(source).await?)
        };
        let mode = if kms && mirror169 {
            switch_to_16x9(size.width).await
        } else {
            None
        };
        let nv = if niri_virtual {
            Some(super::niri_virtual(niri_name, (size.width, size.height)).await?)
        } else {
            None
        };
        let virtual_source = source == SourceType::VIRTUAL && !niri_virtual;
        let p = super::pipeline(
            portal
                .as_ref()
                .map(|(_, _, fd, node)| (Some(fd.as_raw_fd()), *node))
                .or(nv.as_ref().map(|n| (None, n.node))),
            size,
            virtual_source,
        )?;
        Ok(CaptureSession {
            pipeline: p,
            portal: std::sync::Mutex::new(portal),
            niri: std::sync::Mutex::new(nv),
            kms: std::sync::Mutex::new(None),
            last_idr: std::sync::Mutex::new(None),
            _mode: mode,
            _awake: awake,
            _mute: mute,
        }
        .counted())
    }

    fn counted(self) -> Self {
        CAPTURES.fetch_add(1, Ordering::SeqCst);
        self
    }

    /// Take ownership of a started KMS grabber: its packets go into the group's
    /// EncoderSession, not into a per-receiver transport.
    pub fn install_kms(&self, g: Grabber) {
        *self.kms.lock().unwrap() = Some(g);
    }

    /// Ask the KMS grabber for an IDR, debounced by the same group-wide cooldown as the
    /// encoder path: repeated keyframe requests must not flood `k` commands.
    /// No-op on non-KMS capture paths (the encoder handles IDR there).
    pub fn request_idr(&self) {
        let now = std::time::Instant::now();
        if let Some(t) = *self.last_idr.lock().unwrap() {
            if now.duration_since(t) < super::encoder::IDR_COOLDOWN {
                return;
            }
        }
        *self.last_idr.lock().unwrap() = Some(now);
        if let Some(k) = self.kms.lock().unwrap().as_mut() {
            k.idr();
        }
    }

    /// Normal teardown after the pipeline stopped; also used on setup failures
    /// (the caller has already stopped the pipeline). Idempotent: takes the resources
    /// out of their slots, so a second call closes nothing twice.
    pub async fn close(&self) {
        // Take everything out of its slot before awaiting: no guard crosses an await
        // (the future must stay Send).
        let kms = self.kms.lock().unwrap().take();
        let niri = self.niri.lock().unwrap().take();
        let portal = self.portal.lock().unwrap().take();
        if let Some(k) = kms {
            k.stop();
        }
        if let Some(n) = niri {
            let _ = n.session.call::<_, _, ()>("Stop", &()).await;
        }
        if let Some((_sc, session, fd, _)) = portal {
            let _ = session.close().await;
            drop(fd);
        }
    }

    /// Test-only capture: no portal, no KMS, a trivial pipeline with just the "a" appsink,
    /// so group tests can run without a display server (same trick as the wire() test).
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        use gstreamer::prelude::*;
        let p = gst::Pipeline::new();
        let sink = gst::ElementFactory::make("appsink")
            .name("a")
            .property("sync", false)
            .build()
            .unwrap();
        p.add(&sink).unwrap();
        CaptureSession {
            pipeline: p,
            portal: std::sync::Mutex::new(None),
            niri: std::sync::Mutex::new(None),
            kms: std::sync::Mutex::new(None),
            last_idr: std::sync::Mutex::new(None),
            _mode: None,
            _awake: ScreenAwake(None),
            // Balanced start/drop of the session counter (no pulseaudio side effect
            // while local_audio stays at its default true).
            _mute: MuteSession::start(),
        }
        .counted()
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        CAPTURES.fetch_sub(1, Ordering::SeqCst);
    }
}
