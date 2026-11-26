//! The TV's volume from this computer (PROTOCOL.md 4e). While a session sends this computer's sound and plays none
//! of it here (LocalAudio off), the default output is luchd's null sink `luch_cast`: the volume keys then change that
//! sink, and luchd moves the TV's volume a step up or down with them. PipeWire's null sink monitor is taken before its volume, so the sound
//! itself is not turned down twice.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

const SINK: &str = "luch_cast";

/// The `volume` message for a word from luchctl or D-Bus: "40", "+", "-", "mute", "unmute".
pub fn message(what: &str) -> Result<Value, String> {
    Ok(match what.trim() {
        "+" | "up" => json!({"t": "volume", "step": 1}),
        "-" | "down" => json!({"t": "volume", "step": -1}),
        "mute" => json!({"t": "volume", "muted": true}),
        "unmute" => json!({"t": "volume", "muted": false}),
        n => {
            let level: u8 = n
                .trim_end_matches('%')
                .parse()
                .ok()
                .filter(|l| *l <= 100)
                .ok_or(format!("volume: 0..100, +, -, mute or unmute, not {n}"))?;
            json!({"t": "volume", "level": level})
        }
    })
}

/// The first percentage in `pactl get-sink-volume` output.
pub fn parse_percent(out: &str) -> Option<u8> {
    out.split('/')
        .map(str::trim)
        .find_map(|p| {
            p.strip_suffix('%')
                .and_then(|n| n.trim().parse::<u32>().ok())
        })
        .map(|p| p.min(100) as u8)
}

fn pactl(args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("pactl")
        .args(args)
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The cast sink's level and mute, when it is the default output now.
fn cast_sink_volume() -> Option<(u8, bool)> {
    if pactl(&["get-default-sink"])?.trim() != SINK {
        return None;
    }
    let level = parse_percent(&pactl(&["get-sink-volume", SINK])?)?;
    let muted = pactl(&["get-sink-mute", SINK])?.contains("yes");
    Some((level, muted))
}

/// What a change of the cast sink means for the TV: a step up or down, or mute on/off. Steps, not the level
/// itself: the TV keeps its own volume, and the keys only move it (a laptop at 80 % must not set the TV to 80 %).
pub fn change(before: (u8, bool), now: (u8, bool)) -> Option<Value> {
    if now.1 != before.1 {
        return Some(json!({"t": "volume", "muted": now.1}));
    }
    match now.0.cmp(&before.0) {
        std::cmp::Ordering::Greater => Some(json!({"t": "volume", "step": 1})),
        std::cmp::Ordering::Less => Some(json!({"t": "volume", "step": -1})),
        std::cmp::Ordering::Equal => None,
    }
}

/// Calls `tell` with a `volume` message whenever the cast sink's volume moves (the keys while a session sends the
/// sound). Runs as long as `pactl subscribe` does.
pub async fn follow(mut tell: impl FnMut(Value) + Send) {
    let Ok(mut child) = tokio::process::Command::new("pactl")
        .arg("subscribe")
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    else {
        return;
    };
    let Some(out) = child.stdout.take() else {
        return;
    };
    let mut lines = BufReader::new(out).lines();
    let mut last: Option<(u8, bool)> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.contains("'change' on sink #") {
            continue;
        }
        let now = tokio::task::spawn_blocking(cast_sink_volume)
            .await
            .ok()
            .flatten();
        match (last, now) {
            (Some(before), Some(now)) => {
                if let Some(m) = change(before, now) {
                    tell(m);
                }
            }
            _ => {}
        }
        last = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        assert_eq!(message("40").unwrap(), json!({"t": "volume", "level": 40}));
        assert_eq!(message("40%").unwrap()["level"], 40);
        assert_eq!(message("+").unwrap()["step"], 1);
        assert_eq!(message("down").unwrap()["step"], -1);
        assert_eq!(message("mute").unwrap()["muted"], true);
        assert!(message("140").is_err());
        assert!(message("loud").is_err());
    }

    #[test]
    fn keys_move_the_tv_by_steps() {
        assert_eq!(change((50, false), (55, false)).unwrap()["step"], 1);
        assert_eq!(change((50, false), (45, false)).unwrap()["step"], -1);
        assert_eq!(change((50, false), (50, true)).unwrap()["muted"], true);
        assert!(change((50, false), (50, false)).is_none());
        // Never a level: a loud laptop must not make the TV loud.
        assert!(change((10, false), (90, false))
            .unwrap()
            .get("level")
            .is_none());
    }

    #[test]
    fn pactl_output() {
        let out = "Volume: front-left: 32768 /  50% / -18.06 dB,   front-right: 32768 /  50% / -18.06 dB\n        balance 0.00\n";
        assert_eq!(parse_percent(out), Some(50));
        assert_eq!(
            parse_percent("Volume: mono: 98304 / 150% / 10.57 dB"),
            Some(100)
        );
        assert_eq!(parse_percent("nothing"), None);
    }
}
