//! What a receiver says works worse on its Android (`sdk`, `limits` in its welcome; luch-android PROTOCOL.md,
//! section 5), the audio mode that follows from it, and the reasons a receiver gives when it ends a session.

use serde_json::{json, Value};

pub const AUDIO_MODES: [&str; 4] = ["auto", "on", "off", "sync"];

fn has(welcome: &Value, limit: &str) -> bool {
    welcome["limits"]["limits"]
        .as_array()
        .is_some_and(|l| l.iter().any(|x| x.as_str() == Some(limit)))
}

/// How far the receiver's sound trails its picture, in ms (about 20 on Android 7+), if it says.
pub fn audio_latency_ms(welcome: &Value) -> Option<u64> {
    welcome["limits"]["audio_latency_ms"].as_u64()
}

/// "Android 5.1" from the API level, for people.
pub fn android(sdk: u64) -> String {
    let v = match sdk {
        21 => "5.0",
        22 => "5.1",
        23 => "6.0",
        24 => "7.0",
        25 => "7.1",
        26 => "8.0",
        27 => "8.1",
        28 => "9",
        29 => "10",
        30 => "11",
        31 => "12",
        32 => "12L",
        33 => "13",
        34 => "14",
        35 => "15",
        36 => "16",
        _ => return format!("API {sdk}"),
    };
    format!("Android {v}")
}

/// One line per limit, in plain words; unknown limits as they are.
pub fn describe(welcome: &Value) -> Vec<String> {
    let lag = audio_latency_ms(welcome).unwrap_or(0);
    welcome["limits"]["limits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|l| match l {
            "audio_lag" => format!(
                "sound trails the picture by about {lag} ms (Android 5 and 6 cannot shorten it): \
                 `luchctl audio <r> sync` holds the picture back to match, `off` leaves the sound out"
            ),
            "decoder_low_latency" => "no low-latency decoder mode (Android 11+)".into(),
            "game_mode" => "the display is not asked for its game mode (Android 11+)".into(),
            "wifi_low_latency" => "no low-latency Wi-Fi mode (Android 10+)".into(),
            "hdr_info" => "HDR support unknown (Android 7+)".into(),
            "display_modes" => "only the current display mode is known (Android 6+)".into(),
            other => other.to_string(),
        })
        .collect()
}

/// The `audio` part of `start` for a mode, or None to send the picture only.
/// "auto": `sync` where the sound cannot be made short (`audio_lag`), else plain sound.
pub fn start_audio(mode: &str, welcome: &Value, frame_ms: u32) -> Option<Value> {
    let sync = match mode {
        "off" => return None,
        "sync" => true,
        "on" => false,
        _ => has(welcome, "audio_lag"),
    };
    let mut a = json!({"codec": "audio/opus", "rate": 48000, "channels": 2, "frame_ms": frame_ms});
    if sync {
        a["sync"] = json!(true);
    }
    Some(a)
}

/// What the receiver's `{"t":"stop","by":"receiver","reason":...}` means.
pub fn stop_reason(v: &Value) -> &'static str {
    match v["reason"].as_str() {
        Some("back") => "Back pressed on the receiver",
        Some("screen_closed") => {
            "the picture was closed on the receiver (Home, another app, or its screen went off)"
        }
        Some("screen_not_shown") => {
            "the receiver could not show the picture: allow Luch to display over other apps on it \
             (adb shell appops set ru.toxblh.luch SYSTEM_ALERT_WINDOW allow)"
        }
        _ => "stopped by receiver",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old() -> Value {
        json!({"sdk": 22, "limits": {"audio_latency_ms": 120, "limits": ["audio_lag", "decoder_low_latency", "x_new"]}})
    }

    #[test]
    fn auto_syncs_only_where_sound_lags() {
        assert_eq!(start_audio("auto", &old(), 5).unwrap()["sync"], true);
        let new = json!({"sdk": 34, "limits": {"audio_latency_ms": 20, "limits": []}});
        assert!(start_audio("auto", &new, 5).unwrap().get("sync").is_none());
        // Receivers before limits existed: plain sound, as always.
        assert!(start_audio("auto", &json!({}), 5)
            .unwrap()
            .get("sync")
            .is_none());
    }

    #[test]
    fn a_chosen_mode_wins() {
        assert!(start_audio("off", &old(), 5).is_none());
        assert!(start_audio("on", &old(), 5).unwrap().get("sync").is_none());
        assert_eq!(start_audio("sync", &json!({}), 5).unwrap()["sync"], true);
    }

    #[test]
    fn limits_read_in_plain_words() {
        let d = describe(&old());
        assert!(d[0].contains("120 ms"), "{d:?}");
        assert_eq!(d[2], "x_new");
        assert_eq!(android(22), "Android 5.1");
    }

    #[test]
    fn stop_reasons() {
        assert!(stop_reason(&json!({"reason": "screen_not_shown"})).contains("SYSTEM_ALERT_WINDOW"));
        assert_eq!(stop_reason(&json!({})), "stopped by receiver");
    }
}
