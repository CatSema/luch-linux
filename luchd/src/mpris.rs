//! The players on this computer through MPRIS (PROTOCOL.md 4d): the TV remote's media keys go to the player that
//! plays, and what it plays goes to the TV as `now_playing`. Other systems put their own media API behind the same
//! two messages; this is the Linux one.

use std::collections::HashMap;

use serde_json::{json, Value};
use zbus::{zvariant::OwnedValue, Connection};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
/// How far seek_forward / seek_back go.
const SEEK_US: i64 = 10_000_000;

/// The D-Bus names of the players that are running.
pub async fn players(conn: &Connection) -> Vec<String> {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        return Vec::new();
    };
    let mut names: Vec<String> = dbus
        .list_names()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|n| n.to_string())
        .filter(|n| n.starts_with(PREFIX))
        .collect();
    names.sort();
    names
}

async fn proxy<'a>(
    conn: &Connection,
    name: &'a str,
    iface: &'a str,
) -> zbus::Result<zbus::Proxy<'a>> {
    zbus::Proxy::new(conn, name.to_string(), PATH, iface.to_string()).await
}

async fn status(conn: &Connection, name: &str) -> String {
    match proxy(conn, name, PLAYER).await {
        Ok(p) => p
            .get_property::<String>("PlaybackStatus")
            .await
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// The player the keys are for: one that plays, else one that is paused, else the first.
pub async fn active(conn: &Connection) -> Option<String> {
    let all = players(conn).await;
    let mut states = Vec::new();
    for n in &all {
        states.push((n.clone(), status(conn, n).await));
    }
    pick(&states)
}

/// Of (name, PlaybackStatus): Playing, then Paused, then any.
pub fn pick(states: &[(String, String)]) -> Option<String> {
    for want in ["Playing", "Paused"] {
        if let Some((n, _)) = states.iter().find(|(_, s)| s == want) {
            return Some(n.clone());
        }
    }
    states.first().map(|(n, _)| n.clone())
}

/// The MPRIS call for a `media_key`: (method, seek offset in µs for Seek).
pub fn method(key: &str) -> Option<(&'static str, i64)> {
    Some(match key {
        "play_pause" => ("PlayPause", 0),
        "play" => ("Play", 0),
        "pause" => ("Pause", 0),
        "next" => ("Next", 0),
        "previous" => ("Previous", 0),
        "stop" => ("Stop", 0),
        "seek_forward" => ("Seek", SEEK_US),
        "seek_back" => ("Seek", -SEEK_US),
        _ => return None,
    })
}

/// Passes a remote's key to the active player; false when there is none or the key is unknown.
pub async fn key(conn: &Connection, key: &str) -> bool {
    let Some((m, offset)) = method(key) else {
        return false;
    };
    let Some(name) = active(conn).await else {
        return false;
    };
    let Ok(p) = proxy(conn, &name, PLAYER).await else {
        return false;
    };
    let r = if m == "Seek" {
        p.call_method(m, &(offset,)).await
    } else {
        p.call_method(m, &()).await
    };
    if let Err(e) = &r {
        eprintln!("[luchd] media key {key} to {name}: {e}");
    }
    r.is_ok()
}

fn text(v: &OwnedValue) -> Option<String> {
    String::try_from(v.try_clone().ok()?).ok()
}

/// `now_playing` (without "t" and "art") from a player's Metadata, PlaybackStatus, Position (µs) and Identity.
pub fn describe(
    meta: &HashMap<String, OwnedValue>,
    status: &str,
    position_us: i64,
    app: &str,
) -> Value {
    let mut v = json!({});
    if let Some(t) = meta.get("xesam:title").and_then(text) {
        v["title"] = json!(t);
    }
    if let Some(a) = meta
        .get("xesam:artist")
        .and_then(|a| Vec::<String>::try_from(a.try_clone().ok()?).ok())
    {
        if !a.is_empty() {
            v["artist"] = json!(a.join(", "));
        }
    }
    if let Some(a) = meta.get("xesam:album").and_then(text) {
        v["album"] = json!(a);
    }
    let length = meta.get("mpris:length").and_then(|l| {
        i64::try_from(l.try_clone().ok()?)
            .ok()
            .or_else(|| u64::try_from(l.try_clone().ok()?).ok().map(|u| u as i64))
    });
    if let Some(l) = length.filter(|l| *l > 0) {
        v["dur"] = json!((l as f64 / 1e6 * 10.0).round() / 10.0);
    }
    if position_us > 0 {
        v["pos"] = json!((position_us as f64 / 1e6 * 10.0).round() / 10.0);
    }
    v["state"] = json!(match status {
        "Playing" => "playing",
        "Paused" => "paused",
        _ => "stopped",
    });
    if !app.is_empty() {
        v["app"] = json!(app);
    }
    v["can"] = json!(["play_pause", "next", "previous", "seek"]);
    v
}

/// The cover's location as the player gave it (file:// or http(s)), if any.
pub fn art(meta: &HashMap<String, OwnedValue>) -> Option<String> {
    meta.get("mpris:artUrl")
        .and_then(text)
        .filter(|u| !u.is_empty())
}

/// What the active player plays: (now_playing without "t" and "art", the cover's location).
pub async fn now(conn: &Connection) -> Option<(Value, Option<String>)> {
    let name = active(conn).await?;
    let p = proxy(conn, &name, PLAYER).await.ok()?;
    let meta: HashMap<String, OwnedValue> = p.get_property("Metadata").await.unwrap_or_default();
    let status: String = p.get_property("PlaybackStatus").await.unwrap_or_default();
    let pos: i64 = p.get_property("Position").await.unwrap_or(0);
    let app = match proxy(conn, &name, "org.mpris.MediaPlayer2").await {
        Ok(root) => root
            .get_property::<String>("Identity")
            .await
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    let a = art(&meta);
    Some((describe(&meta, &status, pos, &app), a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value as V;

    fn ov(v: V<'_>) -> OwnedValue {
        OwnedValue::try_from(v).unwrap()
    }

    #[test]
    fn keys_map_to_mpris() {
        assert_eq!(method("play_pause"), Some(("PlayPause", 0)));
        assert_eq!(method("seek_back"), Some(("Seek", -10_000_000)));
        assert_eq!(method("dance"), None);
    }

    #[test]
    fn the_playing_player_is_picked() {
        let s = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            pick(&[s("a", "Stopped"), s("b", "Paused"), s("c", "Playing")]).as_deref(),
            Some("c")
        );
        assert_eq!(
            pick(&[s("a", "Stopped"), s("b", "Paused")]).as_deref(),
            Some("b")
        );
        assert_eq!(pick(&[s("a", "Stopped")]).as_deref(), Some("a"));
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn metadata_becomes_now_playing() {
        let mut m = HashMap::new();
        m.insert("xesam:title".to_string(), ov(V::from("Song")));
        m.insert("xesam:artist".to_string(), ov(V::from(vec!["A", "B"])));
        m.insert("xesam:album".to_string(), ov(V::from("Album")));
        m.insert("mpris:length".to_string(), ov(V::from(215_000_000i64)));
        m.insert(
            "mpris:artUrl".to_string(),
            ov(V::from("file:///tmp/cover.jpg")),
        );
        let v = describe(&m, "Playing", 12_340_000, "Rhythmbox");
        assert_eq!(v["title"], "Song");
        assert_eq!(v["artist"], "A, B");
        assert_eq!(v["album"], "Album");
        assert_eq!(v["dur"], 215.0);
        assert_eq!(v["pos"], 12.3);
        assert_eq!(v["state"], "playing");
        assert_eq!(v["app"], "Rhythmbox");
        assert_eq!(art(&m).as_deref(), Some("file:///tmp/cover.jpg"));
        assert_eq!(
            describe(&HashMap::new(), "Stopped", 0, "")["state"],
            "stopped"
        );
    }

    /// A player on a private session bus: the keys reach it and its metadata comes back.
    #[tokio::test]
    async fn a_real_player_obeys_the_keys() {
        let Ok(conn) = Connection::session().await else {
            eprintln!("no session bus: skipped");
            return;
        };
        struct Fake {
            next: std::sync::Arc<std::sync::atomic::AtomicU32>,
        }
        #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
        impl Fake {
            fn next(&self) {
                self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            #[zbus(property)]
            fn playback_status(&self) -> String {
                "Playing".into()
            }
            #[zbus(property)]
            fn metadata(&self) -> HashMap<String, OwnedValue> {
                let mut m = HashMap::new();
                m.insert(
                    "xesam:title".to_string(),
                    OwnedValue::try_from(V::from("Fake song")).unwrap(),
                );
                m
            }
            #[zbus(property)]
            fn position(&self) -> i64 {
                1_000_000
            }
        }
        let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let name = format!("org.mpris.MediaPlayer2.luchtest{}", std::process::id());
        let _player = zbus::connection::Builder::session()
            .unwrap()
            .name(name.as_str())
            .unwrap()
            .serve_at(
                PATH,
                Fake {
                    next: count.clone(),
                },
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        assert!(players(&conn).await.contains(&name));
        assert!(key(&conn, "next").await);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (v, _) = now(&conn).await.unwrap();
        assert_eq!(v["title"], "Fake song");
        assert_eq!(v["state"], "playing");
    }
}
