//! TCP transport, connection lifecycle, and the reconciliation session driver.
//!
//! ## Topology
//! Both machines run this daemon. To guarantee exactly one connection between a
//! pair (rather than two, one opened from each side), the roles are derived
//! deterministically from the hostnames: the lexicographically-smaller hostname
//! is the **client** and dials out; the larger is the **server** and listens.
//! Once a socket exists the session itself is fully symmetric.
//!
//! ## Session
//! A connection spawns a dedicated writer thread draining an mpsc channel to the
//! socket; the connection thread only ever reads. This decouples the two
//! directions so a large `Entry` batch can never deadlock against the peer's,
//! and lets the DB observer push a `Have` at any time by sending on the channel.
//!
//! ## Binding
//! The listener binds to the `tailscale0` interface's IPv4 only — never
//! `0.0.0.0` — so the sync port is reachable solely over the tailnet.

use crate::clip::db::ClipboardDb;
use crate::clipsync::db_observer::{DbObserver, POLL_INTERVAL};
use crate::clipsync::protocol::{Message, PROTOCOL_VERSION};
use crate::clipsync::reconcile;
use std::io::{BufReader, BufWriter, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// TCP port the sync daemon listens on / connects to. Arbitrary, fixed on both
/// ends; reachable only over the tailnet because the listener binds the
/// Tailscale IP.
pub const SYNC_PORT: u16 = 47654;

/// Backoff bounds for the client reconnect loop.
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(20);

/// Static configuration for the daemon, resolved once at startup.
pub struct Config {
    /// This machine's hostname.
    pub local_host: String,
    /// The peer's Tailscale hostname.
    pub peer_host: String,
}

impl Config {
    /// True when this machine should dial the peer (it owns the smaller name).
    /// The other machine listens. Equal names would be a misconfiguration; we
    /// fall back to listening so two identically-named hosts do not both dial.
    fn is_client(&self) -> bool {
        self.local_host < self.peer_host
    }
}

/// Run the daemon forever. Picks client or server behaviour from [`Config`].
pub fn run(cfg: Config) -> ! {
    if cfg.is_client() {
        run_client(cfg)
    } else {
        run_server(cfg)
    }
}

/// Server role: accept one peer connection at a time and service it.
fn run_server(cfg: Config) -> ! {
    let bind_ip = match tailscale_ipv4() {
        Ok(ip) => ip,
        Err(e) => {
            eprintln!("[clip-sync] cannot bind listener: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[clip-sync] bound to tailscale0 address {bind_ip}");
    let addr = SocketAddr::new(bind_ip, SYNC_PORT);

    loop {
        let listener = match TcpListener::bind(addr) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[clip-sync] bind {addr} failed: {e}; retrying in 5s");
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        eprintln!("[clip-sync] listening on {addr} for peer '{}'", cfg.peer_host);

        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let peer = s
                        .peer_addr()
                        .map(|a| a.to_string())
                        .unwrap_or_else(|_| "?".into());
                    eprintln!("[clip-sync] peer connected from {peer}");
                    if let Err(e) = serve_connection(s, &cfg) {
                        eprintln!("[clip-sync] connection ended: {e}");
                    }
                }
                Err(e) => eprintln!("[clip-sync] accept error: {e}"),
            }
        }
    }
}

