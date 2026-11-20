//! luchd — Luch v0 sender daemon: announces `_luchsrc._tcp`, browses `_luch._tcp`
//! receivers via Avahi's D-Bus API, keeps control channels, exposes dev.luch.Luch1.

use std::{collections::HashMap, error::Error, io, net::SocketAddr, sync::Arc, time::Duration};

use futures_util::StreamExt;
use luch_proto::{self as p, ReceiverRow};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{tcp::OwnedWriteHalf, TcpSocket},
    sync::{mpsc, Mutex},
    time::{timeout, Instant},
};
use zbus::{
    fdo, interface, message::Type as MsgType, object_server::SignalEmitter,
    zvariant::OwnedObjectPath, Connection, MatchRule, MessageStream,
};

const AVAHI: &str = "org.freedesktop.Avahi";
const AVAHI_SERVER: &str = "org.freedesktop.Avahi.Server";
const AVAHI_IF_UNSPEC: i32 = -1;
const AVAHI_PROTO_INET: i32 = 0;
const PING_EVERY: Duration = Duration::from_secs(10);
const DEAD_AFTER: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
struct Receiver {
    id: String,
    name: String,
    model: String,
    address: String,
    ifname: String,
    ifindex: i32,
    port: u16,
    service: String, // Avahi instance name, for ItemRemove
}

#[derive(Default)]
struct State {
    found: HashMap<String, Receiver>,
    conns: HashMap<String, mpsc::Sender<()>>, // id -> stop signal
}
type Shared = Arc<Mutex<State>>;

struct Luch {
    st: Shared,
    sender_id: String,
    host: String,
}

#[interface(name = "dev.luch.Luch1")]
impl Luch {
    async fn receivers(&self) -> Vec<ReceiverRow> {
        let st = self.st.lock().await;
        let mut rows: Vec<ReceiverRow> = st
            .found
            .values()
            .map(|r| {
                let c = st.conns.contains_key(&r.id);
                (r.id.clone(), r.name.clone(), r.model.clone(), r.address.clone(), r.ifname.clone(), r.port, c)
            })
            .collect();
        rows.sort();
        rows
    }

    /// Opens and keeps a control channel; returns the receiver's welcome as JSON.
    async fn connect(&self, id: String, #[zbus(connection)] conn: &Connection) -> fdo::Result<String> {
        let r = {
            let st = self.st.lock().await;
            if st.conns.contains_key(&id) {
                return Err(fdo::Error::Failed(format!("already connected: {id}")));
            }
            st.found.get(&id).cloned().ok_or_else(|| fdo::Error::InvalidArgs(format!("unknown receiver: {id}")))?
        };
        let (welcome, rd, wr) = handshake(&r, &self.sender_id, &self.host)
            .await
            .map_err(|e| fdo::Error::Failed(format!("{} ({}%{}): {e}", r.name, r.address, r.ifname)))?;
        let (tx, rx) = mpsc::channel(1);
        self.st.lock().await.conns.insert(id.clone(), tx);
        eprintln!("[luchd] connected to {} ({}:{} via {})", r.name, r.address, r.port, r.ifname);
        changed(conn).await;
        tokio::spawn(run_channel(r.name, id, rd, wr, rx, self.st.clone(), conn.clone()));
        Ok(welcome.to_string())
    }

    async fn disconnect(&self, id: String) -> fdo::Result<()> {
        let tx = self.st.lock().await.conns.get(&id).cloned();
        let tx = tx.ok_or_else(|| fdo::Error::InvalidArgs(format!("not connected: {id}")))?;
        let _ = tx.send(()).await;
        Ok(())
    }

    #[zbus(property)]
    async fn sender_id(&self) -> String {
        self.sender_id.clone()
    }

