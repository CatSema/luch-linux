//! luchctl — CLI for luchd over D-Bus (dev.luch.Luch1).

use futures_util::StreamExt;
use luch_proto::ReceiverRow;

#[zbus::proxy(interface = "dev.luch.Luch1", default_service = "dev.luch.Luch1", default_path = "/dev/luch/Luch1")]
trait Luch1 {
    fn receivers(&self) -> zbus::Result<Vec<ReceiverRow>>;
    fn connect(&self, id: &str) -> zbus::Result<String>;
    fn disconnect(&self, id: &str) -> zbus::Result<()>;
    fn mirror(&self, id: &str, source: &str) -> zbus::Result<()>;
    fn stop_mirror(&self, id: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn receivers_changed(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn sender_id(&self) -> zbus::Result<String>;
}

const USAGE: &str = "usage: luchctl [list | connect <r> | disconnect <r> | mirror <r> [monitor|window|virtual] | stop <r> | watch | id]
       <r> = receiver id or name; no arguments: interactive";

fn print(rows: &[ReceiverRow]) {
    if rows.is_empty() {
        println!("(no receivers)");
        return;
    }
    println!("{:<38} {:<20} {:<16} {:<21} {:<10} STATE", "ID", "NAME", "MODEL", "ADDRESS", "IFACE");
    for (id, name, model, addr, ifname, port, connected, mirroring) in rows {
        let state = if *mirroring { "mirroring" } else if *connected { "connected" } else { "-" };
        println!("{id:<38} {name:<20} {model:<16} {:<21} {ifname:<10} {state}", format!("{addr}:{port}"));
    }
}

async fn find_id(p: &Luch1Proxy<'_>, key: &str) -> Result<String, String> {
    let rows = p.receivers().await.map_err(|e| e.to_string())?;
    let hits: Vec<_> = rows.iter().filter(|r| r.0 == key || r.1.eq_ignore_ascii_case(key)).collect();
    match hits.as_slice() {
        [one] => Ok(one.0.clone()),
        [] => Err(format!("no receiver '{key}' (see `luchctl list`)")),
        _ => Err(format!("'{key}' is ambiguous, use the id")),
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    let conn = zbus::Connection::session().await.map_err(|e| e.to_string())?;
    let p = Luch1Proxy::new(&conn).await.map_err(|e| e.to_string())?;
    let e = |e: zbus::Error| format!("{e} (is luchd running?)");
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["list"] => print(&p.receivers().await.map_err(e)?),
        ["id"] => println!("{}", p.sender_id().await.map_err(e)?),
        ["connect", key] => {
            let id = find_id(&p, key).await?;
            let welcome = p.connect(&id).await.map_err(e)?;
            let pretty = serde_json::from_str::<serde_json::Value>(&welcome)
                .and_then(|v| serde_json::to_string_pretty(&v))
                .unwrap_or(welcome);
            println!("{pretty}");
        }
        ["disconnect", key] => p.disconnect(&find_id(&p, key).await?).await.map_err(e)?,
        ["mirror", key] => p.mirror(&find_id(&p, key).await?, "monitor").await.map_err(e)?,
        ["mirror", key, source] => p.mirror(&find_id(&p, key).await?, source).await.map_err(e)?,
        ["stop", key] => p.stop_mirror(&find_id(&p, key).await?).await.map_err(e)?,
        ["watch"] => {
            let mut changes = p.receive_receivers_changed().await.map_err(e)?;
            print(&p.receivers().await.map_err(e)?);
            while changes.next().await.is_some() {
                println!("--- changed");
                print(&p.receivers().await.map_err(e)?);
            }
        }
        [] => tui(&p).await.map_err(e)?,
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn draw(rows: &[ReceiverRow], note: &str) {
    print!("\x1b[2J\x1b[H"); // clear screen, cursor home
    println!("Luch — receivers on the LAN\n");
    if rows.is_empty() {
        println!("  (searching…)");
    }
    for (i, (_, name, model, addr, ifname, port, connected, mirroring)) in rows.iter().enumerate() {
        let mark = if *mirroring { "▶" } else if *connected { "●" } else { "○" };
        println!("  {} {mark} {name:<24} {model:<14} {addr}:{port} ({ifname})", i + 1);
    }
    println!("\n  <N> connect/disconnect · m<N> mirror screen · w<N> one window · e<N> extend · s<N> stop · q quit");
    if !note.is_empty() {
        println!("  {note}");
    }
    print!("> ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

// ponytail: line-based screen, no raw mode; ratatui when it grows menus.
async fn tui(p: &Luch1Proxy<'_>) -> zbus::Result<()> {
    use tokio::io::AsyncBufReadExt;
    let mut changes = p.receive_receivers_changed().await?;
    let mut input = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut rows = p.receivers().await?;
    let mut note = String::new();
    loop {
        draw(&rows, &note);
        tokio::select! {
            _ = changes.next() => {}
            line = input.next_line() => {
                let Ok(Some(line)) = line else { break };
                let cmd = line.trim();
                note.clear();
                let pick = |n: &str| n.parse::<usize>().ok().filter(|n| (1..=rows.len()).contains(n)).map(|n| &rows[n - 1]);
                let action = cmd.get(..1).zip(cmd.get(1..)).and_then(|(a, n)| Some((a, pick(n)?)));
                if let Some((a @ ("m" | "w" | "e" | "s"), (id, name, ..))) = action {
                    let r = match a {
                        "s" => p.stop_mirror(id).await.map(|_| "stopped"),
                        "w" => p.mirror(id, "window").await.map(|_| "mirroring a window"),
                        "e" => p.mirror(id, "virtual").await.map(|_| "extending the desktop"),
                        _ => p.mirror(id, "monitor").await.map(|_| "mirroring"),
                    };
                    note = match r { Ok(s) => format!("{name}: {s}"), Err(err) => format!("{name}: {err}") };
                    rows = p.receivers().await?;
                    continue;
                }
                match (cmd, cmd.parse::<usize>()) {
                    ("q", _) => break,
                    (_, Ok(n)) if (1..=rows.len()).contains(&n) => {
                        let (id, name, .., connected) = &rows[n - 1];
                        let r: zbus::Result<&str> = if *connected { p.disconnect(id).await.map(|_| "disconnected") } else { p.connect(id).await.map(|_| "connected") };
                        note = match r {
                            Ok(s) => format!("{name}: {s}"),
                            Err(err) => format!("{name}: {err}"),
                        };
                    }
                    ("" | "r", _) => {}
                    _ => note = format!("unknown command: {cmd}"),
                }
            }
        }
        rows = p.receivers().await?;
    }
    println!();
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(msg) = run(&args).await {
        eprintln!("luchctl: {msg}");
        std::process::exit(1);
    }
}
