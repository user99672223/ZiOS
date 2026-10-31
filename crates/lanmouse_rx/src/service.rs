//! The receiving end of lan-mouse, running on its own thread.
//!
//! This mirrors lan-mouse's `ListenTask` (`src/emulation.rs`, v0.11.0): it
//! acknowledges `Enter`/`Leave`, answers `Ping` with `Pong`, forwards input,
//! sends `Leave(0)` when the cursor should return to the laptop, and drops peers
//! that stop answering. Events are forwarded the moment they arrive.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use futures::{StreamExt, channel::mpsc};
use crate::proto::{Position, ProtoEvent};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::{
    crypto,
    listen::{LanMouseListener, ListenEvent},
};

/// lan-mouse's default port (`lan_mouse_ipc::DEFAULT_PORT`).
pub const DEFAULT_PORT: u16 = 4242;
/// A peer that sends nothing (not even pings) for this long is gone.
const PEER_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    /// PEM file holding this device's certificate and key.
    pub certificate_path: PathBuf,
    /// JSON map of authorized peer fingerprints to descriptions.
    pub authorized_path: PathBuf,
}

#[derive(Debug, Clone)]
pub enum ServiceEvent {
    Connected { addr: SocketAddr, fingerprint: String },
    Disconnected { addr: SocketAddr },
    /// An unknown laptop tried to connect; the user may authorize it.
    ConnectionAttempt { fingerprint: String },
    Entered { addr: SocketAddr, position: Position },
    Left { addr: SocketAddr },
    Input { addr: SocketAddr, event: input_event::Event },
}

#[derive(Debug)]
enum Request {
    Release,
    Stop,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Status {
    pub port: Option<u16>,
    pub error: Option<String>,
    pub fingerprint: String,
    /// Connected peers and their certificate fingerprints.
    pub peers: Vec<(SocketAddr, String)>,
    /// The peer currently driving this device's cursor.
    pub active: Option<SocketAddr>,
    pub pending_authorizations: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthorizedKeys(HashMap<String, String>);

pub struct Receiver {
    requests: tokio::sync::mpsc::UnboundedSender<Request>,
    status: Arc<Mutex<Status>>,
    authorized: Arc<RwLock<HashMap<String, String>>>,
    authorized_path: PathBuf,
}

impl Receiver {
    /// Starts listening. Returns the receiver and a stream of its events.
    pub fn start(config: Config) -> Result<(Self, mpsc::UnboundedReceiver<ServiceEvent>)> {
        let certificate = crypto::load_or_generate_key_and_cert(&config.certificate_path)
            .with_context(|| format!("certificate at {}", config.certificate_path.display()))?;
        let fingerprint = crypto::certificate_fingerprint(&certificate);
        let authorized = load_authorized(&config.authorized_path);
        let authorized = Arc::new(RwLock::new(authorized));
        let status = Arc::new(Mutex::new(Status {
            fingerprint,
            ..Default::default()
        }));
        let (event_sender, events) = mpsc::unbounded();
        let (requests, request_receiver) = tokio::sync::mpsc::unbounded_channel();

        let thread_status = status.clone();
        let thread_authorized = authorized.clone();
        let port = config.port;
        std::thread::Builder::new()
            .name("lan-mouse".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        thread_status.lock().error = Some(format!("runtime: {error}"));
                        return;
                    }
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(
                    &runtime,
                    run(
                        port,
                        certificate,
                        thread_authorized,
                        thread_status,
                        event_sender,
                        request_receiver,
                    ),
                );
            })
            .context("spawning the lan-mouse thread")?;

        Ok((
            Self {
                requests,
                status,
                authorized,
                authorized_path: config.authorized_path,
            },
            events,
        ))
    }

    pub fn status(&self) -> Status {
        self.status.lock().clone()
    }

    /// Hands the cursor back to the laptop that is driving this device.
    pub fn release(&self) {
        if self.requests.send(Request::Release).is_err() {
            log::warn!("lan-mouse receiver is not running");
        }
    }

    /// Authorizes a laptop's certificate fingerprint and remembers it.
    pub fn authorize(&self, fingerprint: &str, description: &str) -> Result<()> {
        let mut authorized = self
            .authorized
            .write()
            .map_err(|_| anyhow::anyhow!("authorized keys lock poisoned"))?;
        authorized.insert(fingerprint.to_string(), description.to_string());
        let keys = AuthorizedKeys(authorized.clone());
        drop(authorized);
        self.status
            .lock()
            .pending_authorizations
            .retain(|pending| pending != fingerprint);
        if let Some(parent) = self.authorized_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.authorized_path, serde_json::to_vec_pretty(&keys)?)
            .with_context(|| format!("writing {}", self.authorized_path.display()))
    }

    pub fn stop(&self) {
        self.requests.send(Request::Stop).ok();
    }
}

