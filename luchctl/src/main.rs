//! luchctl — CLI for luchd over D-Bus (dev.luch.Luch1).

use futures_util::StreamExt;
use luch_proto::ReceiverRow;

#[zbus::proxy(interface = "dev.luch.Luch1", default_service = "dev.luch.Luch1", default_path = "/dev/luch/Luch1")]
trait Luch1 {
    fn receivers(&self) -> zbus::Result<Vec<ReceiverRow>>;
    fn connect(&self, id: &str) -> zbus::Result<String>;
    fn disconnect(&self, id: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    fn receivers_changed(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn sender_id(&self) -> zbus::Result<String>;
}

const USAGE: &str = "usage: luchctl list | connect <id|name> | disconnect <id|name> | watch | id";

fn print(rows: &[ReceiverRow]) {
    if rows.is_empty() {
        println!("(no receivers)");
        return;
    }
    println!("{:<38} {:<20} {:<16} {:<21} {:<10} STATE", "ID", "NAME", "MODEL", "ADDRESS", "IFACE");
    for (id, name, model, addr, ifname, port, connected) in rows {
        let state = if *connected { "connected" } else { "-" };
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
        ["watch"] => {
            let mut changes = p.receive_receivers_changed().await.map_err(e)?;
            print(&p.receivers().await.map_err(e)?);
            while changes.next().await.is_some() {
                println!("--- changed");
                print(&p.receivers().await.map_err(e)?);
            }
        }
        _ => return Err(USAGE.into()),
    }
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
