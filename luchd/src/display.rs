//! Receiver display dimensions and temporary GNOME layout for extended desktops.
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use zbus::{zvariant::OwnedValue, Connection};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}
impl Default for Size {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
        }
    }
}
impl Size {
    fn new(width: u64, height: u64) -> Option<Self> {
        // NV12 requires even dimensions. Do not accept unbounded dimensions from the network.
        (16..=8192).contains(&width).then_some(())?;
        (16..=8192).contains(&height).then_some(())?;
        Some(Self {
            width: width as u32 & !1,
            height: height as u32 & !1,
        })
    }
}

/// Use display capabilities, never the decoder's unrelated maximum coded frame size.
/// Browsers send physical screen dimensions; older Android receivers advertise physical modes.
pub fn native_size(welcome: &Value) -> Option<Size> {
    let display = &welcome["hw"]["display"];
    if let Some(size) = display["width"]
        .as_u64()
        .zip(display["height"].as_u64())
        .and_then(|(w, h)| Size::new(w, h))
    {
        return Some(size);
    }
    display["modes"]
        .as_array()?
        .iter()
        .filter_map(|mode| {
            let resolution = mode.as_str()?.split('@').next()?;
            let (w, h) = resolution.split_once('x')?;
            Size::new(w.parse().ok()?, h.parse().ok()?)
        })
        .max_by_key(|s| u64::from(s.width) * u64::from(s.height))
}

type Props = HashMap<String, OwnedValue>;
type Spec = (String, String, String, String);
type Mode = (String, i32, i32, f64, f64, Vec<f64>, Props);
type Monitor = (Spec, Vec<Mode>, Props);
type Logical = (i32, i32, f64, u32, bool, Vec<Spec>, Props);
type State = (u32, Vec<Monitor>, Vec<Logical>, Props);
type Applied = (i32, i32, f64, u32, bool, Vec<(String, String, Props)>);
const NAME: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
fn flag(props: &Props, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}
fn number(props: &Props, key: &str) -> Option<u32> {
    props.get(key).and_then(|v| u32::try_from(v).ok())
}
fn current(monitor: &Monitor) -> Option<&Mode> {
    monitor.1.iter().find(|m| flag(&m.6, "is-current"))
}
async fn state(conn: &Connection) -> Result<State, String> {
    conn.call_method(Some(NAME), PATH, Some(NAME), "GetCurrentState", &())
        .await
        .map_err(|e| format!("GNOME display layout: {e}"))?
        .body()
        .deserialize()
        .map_err(|e| e.to_string())
}

/// A logical monitor exactly as it was before the virtual one appeared.
#[derive(Clone, Debug)]
struct LogicalSnapshot {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    specs: Vec<Spec>,
    /// Mode id per connector at capture time, so a compositor fallback does not change the refresh rate.
    modes: Vec<(String, String)>,
    /// Logical width in the layout mode that was active then.
    width: i32,
}

fn snapshot(state: &State, logical: &Logical) -> Option<LogicalSnapshot> {
    let logical_mode = number(&state.3, "layout-mode").unwrap_or(1) == 1;
    let spec = logical.5.first()?;
    let monitor = state.1.iter().find(|m| &m.0 == spec)?;
    let mode = current(monitor)?;
    let pixels = if logical.3 % 2 == 1 { mode.2 } else { mode.1 };
    let width = if logical_mode {
        (f64::from(pixels) / logical.2).round() as i32
    } else {
        pixels
    };
    Some(LogicalSnapshot {
        x: logical.0,
        y: logical.1,
        scale: logical.2,
        transform: logical.3,
        primary: logical.4,
        specs: logical.5.clone(),
        modes: logical
            .5
            .iter()
            .filter_map(|spec| {
                let monitor = state.1.iter().find(|m| &m.0 == spec)?;
                let mode = current(monitor)?;
                Some((spec.0.clone(), mode.0.clone()))
            })
            .collect(),
        width,
    })
}