fn load_authorized(path: &PathBuf) -> HashMap<String, String> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<AuthorizedKeys>(&bytes) {
            Ok(keys) => keys.0,
            Err(error) => {
                log::error!("ignoring malformed {}: {error}", path.display());
                HashMap::new()
            }
        },
        Err(_) => HashMap::new(),
    }
}

async fn run(
    port: u16,
    certificate: webrtc_dtls::crypto::Certificate,
    authorized: Arc<RwLock<HashMap<String, String>>>,
    status: Arc<Mutex<Status>>,
    events: mpsc::UnboundedSender<ServiceEvent>,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<Request>,
) {
    let mut listener = match LanMouseListener::new(port, certificate, authorized).await {
        Ok(listener) => listener,
        Err(error) => {
            log::error!("lan-mouse: cannot listen on UDP port {port}: {error}");
            status.lock().error = Some(format!("cannot listen on port {port}: {error}"));
            return;
        }
    };
    log::info!("lan-mouse: listening on UDP port {port}");
    status.lock().port = Some(port);

    let send = |event: ServiceEvent| {
        if events.unbounded_send(event).is_err() {
            log::debug!("lan-mouse: nobody is listening for events");
        }
    };
    let mut last_seen: HashMap<SocketAddr, Instant> = HashMap::new();
    let mut liveness = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            event = listener.next() => match event {
                Some(ListenEvent::Msg { event, addr }) => {
                    last_seen.insert(addr, Instant::now());
                    match event {
                        ProtoEvent::Enter(position) => {
                            listener.reply(addr, ProtoEvent::Ack(0)).await;
                            status.lock().active = Some(addr);
                            send(ServiceEvent::Entered { addr, position });
                        }
                        ProtoEvent::Leave(_) => {
                            listener.reply(addr, ProtoEvent::Ack(0)).await;
                            let mut status = status.lock();
                            if status.active == Some(addr) {
                                status.active = None;
                            }
                            drop(status);
                            send(ServiceEvent::Left { addr });
                        }
                        ProtoEvent::Input(event) => send(ServiceEvent::Input { addr, event }),
                        // Emulation is always available on this device.
                        ProtoEvent::Ping => listener.reply(addr, ProtoEvent::Pong(true)).await,
                        ProtoEvent::Ack(_) | ProtoEvent::Pong(_) => {}
                    }
                }
                Some(ListenEvent::Accept { addr, fingerprint }) => {
                    log::info!("lan-mouse: {addr} connected");
                    last_seen.insert(addr, Instant::now());
                    status.lock().peers.push((addr, fingerprint.clone()));
                    send(ServiceEvent::Connected { addr, fingerprint });
                }
                Some(ListenEvent::Rejected { fingerprint }) => {
                    let mut status = status.lock();
                    if !status.pending_authorizations.contains(&fingerprint) {
                        log::info!("lan-mouse: unauthorized peer {fingerprint}");
                        status.pending_authorizations.push(fingerprint.clone());
                        drop(status);
                        send(ServiceEvent::ConnectionAttempt { fingerprint });
                    }
                }
                None => break,
            },
            request = requests.recv() => match request {
                Some(Request::Release) => {
                    let active = status.lock().active.take();
                    if let Some(addr) = active {
                        listener.reply(addr, ProtoEvent::Leave(0)).await;
                        send(ServiceEvent::Left { addr });
                    }
                }
                Some(Request::Stop) | None => break,
            },
            _ = liveness.tick() => {
                let now = Instant::now();
                let gone: Vec<SocketAddr> = last_seen
                    .iter()
                    .filter(|(_, seen)| now.duration_since(**seen) > PEER_TIMEOUT)
                    .map(|(addr, _)| *addr)
                    .collect();
                for addr in gone {
                    log::warn!("lan-mouse: {addr} stopped responding");
                    last_seen.remove(&addr);
                    let mut status = status.lock();
                    status.peers.retain(|(peer, _)| *peer != addr);
                    if status.active == Some(addr) {
                        status.active = None;
                    }
                    drop(status);
                    send(ServiceEvent::Disconnected { addr });
                }
            }
        }
    }
    listener.terminate().await;
    status.lock().port = None;
}