/// Client role: keep a connection to the peer up, reconnecting with capped
/// exponential backoff. Each successful connect drives a full reconciliation,
/// which is exactly the network-restore resync.
fn run_client(cfg: Config) -> ! {
    let mut backoff = BACKOFF_START;
    loop {
        match dial_peer(&cfg.peer_host) {
            Ok(stream) => {
                eprintln!("[clip-sync] connected to peer '{}'", cfg.peer_host);
                backoff = BACKOFF_START;
                if let Err(e) = serve_connection(stream, &cfg) {
                    eprintln!("[clip-sync] connection to '{}' ended: {e}", cfg.peer_host);
                }
            }
            Err(e) => {
                eprintln!(
                    "[clip-sync] connect to '{}' failed: {e}; retry in {:?}",
                    cfg.peer_host, backoff
                );
            }
        }
        thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Resolve the peer hostname to its tailnet address and open a TCP connection.
fn dial_peer(peer_host: &str) -> std::io::Result<TcpStream> {
    let mut last_err = std::io::Error::new(std::io::ErrorKind::Other, "no addresses resolved");
    for addr in (peer_host, SYNC_PORT).to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// Drive one connection: handshake, full reconciliation, then steady-state push
/// of locally-observed changes until the peer disconnects.
fn serve_connection(stream: TcpStream, cfg: &Config) -> std::io::Result<()> {
    stream.set_nodelay(true).ok();

    let write_half = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    // Outgoing messages funnel through this channel into a single writer thread,
    // so reads and writes never block each other.
    let (tx, rx): (Sender<Message>, Receiver<Message>) = mpsc::channel();
    let writer_handle = thread::spawn(move || writer_loop(write_half, rx));

    // Handshake.
    tx.send(Message::Hello {
        version: PROTOCOL_VERSION,
        host: cfg.local_host.clone(),
    })
    .ok();

    match Message::read_from(&mut reader)? {
        Message::Hello { version, host } => {
            if version != PROTOCOL_VERSION {
                drop(tx);
                let _ = writer_handle.join();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("protocol version mismatch: local {PROTOCOL_VERSION}, peer {version}"),
                ));
            }
            eprintln!("[clip-sync] handshake ok with '{host}' (v{version})");
        }
        other => {
            drop(tx);
            let _ = writer_handle.join();
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("expected Hello, got {other:?}"),
            ));
        }
    }

    // The DB handle is shared with the observer thread; SQLite calls are
    // serialized through the mutex (the connection itself is not Sync).
    let db = Arc::new(Mutex::new(ClipboardDb::open_default().map_err(db_io_err)?));

    // Kick off the round: advertise everything we hold.
    let initial = {
        let guard = db.lock().unwrap();
        reconcile::local_hashes(&guard).map_err(db_io_err)?
    };
    tx.send(Message::Have(initial)).ok();

    // Start the local-change observer once the round is under way.
    spawn_observer(db.clone(), tx.clone());

    // Pending entry batch being received from the peer.
    let mut incoming: Vec<crate::clipsync::protocol::WireEntry> = Vec::new();

    loop {
        let msg = match Message::read_from(&mut reader) {
            Ok(m) => m,
            Err(e) => {
                drop(tx);
                let _ = writer_handle.join();
                return Err(e);
            }
        };

        match msg {
            Message::Hello { .. } => {
                // A second Hello is protocol noise; ignore it.
            }
            Message::Have(peer_hashes) => {
                // Peer told us what it holds — ask for whatever we lack.
                let want = {
                    let guard = db.lock().unwrap();
                    reconcile::missing_locally(&guard, &peer_hashes).map_err(db_io_err)?
                };
                tx.send(Message::Want(want)).ok();
            }
            Message::Want(wanted) => {
                // Peer asked for entries — send them, then close the batch.
                let entries = {
                    let guard = db.lock().unwrap();
                    reconcile::entries_for(&guard, &wanted).map_err(db_io_err)?
                };
                let n = entries.len();
                for e in entries {
                    tx.send(Message::Entry(e)).ok();
                }
                tx.send(Message::Done).ok();
                if n > 0 {
                    eprintln!("[clip-sync] sent {n} entries to peer");
                }
            }
            Message::Entry(e) => incoming.push(e),
            Message::Done => {
                // End of a peer batch — merge it atomically.
                let batch = std::mem::take(&mut incoming);
                if !batch.is_empty() {
                    let result = {
                        let guard = db.lock().unwrap();
                        reconcile::merge(&guard, batch).map_err(db_io_err)?
                    };
                    if result.inserted > 0 {
                        eprintln!("[clip-sync] merged {} new entries", result.inserted);
                        apply_if_freshest(&db, result.newest);
                    }
                }
            }
        }
    }
}

/// Writer thread: serialize every queued message onto the socket in order.
/// Exits when the channel closes (connection torn down) or a write fails.
fn writer_loop(stream: TcpStream, rx: Receiver<Message>) {
    let mut w = BufWriter::new(stream);
    for msg in rx {
        if msg.write_to(&mut w).is_err() {
            break;
        }
        // Flush eagerly: messages are infrequent and latency matters more than
        // batching here.
        if w.flush().is_err() {
            break;
        }
    }
}