pub struct Layout {
    conn: Connection,
    before: HashSet<String>,
    /// The active layout when the session started: the compositor may reorder monitors when the virtual one appears.
    before_layout: Vec<LogicalSnapshot>,
}
impl Layout {
    pub async fn capture() -> Result<Self, String> {
        let conn = Connection::session().await.map_err(|e| e.to_string())?;
        let state = state(&conn).await?;
        let before = state.1.iter().map(|m| m.0 .0.clone()).collect();
        let before_layout = state.2.iter().filter_map(|l| snapshot(&state, l)).collect();
        Ok(Self {
            conn,
            before,
            before_layout,
        })
    }
    pub async fn place_new_right(&self, size: Size) -> Result<String, String> {
        let mut last_error = "virtual monitor did not appear".to_string();
        // PipeWire creates/resizes the monitor asynchronously. A changed layout serial also
        // needs a fresh snapshot; never reapply an old configuration after another change.
        for _ in 0..30 {
            let state = state(&self.conn).await?;
            let new: Vec<_> = state
                .1
                .iter()
                .filter(|m| !self.before.contains(&m.0 .0) && m.0 .2 == "Virtual remote monitor")
                .collect();
            if new.len() > 1 {
                return Err("cannot identify our virtual monitor unambiguously".into());
            }
            if let Some(monitor) = new.first() {
                if current(monitor)
                    .is_some_and(|m| m.1 == size.width as i32 && m.2 == size.height as i32)
                {
                    let connector = monitor.0 .0.clone();
                    match place_right(&state, &connector, &self.before_layout) {
                        Ok(None) => return Ok(connector), // GNOME already placed it on the right.
                        Ok(Some(logical)) => {
                            let props = apply_props(&state)?;
                            match self
                                .conn
                                .call_method(
                                    Some(NAME),
                                    PATH,
                                    Some(NAME),
                                    "ApplyMonitorsConfig",
                                    &(state.0, 1u32, logical, props),
                                )
                                .await
                            {
                                Ok(_) => return Ok(connector), // temporary: do not overwrite monitors.xml
                                Err(e) => last_error = e.to_string(),
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        Err(format!("virtual display layout: {last_error}"))
    }

    /// After the portal session is closed, remove only the virtual monitor: put the physical
    /// monitors back exactly where and how they were before extend.
    pub async fn restore(&self, connector: &str) -> Result<(), String> {
        if self.before_layout.is_empty() {
            return Ok(());
        }
        // The portal session has just been closed: wait for Mutter to drop the virtual monitor.
        for _ in 0..20 {
            let state = state(&self.conn).await?;
            if !state.2.iter().any(|l| l.5.iter().any(|s| s.0 == connector)) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let state = state(&self.conn).await?;
        let applied = applied_from_snapshots(&state, &self.before_layout)?;
        if applied.is_empty() || layout_matches(&applied, &state) {
            return Ok(());
        }
        let props = apply_props(&state)?;
        self.conn
            .call_method(
                Some(NAME),
                PATH,
                Some(NAME),
                "ApplyMonitorsConfig",
                &(state.0, 1u32, applied, props),
            )
            .await
            .map_err(|e| format!("GNOME display layout: {e}"))?;
        Ok(())
    }
}

/// The physical monitors exactly as captured, with the modes that were active then.
fn applied_from_snapshots(
    state: &State,
    before: &[LogicalSnapshot],
) -> Result<Vec<Applied>, String> {
    let mut applied = Vec::new();
    for snap in before {
        let mut monitors = Vec::new();
        for spec in &snap.specs {
            let Some(monitor) = state.1.iter().find(|m| &m.0 == spec) else {
                continue; // that monitor went away
            };
            let mode = snap
                .modes
                .iter()
                .find(|(c, _)| c == &spec.0)
                .and_then(|(_, id)| monitor.1.iter().find(|m| &m.0 == id))
                .or_else(|| current(monitor))
                .ok_or("missing active monitor mode")?;
            monitors.push((spec.0.clone(), mode.0.clone(), monitor_props(monitor)?));
        }
        if !monitors.is_empty() {
            applied.push((
                snap.x,
                snap.y,
                snap.scale,
                snap.transform,
                snap.primary,
                monitors,
            ));
        }
    }
    Ok(applied)
}

/// Whether a config would leave the current layout exactly as it is.
fn layout_matches(applied: &[Applied], state: &State) -> bool {
    applied.len() == state.2.len()
        && applied.iter().zip(&state.2).all(|(a, l)| {
            a.0 == l.0
                && a.1 == l.1
                && a.2 == l.2
                && a.3 == l.3
                && a.4 == l.4
                && a.5.len() == l.5.len()
                && a.5.iter().all(|(conn, mode, _)| {
                    let Some(spec) = l.5.iter().find(|s| &s.0 == conn) else {
                        return false;
                    };
                    let Some(monitor) = state.1.iter().find(|m| &m.0 == spec) else {
                        return false;
                    };
                    current(monitor).is_some_and(|m| &m.0 == mode)
                })
        })
}

fn apply_props(state: &State) -> Result<Props, String> {
    let mut props = Props::new();
    if flag(&state.3, "supports-changing-layout-mode") {
        if let Some(mode) = number(&state.3, "layout-mode") {
            props.insert("layout-mode".into(), mode.into());
        }
    }
    let leased: Vec<_> = state
        .1
        .iter()
        .filter(|m| flag(&m.2, "is-for-lease"))
        .map(|m| m.0.clone())
        .collect();
    if !leased.is_empty() {
        props.insert(
            "monitors-for-lease".into(),
            OwnedValue::try_from(zbus::zvariant::Value::new(leased)).map_err(|e| e.to_string())?,
        );
    }
    Ok(props)
}

/// Restore the monitors exactly as they were before the virtual one appeared, then put it on the right.
fn place_right(
    state: &State,
    connector: &str,
    before: &[LogicalSnapshot],
) -> Result<Option<Vec<Applied>>, String> {
    let target = state
        .2
        .iter()
        .position(|l| l.5.iter().any(|s| s.0 == connector))
        .ok_or("virtual monitor has no logical monitor")?;
    if state.2[target].5.len() != 1 {
        return Err("virtual monitor unexpectedly belongs to a mirrored group".into());
    }
    let mut anchor = None;
    for snap in before {
        let edge = snap.x + snap.width;
        if anchor.is_none_or(|(right, _)| edge > right) {
            anchor = Some((edge, snap.y));
        }
    }
    let Some((x, y)) = anchor else {
        return Ok(None); // only display: nothing to extend
    };
    let mut applied = applied_from_snapshots(state, before)?;
    let mut monitors = Vec::new();
    for spec in &state.2[target].5 {
        let monitor = state
            .1
            .iter()
            .find(|m| &m.0 == spec)
            .ok_or("missing monitor")?;
        let mode = current(monitor).ok_or("missing active monitor mode")?;
        monitors.push((spec.0.clone(), mode.0.clone(), monitor_props(monitor)?));
    }
    applied.push((x, y, state.2[target].2, state.2[target].3, false, monitors));

    // Nothing to change: the virtual monitor is already on the right, the physical ones are in place
    // and no extra monitor was switched on.
    Ok((!layout_matches(&applied, state)).then_some(applied))
}

fn monitor_props(monitor: &Monitor) -> Result<Props, String> {
    let mut props = Props::new();
    for (source, target) in [
        ("is-underscanning", "underscanning"),
        ("color-mode", "color-mode"),
        ("rgb-range", "rgb-range"),
    ] {
        if let Some(value) = monitor.2.get(source) {
            props.insert(target.into(), value.try_clone().map_err(|e| e.to_string())?);
        }
    }
    Ok(props)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uses_display_modes_instead_of_decoder_limit() {
        let welcome =
            json!({"max":"4096x2304", "hw":{"display":{"modes":["1920x1080@120","3840x2160@60"]}}});
        assert_eq!(
            native_size(&welcome),
            Some(Size {
                width: 3840,
                height: 2160
            })
        );
        assert_eq!(native_size(&json!({"max":"4096x2304"})), None);
    }
    #[test]
    fn browser_dimensions_preserve_orientation_and_reject_bad_sizes() {
        assert_eq!(
            native_size(&json!({"hw":{"display":{"width":1179,"height":2556}}})),
            Some(Size {
                width: 1178,
                height: 2556
            })
        );
        for modes in [
            json!(["0x0@60"]),
            json!(["999999x1080@60"]),
            json!(["garbage"]),
        ] {
            assert_eq!(
                native_size(&json!({"hw":{"display":{"modes":modes}}})),
                None
            );
        }
    }
    fn monitor(name: &str, width: i32, height: i32) -> Monitor {
        let spec = (
            name.into(),
            "Vendor".into(),
            "Panel".into(),
            "serial".into(),
        );
        let mut props = Props::new();
        props.insert("is-current".into(), true.into());
        let mode = (
            format!("{width}x{height}@60"),
            width,
            height,
            60.0,
            1.0,
            vec![1.0, 1.25, 1.5],
            props,
        );
        (spec, vec![mode], Props::new())
    }
    fn snap(
        name: &str,
        x: i32,
        y: i32,
        scale: f64,
        transform: u32,
        primary: bool,
        width: i32,
    ) -> LogicalSnapshot {
        LogicalSnapshot {
            x,
            y,
            scale,
            transform,
            primary,
            specs: vec![(
                name.into(),
                "Vendor".into(),
                "Panel".into(),
                "serial".into(),
            )],
            modes: Vec::new(),
            width,
        }
    }
    #[test]
    fn places_virtual_right_of_scaled_rotated_displays_without_changing_them() {
        let mut main = monitor("main", 2560, 1440);
        main.2.insert("is-underscanning".into(), true.into());
        main.2.insert("color-mode".into(), 2u32.into());
        let rotated = monitor("portrait", 1920, 1080);
        let virtual_monitor = monitor("virtual", 3840, 2160);
        let logical = vec![
            (0, 0, 1.25, 0, true, vec![main.0.clone()], Props::new()),
            (
                2048,
                100,
                1.5,
                1,
                false,
                vec![rotated.0.clone()],
                Props::new(),
            ),
            (
                0,
                2000,
                1.0,
                0,
                false,
                vec![virtual_monitor.0.clone()],
                Props::new(),
            ),
        ];
        let before = vec![
            snap("main", 0, 0, 1.25, 0, true, 2048),
            snap("portrait", 2048, 100, 1.5, 1, false, 720),
        ];
        let mut state = (
            1,
            vec![main, rotated, virtual_monitor],
            logical,
            Props::new(),
        );
        let applied = place_right(&state, "virtual", &before).unwrap().unwrap();
        assert_eq!((applied[2].0, applied[2].1), (2768, 100));
        assert_eq!(
            (
                applied[0].0,
                applied[0].1,
                applied[0].2,
                applied[0].3,
                applied[0].4
            ),
            (0, 0, 1.25, 0, true)
        );
        assert_eq!(
            (applied[1].0, applied[1].1, applied[1].2, applied[1].3),
            (2048, 100, 1.5, 1)
        );
        assert!(flag(&applied[0].5[0].2, "underscanning"));
        assert_eq!(number(&applied[0].5[0].2, "color-mode"), Some(2));
        for (logical, a) in state.2.iter_mut().zip(&applied) {
            logical.0 = a.0;
            logical.1 = a.1;
            logical.2 = a.2;
            logical.3 = a.3;
            logical.4 = a.4;
        }
        assert!(place_right(&state, "virtual", &before).unwrap().is_none());
    }
    #[test]
    fn physical_layout_uses_pixel_width_without_dividing_by_scale() {
        let main = monitor("main", 3840, 2160);
        let target = monitor("virtual", 1920, 1080);
        let logical = vec![
            (0, 0, 2.0, 0, true, vec![main.0.clone()], Props::new()),
            (0, 2160, 1.0, 0, false, vec![target.0.clone()], Props::new()),
        ];
        let mut props = Props::new();
        props.insert("layout-mode".into(), 2u32.into());
        let state = (1, vec![main, target], logical, props);
        let before = vec![snap("main", 0, 0, 2.0, 0, true, 3840)];
        let applied = place_right(&state, "virtual", &before).unwrap().unwrap();
        assert_eq!((applied[1].0, applied[1].1), (3840, 0));
    }
    #[test]
    fn does_not_enable_monitors_that_were_off() {
        let main = monitor("main", 2560, 1440);
        let off = monitor("off", 1920, 1080);
        let virtual_monitor = monitor("virtual", 1920, 1080);
        let logical = vec![
            (0, 0, 1.0, 0, true, vec![main.0.clone()], Props::new()),
            (2560, 0, 1.0, 0, false, vec![off.0.clone()], Props::new()),
            (
                4480,
                0,
                1.0,
                0,
                false,
                vec![virtual_monitor.0.clone()],
                Props::new(),
            ),
        ];
        let state = (1, vec![main, off, virtual_monitor], logical, Props::new());
        let before = vec![snap("main", 0, 0, 1.0, 0, true, 2560)];
        let applied = place_right(&state, "virtual", &before).unwrap().unwrap();
        // The monitor that was off is left out and the virtual one goes right of the active display.
        assert_eq!(applied.len(), 2);
        assert_eq!((applied[1].0, applied[1].1), (2560, 0));
        assert!(applied.iter().all(|l| l.5.iter().all(|m| m.0 != "off")));
    }
    #[test]
    fn restores_monitor_order_when_mutter_reorders_them() {
        let left = monitor("left", 2560, 1440);
        let right = monitor("right", 3440, 1440);
        let virtual_monitor = monitor("virtual", 2800, 1840);
        // Mutter has swapped the physical monitors and put the virtual one after them.
        let logical = vec![
            (0, 0, 1.0, 0, true, vec![right.0.clone()], Props::new()),
            (3440, 0, 1.0, 0, false, vec![left.0.clone()], Props::new()),
            (
                6000,
                0,
                1.0,
                0,
                false,
                vec![virtual_monitor.0.clone()],
                Props::new(),
            ),
        ];
        let state = (1, vec![left, right, virtual_monitor], logical, Props::new());
        // Before extend: left was at 0, right at 2560 and primary.
        let before = vec![
            snap("left", 0, 0, 1.0, 0, false, 2560),
            snap("right", 2560, 0, 1.0, 0, true, 3440),
        ];
        let applied = place_right(&state, "virtual", &before).unwrap().unwrap();
        assert_eq!((applied[0].5[0].0.as_str(), applied[0].0), ("left", 0));
        assert_eq!((applied[1].5[0].0.as_str(), applied[1].0), ("right", 2560));
        assert!(applied[1].4); // the primary flag stays on the right display
        assert_eq!((applied[2].0, applied[2].1), (6000, 0));
    }
    #[test]
    fn keeps_the_captured_refresh_rate() {
        let mut main = monitor("main", 2560, 1440);
        let mut fast = Props::new();
        fast.insert("is-current".into(), false.into());
        main.1.push((
            "2560x1440@144".into(),
            2560,
            1440,
            144.0,
            1.0,
            vec![1.0],
            fast,
        ));
        let virtual_monitor = monitor("virtual", 1920, 1080);
        let logical = vec![
            (0, 0, 1.0, 0, true, vec![main.0.clone()], Props::new()),
            (
                2560,
                0,
                1.0,
                0,
                false,
                vec![virtual_monitor.0.clone()],
                Props::new(),
            ),
        ];
        let state = (1, vec![main, virtual_monitor], logical, Props::new());
        // Before extend the display ran at 144 Hz; Mutter then fell back to the 60 Hz mode.
        let mut before = snap("main", 0, 0, 1.0, 0, true, 2560);
        before.modes = vec![("main".into(), "2560x1440@144".into())];
        let applied = place_right(&state, "virtual", &[before]).unwrap().unwrap();
        assert_eq!(applied[0].5[0].1, "2560x1440@144");
    }
    #[test]
    fn restores_monitors_mutter_disabled_after_the_virtual_one_left() {
        let main = monitor("main", 3440, 1440);
        let second = monitor("second", 2560, 1440);
        // Mutter dropped the virtual monitor and the second display with it.
        let logical = vec![(0, 0, 1.0, 0, true, vec![main.0.clone()], Props::new())];
        let state = (1, vec![main, second], logical, Props::new());
        let before = vec![
            snap("main", 0, 0, 1.0, 0, true, 3440),
            snap("second", 3440, 0, 1.0, 0, false, 2560),
        ];
        let applied = applied_from_snapshots(&state, &before).unwrap();
        assert_eq!(applied.len(), 2);
        assert_eq!((applied[1].0, applied[1].5[0].0.as_str()), (3440, "second"));
        assert!(!layout_matches(&applied, &state));
    }
}