    #[zbus(signal)]
    async fn receivers_changed(e: &SignalEmitter<'_>) -> zbus::Result<()>;
}

async fn changed(conn: &Connection) {
    if let Ok(e) = SignalEmitter::new(conn, p::DBUS_PATH) {
        let _ = Luch::receivers_changed(&e).await;
    }
}

// ---------- control channel ----------

type Reader = BufReader<tokio::net::tcp::OwnedReadHalf>;

/// One JSON line, at most MAX_LINE bytes. Ok(None) on EOF.
async fn read_msg(rd: &mut Reader) -> io::Result<Option<Value>> {
    let mut buf = Vec::new();
    let n = (&mut *rd).take(p::MAX_LINE as u64 + 1).read_until(b'\n', &mut buf).await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long or truncated"));
    }
    serde_json::from_slice(&buf).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

async fn send(wr: &mut OwnedWriteHalf, v: &Value) -> io::Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    wr.write_all(line.as_bytes()).await
}

/// Socket pinned to the interface the receiver was discovered on: the default
/// route may be a full-tunnel VPN, so it must never decide where this goes.
async fn handshake(r: &Receiver, id: &str, host: &str) -> io::Result<(Value, Reader, OwnedWriteHalf)> {
    let addr: SocketAddr = format!("{}:{}", r.address, r.port)
        .parse()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let sock = TcpSocket::new_v4()?;
    sock.bind_device(Some(r.ifname.as_bytes()))?;
    let stream = timeout(HANDSHAKE_TIMEOUT, sock.connect(addr))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timed out"))??;
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let hello = json!({"t": "hello", "v": p::VERSION, "id": id, "name": host, "app": p::APP,
                       "want": ["mirror", "media", "photo"]});
    send(&mut wr, &hello).await?;
    let welcome = timeout(HANDSHAKE_TIMEOUT, read_msg(&mut rd))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no welcome"))??
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "closed before welcome"))?;
    if welcome["t"] != "welcome" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("expected welcome, got {welcome}")));
    }
    Ok((welcome, rd, wr))
}

async fn run_channel(
    name: String,
    id: String,
    mut rd: Reader,
    mut wr: OwnedWriteHalf,
    mut stop: mpsc::Receiver<()>,
    st: Shared,
    conn: Connection,
) {
    // Reading lives in its own task: read_until is not cancel-safe inside select!.
    let (in_tx, mut inbox) = mpsc::channel::<Value>(16);
    let reader = tokio::spawn(async move {
        loop {
            match read_msg(&mut rd).await {
                Ok(Some(v)) => {
                    if in_tx.send(v).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) if e.kind() == io::ErrorKind::InvalidData => eprintln!("[luchd] bad line: {e}"),
                Err(_) => break,
            }
        }
    });

    let mut last_rx = Instant::now();
    let mut n: u64 = 0;
    let mut tick = tokio::time::interval(PING_EVERY);
    tick.tick().await;
    let why = loop {
        tokio::select! {
            _ = stop.recv() => {
                let _ = send(&mut wr, &json!({"t": "bye"})).await;
                break "disconnect requested";
            }
            _ = tick.tick() => {
                if last_rx.elapsed() >= DEAD_AFTER {
                    break "silent for 30 s";
                }
                n += 1;
                if send(&mut wr, &json!({"t": "ping", "n": n})).await.is_err() {
                    break "write failed";
                }
            }
            m = inbox.recv() => {
                let Some(v) = m else { break "closed by receiver" };
                last_rx = Instant::now();
                match v["t"].as_str().unwrap_or("") {
                    "pong" | "welcome" => {}
                    "ping" => {
                        if send(&mut wr, &json!({"t": "pong", "n": v["n"]})).await.is_err() {
                            break "write failed";
                        }
                    }
                    "bye" => break "bye from receiver",
                    "error" => eprintln!("[luchd] {name}: error {v}"),
                    t => {
                        let _ = send(&mut wr, &json!({"t": "error", "code": "unknown", "for": t})).await;
                    }
                }
            }
        }
    };
    reader.abort();
    let _ = wr.shutdown().await;
    eprintln!("[luchd] {name}: channel closed ({why})");
    st.lock().await.conns.remove(&id);
    changed(&conn).await;
}

// ---------- Avahi ----------