/// Background thread polling the DB for clipd-appended rows and pushing their
/// hashes to the peer as a `Have`. The peer replies with a `Want` and the
/// normal session loop ships the entries — reusing the reconciliation path for
/// steady-state sync.
fn spawn_observer(db: Arc<Mutex<ClipboardDb>>, tx: Sender<Message>) {
    thread::spawn(move || {
        let mut observer = {
            let guard = db.lock().unwrap();
            match DbObserver::new(&guard) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("[clip-sync] observer init failed: {e}");
                    return;
                }
            }
        };
        loop {
            thread::sleep(POLL_INTERVAL);
            let new_hashes = {
                let guard = db.lock().unwrap();
                match observer.poll(&guard) {
                    Ok(h) => h,
                    Err(e) => {
                        eprintln!("[clip-sync] observer poll error: {e}");
                        continue;
                    }
                }
            };
            if new_hashes.is_empty() {
                continue;
            }
            // Channel closed => connection gone => stop observing.
            if tx.send(Message::Have(new_hashes)).is_err() {
                break;
            }
        }
    });
}

/// If the freshest entry just merged from the peer is the newest copy known to
/// either machine, push it onto the live clipboard so a `paste` here yields the
/// most recent thing copied on either side.
///
/// `merge` has already inserted the entry, so it is included in the DB's
/// `max_created_at`. The entry takes the live clipboard only when its
/// `created_at` is that maximum — i.e. nothing copied more recently (locally or
/// on the peer) exists. A more recent local copy therefore wins and is left
/// untouched.
///
/// clipd's watcher will re-observe this `wl-copy` and re-record it — but the
/// content hash already exists, so its upsert is a dedup no-op, not a new row.
fn apply_if_freshest(
    db: &Arc<Mutex<ClipboardDb>>,
    newest: Option<crate::clipsync::protocol::WireEntry>,
) {
    let Some(entry) = newest else { return };

    let max_created = {
        let guard = db.lock().unwrap();
        guard.max_created_at().unwrap_or(0)
    };
    if entry.created_at < max_created {
        return;
    }

    apply_to_clipboard(&entry);
}

/// Put an entry's bytes on the Wayland clipboard via `wl-copy`, mirroring
/// `clipboard.rs`'s activate path (explicit `--type` for images).
fn apply_to_clipboard(entry: &crate::clipsync::protocol::WireEntry) {
    let mut cmd = Command::new("wl-copy");
    if entry.mime.starts_with("image/") {
        cmd.arg("--type").arg(&entry.mime);
    }
    match cmd.stdin(Stdio::piped()).spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(&entry.content);
            }
            let _ = child.wait();
            eprintln!("[clip-sync] applied freshest peer entry to clipboard");
        }
        Err(e) => eprintln!("[clip-sync] wl-copy failed: {e}"),
    }
}

/// Name of the Tailscale interface, identical on every host the daemon runs on.
const TAILSCALE_IFACE: &str = "tailscale0";

/// Find this host's Tailscale IPv4 by interface name.
///
/// Enumerates the host's interfaces and returns the IPv4 bound to `tailscale0`.
/// This is exact — no routing heuristic, no CGNAT-range guess — and depends
/// only on the kernel's interface table, not the `tailscale` CLI.
///
/// Errors (rather than waiting or falling back) when `tailscale0` is absent or
/// has no IPv4: Tailscale is not up, and the daemon cannot function off the
/// tailnet anyway, so the listener must never land on `0.0.0.0`.
fn tailscale_ipv4() -> std::io::Result<IpAddr> {
    let ifaces = if_addrs::get_if_addrs()?;
    let mut iface_present = false;
    for iface in &ifaces {
        if iface.name != TAILSCALE_IFACE {
            continue;
        }
        iface_present = true;
        if let IpAddr::V4(v4) = iface.ip() {
            return Ok(IpAddr::V4(v4));
        }
    }
    let detail = if iface_present {
        format!("interface '{TAILSCALE_IFACE}' has no IPv4 address")
    } else {
        format!("interface '{TAILSCALE_IFACE}' not found")
    };
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrNotAvailable,
        format!("{detail} — is Tailscale up?"),
    ))
}

/// This machine's hostname, lowercased to match Tailscale's naming.
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|_| "localhost".into())
}

/// Wrap a rusqlite error as an io::Error so the connection loop can use `?`.
fn db_io_err(e: rusqlite::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, format!("db error: {e}"))
}
