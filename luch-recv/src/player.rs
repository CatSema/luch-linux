//! The picture: an access unit goes into GStreamer, decoded (hardware when the system has it) and shown in a window.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use std::str::FromStr;

pub struct Player {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
}

impl Player {
    pub fn new(codec: &str) -> Result<Player, String> {
        gst::init().map_err(|e| e.to_string())?;
        let (caps, parse) = match codec {
            "video/hevc" => (
                "video/x-h265,stream-format=byte-stream,alignment=au",
                "h265parse",
            ),
            "video/avc" => (
                "video/x-h264,stream-format=byte-stream,alignment=au",
                "h264parse",
            ),
            other => return Err(format!("no decoder for {other}")),
        };
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true block=false ! {parse} ! decodebin ! videoconvert ! autovideosink sync=false"
        );
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_string())?;
        let src = pipeline
            .by_name("src")
            .ok_or("no appsrc")?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| "not an appsrc".to_string())?;
        src.set_caps(Some(&gst::Caps::from_str(caps).map_err(|e| e.to_string())?));
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| e.to_string())?;
        Ok(Player { pipeline, src })
    }

    pub fn push(&self, au: &[u8]) {
        let _ = self.src.push_buffer(gst::Buffer::from_slice(au.to_vec()));
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