fn ifname(index: i32) -> String {
    let mut buf = [0 as libc::c_char; libc::IF_NAMESIZE];
    // SAFETY: buf is IF_NAMESIZE bytes, as if_indextoname requires.
    let p = unsafe { libc::if_indextoname(index as u32, buf.as_mut_ptr()) };
    if p.is_null() {
        return String::new();
    }
    // SAFETY: on success the buffer holds a NUL-terminated name.
    unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn txt_map(txt: &[Vec<u8>]) -> HashMap<String, String> {
    txt.iter()
        .filter_map(|e| {
            let s = String::from_utf8_lossy(e);
            let (k, v) = s.split_once('=')?;
            Some((k.to_ascii_lowercase(), v.to_string()))
        })
        .collect()
}

async fn publish(sys: &Connection, id: &str, host: &str) -> zbus::Result<OwnedObjectPath> {
    let group: OwnedObjectPath = sys
        .call_method(Some(AVAHI), "/", Some(AVAHI_SERVER), "EntryGroupNew", &())
        .await?
        .body()
        .deserialize()?;
    let txt: Vec<Vec<u8>> = [format!("v={}", p::VERSION), format!("id={id}"), format!("name={host}")]
        .into_iter()
        .map(String::into_bytes)
        .collect();
    let eg = Some("org.freedesktop.Avahi.EntryGroup");
    // The sender listens on nothing yet; port 9 (discard) is a placeholder, the TXT is what matters.
    let args = (AVAHI_IF_UNSPEC, AVAHI_PROTO_INET, 0u32, host, p::SENDER_TYPE, "", "", 9u16, txt);
    sys.call_method(Some(AVAHI), &group, eg, "AddService", &args).await?;
    sys.call_method(Some(AVAHI), &group, eg, "Commit", &()).await?;
    Ok(group)
}

async fn resolve(sys: &Connection, iface: i32, proto: i32, name: &str, ty: &str, domain: &str) -> zbus::Result<Option<Receiver>> {
    let args = (iface, proto, name, ty, domain, AVAHI_PROTO_INET, 0u32);
    let reply = sys.call_method(Some(AVAHI), "/", Some(AVAHI_SERVER), "ResolveService", &args).await?;
    let (ifindex, _, service, _, _, _host, _, address, port, txt, _): (
        i32, i32, String, String, String, String, i32, String, u16, Vec<Vec<u8>>, u32,
    ) = reply.body().deserialize()?;
    let t = txt_map(&txt);
    if t.get("v").is_some_and(|v| v != "0") {
        return Ok(None);
    }
    let Some(id) = t.get("id").cloned() else { return Ok(None) };
    Ok(Some(Receiver {
        name: t.get("name").cloned().unwrap_or_else(|| service.clone()),
        model: t.get("model").cloned().unwrap_or_default(),
        id,
        address,
        ifname: ifname(ifindex),
        ifindex,
        port,
        service,
    }))
}

// ponytail: browse runs once; if avahi-daemon restarts, restart luchd (systemd Restart=on-failure).
async fn discover(sys: Connection, st: Shared, conn: Connection) -> zbus::Result<()> {
    // Subscribe before creating the browser so no ItemNew is lost to the race.
    let rule = MatchRule::builder()
        .msg_type(MsgType::Signal)
        .sender(AVAHI)?
        .interface("org.freedesktop.Avahi.ServiceBrowser")?
        .build();
    let mut signals = MessageStream::for_match_rule(rule, &sys, None).await?;
    let browse = (AVAHI_IF_UNSPEC, AVAHI_PROTO_INET, p::RECEIVER_TYPE, "", 0u32);
    let _browser: OwnedObjectPath = sys
        .call_method(Some(AVAHI), "/", Some(AVAHI_SERVER), "ServiceBrowserNew", &browse)
        .await?
        .body()
        .deserialize()?;

    while let Some(msg) = signals.next().await {
        let msg = msg?;
        let member = msg.header().member().map(|m| m.to_string()).unwrap_or_default();
        if member != "ItemNew" && member != "ItemRemove" {
            if member == "Failure" {
                eprintln!("[luchd] avahi browse failure: {:?}", msg.body().deserialize::<String>());
            }
            continue;
        }
        let (iface, proto, name, ty, domain, _flags): (i32, i32, String, String, String, u32) = msg.body().deserialize()?;
        if ty != p::RECEIVER_TYPE {
            continue;
        }
        if member == "ItemRemove" {
            let mut s = st.lock().await;
            let before = s.found.len();
            s.found.retain(|_, r| !(r.ifindex == iface && r.service == name));
            if s.found.len() != before {
                drop(s);
                eprintln!("[luchd] gone: {name} ({})", ifname(iface));
                changed(&conn).await;
            }
            continue;
        }
        let (sys, st, conn) = (sys.clone(), st.clone(), conn.clone());
        tokio::spawn(async move {
            match resolve(&sys, iface, proto, &name, &ty, &domain).await {
                Ok(Some(r)) => {
                    let mut s = st.lock().await;
                    // ponytail: same id on several interfaces (a local stub, docker bridges) -> first one wins.
                    if s.found.get(&r.id).is_some_and(|old| old.ifindex != r.ifindex) {
                        return;
                    }
                    eprintln!("[luchd] found: {} id={} {}:{} via {}", r.name, r.id, r.address, r.port, r.ifname);
                    s.found.insert(r.id.clone(), r);
                    drop(s);
                    changed(&conn).await;
                }
                Ok(None) => eprintln!("[luchd] ignored {name}: no id or unsupported v"),
                Err(e) => eprintln!("[luchd] resolve {name} failed: {e}"),
            }
        });
    }
    Ok(())
}

// ---------- identity ----------

fn sender_id() -> io::Result<String> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(".config")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no HOME"))?
        .join("luch");
    let path = dir.join("id");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if !s.trim().is_empty() {
            return Ok(s.trim().to_string());
        }
    }
    std::fs::create_dir_all(&dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(&path, format!("{id}\n"))?;
    Ok(id)
}

fn short_host() -> String {
    let h = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    h.trim().split('.').next().filter(|s| !s.is_empty()).unwrap_or("linux").to_string()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let id = sender_id()?;
    let host = short_host();
    let st = Shared::default();
    let sys = Connection::system().await?;
    // ponytail: name collisions on the LAN are not handled (EntryGroup Collision state).
    let _group = match publish(&sys, &id, &host).await {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("[luchd] cannot publish {}: {e}", p::SENDER_TYPE);
            None
        }
    };
    let conn = zbus::connection::Builder::session()?
        .name(p::DBUS_NAME)?
        .serve_at(p::DBUS_PATH, Luch { st: st.clone(), sender_id: id.clone(), host: host.clone() })?
        .build()
        .await?;
    eprintln!("[luchd] {} id={id} name={host}; browsing {}", p::APP, p::RECEIVER_TYPE);
    discover(sys, st, conn).await?;
    Err("avahi browse stream ended".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_parses_and_lowercases_keys() {
        let t = txt_map(&[b"v=0".to_vec(), b"ID=abc".to_vec(), b"dec=avc,hevc".to_vec(), b"junk".to_vec()]);
        assert_eq!(t["v"], "0");
        assert_eq!(t["id"], "abc");
        assert_eq!(t["dec"], "avc,hevc");
        assert!(!t.contains_key("junk"));
    }

    #[tokio::test]
    async fn read_msg_limits_and_eof() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            s.write_all(b"{\"t\":\"pong\",\"n\":1}\n").await.unwrap();
            s.write_all(&vec![b'x'; p::MAX_LINE + 10]).await.unwrap();
        });
        let s = tokio::net::TcpStream::connect(a).await.unwrap();
        let (rd, _wr) = s.into_split();
        let mut rd = BufReader::new(rd);
        assert_eq!(read_msg(&mut rd).await.unwrap().unwrap()["t"], "pong");
        assert!(read_msg(&mut rd).await.is_err());
    }
}
