use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use snow::TransportState;
use socket2::{Domain, Protocol, Socket, Type};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{
    Fallible,
    model::{Calendar, Id, MAX_PLANS, PLAN_LIMIT_ERROR, Record},
    storage::{
        LimitedWriter, LocalState, PeerRoutes, SharedStore, Store,
        StoreReader, Workspace,
    },
};

mod capabilities;
mod delta;
use capabilities::Capabilities;
mod discovery;
mod inbound;
use inbound::{Inbound, Wait};
mod pairing;
use pairing::{ChannelKey, PairingKeys};
mod scheduler;
use scheduler::{PeerProtocol, sync_forever};
mod wire;
use wire::{
    MAGIC, MESSAGE_LIMIT, Operation, SYNC_HELLO_LEN, calendar_digest, header, noise, read_calendar,
    read_control, read_frame, read_secure, send_calendar, send_control, send_secure, write_frame,
};

pub const SYNC_PORT: u16 = 45736;
pub const DISCOVERY_PORT: u16 = 45737;
const MAX_PEERS: usize = 64;
const MAX_REVISION_LEAD: u64 = 1_000_000;
const SESSION_LIMIT: Duration = Duration::from_secs(6);
const INVITE_PROBE_LIMIT: Duration = Duration::from_millis(1200);
const IO_POLL: Duration = Duration::from_millis(400);
const DISCOVERY_TTL: Duration = Duration::from_secs(20);
const INVITE_TTL: Duration = Duration::from_secs(15 * 60);
const INVITE_CODE_LIMIT: usize = 300;
const INVITE_KEY_TEXT_LEN: usize = 43;
const LEGACY_RECHECK: Duration = Duration::from_secs(60);
// Advertised only inside authenticated digest-sync handshakes. Older peers
// ignore the optional payload and retain their original polling cadence.
const DIGEST_CAPABILITIES: Capabilities = Capabilities::EDIT_WAKE
    .union(Capabilities::DELTA)
    .union(Capabilities::MUTUAL_DELTA)
    .union(Capabilities::REVISION_HINT);
const QUIET_INBOUND: Duration = Duration::from_secs(11);
const RECENT_INBOUND: Duration = Duration::from_secs(5);
const OVERLAP_DEFER: Duration = Duration::from_millis(300);

#[derive(Clone, Copy)]
pub struct Config {
    pub listen: SocketAddrV4,
    pub discovery_port: Option<u16>,
}

impl Config {
    fn effective_discovery_port(self) -> Option<u16> {
        let ip = self.listen.ip();
        self.discovery_port
            .filter(|_| ip.is_unspecified() || ip.is_loopback())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SYNC_PORT),
            discovery_port: Some(DISCOVERY_PORT),
        }
    }
}

struct InviteState {
    device: Id,
    recipient: [u8; 32],
    // The invitation map can move entries when inserting or removing a key.
    key: Box<Zeroizing<[u8; 32]>>,
    expires: Instant,
    used_by: Option<(Id, [u8; 32])>,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub last_sync: Option<i64>,
    pub warning: Option<String>,
    pub route_warning: Option<String>,
    pub merge_error: Option<String>,
    pub recent_peers: usize,
}

#[derive(Clone, Copy)]
struct Seen {
    device: Id,
    workspace: Id,
    time: Instant,
}

struct Core {
    pairing: PairingKeys,
    store: SharedStore,
    reader: StoreReader,
    listen: SocketAddrV4,
    port: AtomicU16,
    dialing: Mutex<()>,
    inbound: OnceLock<Inbound>,
    stopped: AtomicBool,
    paused: AtomicBool,
    generation: AtomicUsize,
    invites: Mutex<BTreeMap<Id, InviteState>>,
    pending_discovery: Mutex<Option<(Id, discovery::Key)>>,
    discovered: Mutex<BTreeMap<SocketAddrV4, Seen>>,
    routes: Mutex<PeerRoutes>,
    connected: Mutex<BTreeMap<Id, Instant>>,
    inbound_until: Mutex<BTreeMap<SocketAddrV4, InboundGrace>>,
    receiving_deltas: Mutex<BTreeMap<SocketAddrV4, ReceivingDelta>>,
    status: Mutex<Status>,
    digest: Mutex<Option<CachedDigest>>,
    delta_baselines: delta::Baselines,
    merge_errors: Mutex<BTreeMap<SocketAddrV4, String>>,
}

struct CachedDigest {
    workspace: Weak<Workspace>,
    digest: [u8; 32],
}

struct InboundGrace {
    until: Instant,
    confirmed: Option<ConfirmedInbound>,
}

struct ConfirmedInbound {
    workspace: Weak<Workspace>,
    generation: usize,
}

impl ConfirmedInbound {
    fn matches(&self, state: &LocalState, generation: usize) -> bool {
        self.generation == generation
            && state
                .workspace
                .as_ref()
                .is_some_and(|live| std::ptr::eq(Arc::as_ptr(live), self.workspace.as_ptr()))
    }
}

struct InboundDelta<'a> {
    core: &'a Core,
    peer: SocketAddrV4,
}

struct ReceivingDelta {
    count: usize,
    defer_until: Instant,
}

impl Drop for InboundDelta<'_> {
    fn drop(&mut self) {
        let mut receiving = self.core.receiving_deltas.lock().unwrap();
        let active = receiving
            .get_mut(&self.peer)
            .expect("registered inbound delta");
        active.count -= 1;
        if active.count == 0 {
            let still_deferred = active.defer_until > Instant::now();
            receiving.remove(&self.peer);
            drop(receiving);
            if still_deferred {
                self.core.wake_sync();
            }
        }
    }
}

pub struct Network {
    core: Arc<Core>,
    workers: Vec<thread::JoinHandle<()>>,
    joining: Mutex<Option<AccessJob>>,
}

struct AccessJob {
    worker: thread::JoinHandle<()>,
    completed: Arc<AtomicBool>,
}

impl Network {
    pub fn start(store: SharedStore, config: Config) -> Fallible<Self> {
        Self::start_with_pause(store, config, false)
    }

    pub fn start_with_pause(store: SharedStore, config: Config, paused: bool) -> Fallible<Self> {
        let config = Config {
            discovery_port: config.effective_discovery_port(),
            ..config
        };
        let paused = paused || store.lock().unwrap().sync_paused();
        let core = Arc::new(Core::new(store, config.listen)?);
        let (inbound, tcp, udp) = Inbound::new(config);
        if !paused {
            inbound.bind(&mut inbound.ports.lock().unwrap(), &core)?;
        }
        core.inbound
            .set(inbound)
            .map_err(|_| "Повторная инициализация сети.")?;
        core.paused.store(paused, Ordering::Release);
        let mut workers = Vec::new();
        if let Some(port) = config.discovery_port {
            let core = Arc::clone(&core);
            workers.push(thread::spawn(move || discovery::run(core, udp, port)));
        }
        let accept_core = Arc::clone(&core);
        workers.push(thread::spawn(move || listen_forever(accept_core, tcp)));
        let sync_core = Arc::clone(&core);
        workers.push(thread::spawn(move || sync_forever(sync_core)));
        Ok(Self {
            core,
            workers,
            joining: Mutex::default(),
        })
    }

    pub fn address(&self) -> Option<SocketAddrV4> {
        if !self.core.active() {
            return None;
        }
        let listen = self.core.listen;
        let ip = if listen.ip().is_unspecified() {
            primary_ipv4()?
        } else {
            *listen.ip()
        };
        Some(SocketAddrV4::new(
            ip,
            self.core.port.load(Ordering::Acquire),
        ))
    }

    pub fn status(&self) -> Status {
        let mut status = self.core.status.lock().unwrap().clone();
        if let Some(warning) = self.core.inbound.get().and_then(Inbound::warning) {
            status.warning = Some(warning);
        }
        status.route_warning = self.core.routes.lock().unwrap().warning.clone();
        let connected = self.core.connected.lock().unwrap();
        let now = Instant::now();
        status.recent_peers = connected
            .values()
            .filter(|&&seen| now - seen < DISCOVERY_TTL)
            .count();
        drop(connected);
        status.merge_error = self
            .core
            .merge_errors
            .lock()
            .unwrap()
            .iter()
            .next()
            .map(|(peer, message)| format!("{peer}: {message}"));
        status
    }

    pub fn has_recent_peer(&self) -> bool {
        let connected = self.core.connected.lock().unwrap();
        let now = Instant::now();
        connected.values().any(|&seen| now - seen < DISCOVERY_TTL)
    }

    #[cfg(feature = "desktop")]
    pub fn recent_peer_remaining(&self) -> Option<Duration> {
        let latest = self
            .core
            .connected
            .lock()
            .unwrap()
            .values()
            .max()
            .copied()?;
        let remaining = DISCOVERY_TTL.saturating_sub(latest.elapsed());
        (!remaining.is_zero()).then_some(remaining)
    }

    pub fn paused(&self) -> bool {
        self.core.paused.load(Ordering::Relaxed)
    }

    /// A failed port rebind can resume a paused network without user input.
    pub fn can_sync_automatically(&self) -> bool {
        !self.paused()
            || self.core.inbound.get().is_some_and(|input| {
                // A successful retry clears its deadline before releasing the ports lock.
                input.retry_at().is_some() || !self.paused()
            })
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.core.change_pause(paused); // Network::status exposes any pending rebind.
    }

    pub fn set_profile_paused(&self, paused: bool) -> Fallible<()> {
        let mut store = self.core.store.lock().unwrap();
        if paused {
            self.core.change_pause(true)?;
        }
        store.set_sync_paused(paused).map_err(|error| {
            if paused { format!("Обмен приостановлен до закрытия приложения. Не удалось сохранить паузу. {error}").into() } else {error}
        })?;
        if !paused {
            self.core.change_pause(false)?;
        }
        Ok(())
    }

    pub fn pairing_request(&self) -> String {
        self.core.pairing.request(self.core.snapshot().device)
    }

    pub fn make_invite(&self, request: &str) -> Fallible<String> {
        use std::fmt::Write as _;

        let (recipient_device, recipient) = PairingKeys::from_request(request)?;
        self.core.require_available()?;
        let state = self.core.snapshot();
        let team = workspace(&state)?;
        team.require_device_access(recipient_device)?;
        team.require_manager(state.device)?;
        let device = state.device;
        let mut invites = self.core.invites.lock().unwrap();
        invites.retain(|_, invite| invite.expires > Instant::now());
        if invites.len() >= MAX_PEERS {
            return Err("Слишком много активных приглашений. Попробуйте позже.".into());
        }
        let id = Id::new_v4();
        let mut key = Box::new(Zeroizing::new([0; 32]));
        getrandom::fill(&mut key[..]).map_err(|error| io::Error::other(error.to_string()))?;
        let mut encoded = Zeroizing::new([0; INVITE_KEY_TEXT_LEN]);
        let encoded_len = URL_SAFE_NO_PAD.encode_slice(&key[..], &mut encoded[..])?;
        invites.insert(
            id,
            InviteState {
                device: recipient_device,
                recipient,
                key,
                expires: Instant::now() + INVITE_TTL,
                used_by: None,
            },
        );
        let address = self
            .address()
            .map_or_else(|| "-".into(), |addr| addr.to_string());
        let mut code = String::with_capacity(INVITE_CODE_LIMIT);
        write!(
            code,
            "yr2.{device}.{id}.{}.{}.{address}",
            std::str::from_utf8(&encoded[..encoded_len])?,
            self.core.pairing.encoded_public()
        )?;
        Ok(code)
    }

    pub fn join_async(
        &self,
        code: String,
        employee: String,
        address: Option<SocketAddrV4>,
    ) -> Receiver<Fallible<()>> {
        let code = Zeroizing::new(code);
        self.access_job(move |core| join(core, code, employee, address, false))
    }

    pub fn rejoin_async(
        &self,
        code: String,
        address: Option<SocketAddrV4>,
    ) -> Receiver<Fallible<()>> {
        let code = Zeroizing::new(code);
        let state = self.core.snapshot();
        let name = state
            .workspace
            .as_ref()
            .and_then(|w| w.calendar.members.get(&state.device))
            .map(|m| m.value.clone())
            .unwrap_or_default();
        self.access_job(move |core| {
            core.pause_for_renewal()?;
            join(core, code, name, address, true)
        })
    }

    /// Begin a new group-key epoch. Old connections retain only the retired store.
    pub fn renew_access_async(&self) -> Receiver<Fallible<()>> {
        self.rotate_access(None)
    }

    pub fn revoke_devices_async(&self, devices: BTreeSet<Id>) -> Receiver<Fallible<()>> {
        self.rotate_access(Some(devices))
    }

    fn rotate_access(&self, revocation: Option<BTreeSet<Id>>) -> Receiver<Fallible<()>> {
        self.access_job(move |core| {
            if revocation.as_ref().is_some_and(BTreeSet::is_empty) {
                return Err("Выберите устройства для отзыва доступа.".into());
            }
            let devices = revocation.unwrap_or_default();
            core.store.lock().unwrap().check_revocations(&devices)?;
            core.pause_for_renewal()?;
            core.store.lock().unwrap().renew_workspace_key(&devices)
        })
    }

    fn access_job(
        &self,
        work: impl FnOnce(&Core) -> Fallible<()> + Send + 'static,
    ) -> Receiver<Fallible<()>> {
        let core = Arc::clone(&self.core);
        let (send, receive) = mpsc::sync_channel(1);
        let mut joining = self.joining.lock().unwrap();
        if joining
            .as_ref()
            .is_some_and(|job| !job.completed.load(Ordering::Acquire) && !job.worker.is_finished())
        {
            let _ = send.send(Err("Подключение уже выполняется.".into()));
            return receive;
        }
        let previous = joining.take().map(|job| job.worker);
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = Arc::clone(&completed);
        let worker = thread::spawn(move || {
            if let Some(previous) = previous {
                let _ = previous.join();
            }
            let result = work(&core);
            // A reply permits another operation even while this thread is exiting.
            worker_completed.store(true, Ordering::Release);
            let _ = send.send(result);
        });
        *joining = Some(AccessJob { worker, completed });
        receive
    }

    pub fn remember_peer(&self, address: SocketAddrV4) -> Fallible<()> {
        self.core.check_peer(address)?;
        self.core.store.lock().unwrap().transaction(|state| {
            if !state.peers.contains(&address) && state.peers.len() >= MAX_PEERS {
                return Err(
                    "Список сохранённых адресов заполнен. Удалите устаревший адрес.".into(),
                );
            }
            state.peers.insert(address);
            Ok(())
        })?;
        self.core.wake_sync();
        Ok(())
    }

    pub fn remove_saved_address(&self, address: SocketAddrV4) -> Fallible<()> {
        let removed = self
            .core
            .store
            .lock()
            .unwrap()
            .transaction(|state| Ok(state.peers.remove(&address)))?;
        if removed {
            self.core.wake_sync();
        }
        Ok(())
    }
}

impl Drop for Network {
    fn drop(&mut self) {
        self.core.stopped.store(true, Ordering::Release);
        let _ = self.core.change_pause(true);
        for worker in &self.workers {
            worker.thread().unpark();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        if let Some(job) = self.joining.get_mut().unwrap().take() {
            let _ = job.worker.join();
        }
    }
}

impl Core {
    fn new(store: SharedStore, listen: SocketAddrV4) -> Fallible<Self> {
        let (reader, routes) = {
            let store = store.lock().unwrap();
            (store.reader(), store.peer_routes(MAX_PEERS))
        };
        Ok(Self {
            pairing: PairingKeys::generate()?,
            store,
            reader,
            listen,
            port: AtomicU16::new(listen.port()),
            dialing: Mutex::default(),
            inbound: OnceLock::new(),
            stopped: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            generation: AtomicUsize::new(0),
            invites: Mutex::default(),
            pending_discovery: Mutex::default(),
            discovered: Mutex::default(),
            routes: Mutex::new(routes),
            connected: Mutex::default(),
            inbound_until: Mutex::default(),
            receiving_deltas: Mutex::default(),
            status: Mutex::default(),
            digest: Mutex::default(),
            delta_baselines: Default::default(),
            merge_errors: Mutex::default(),
        })
    }

    fn change_pause(&self, paused: bool) -> Fallible<()> {
        let inbound = self.inbound.get();
        let mut ports = inbound.map(|p| p.ports.lock().unwrap());
        if !paused && let (Some(input), Some(ports)) = (inbound, ports.as_mut()) {
            if self.stopped.load(Ordering::Acquire) {
                return Err("Обмен остановлен.".into());
            }
            let result = input.bind(ports, self);
            if result.is_err() {
                input.wake();
            }
            result?;
        }
        if self.paused.swap(paused, Ordering::AcqRel) != paused {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        if paused {
            if let (Some(input), Some(ports)) = (inbound, ports.as_mut()) {
                input.close(ports);
            }
            self.connected.lock().unwrap().clear();
        }
        if paused {
            // Let an already admitted TCP connect finish its bounded dial phase.
            // Once this returns no autonomous dial admitted before pause can start.
            drop(self.dialing.lock().unwrap());
        }
        drop(ports);
        if let Some(input) = inbound {
            input.wake();
        }
        Ok(())
    }

    fn advertised_port(&self) -> u16 {
        // A paused renewal still needs a valid legacy protocol hint. The default
        // port is used until a dynamic listener has been opened for the first time.
        match self.port.load(Ordering::Acquire) {
            0 => SYNC_PORT,
            port => port,
        }
    }

    fn pause_for_renewal(&self) -> Fallible<()> {
        self.change_pause(true)?;
        self.invites.lock().unwrap().clear();
        self.store.lock().unwrap().set_sync_paused(true)
    }

    fn permits_operation(&self, renewal: bool) -> bool {
        !self.stopped.load(Ordering::Acquire) && (renewal || !self.paused.load(Ordering::Acquire))
    }

    fn require_operation(&self, renewal: bool) -> Fallible<()> {
        if self.permits_operation(renewal) {
            Ok(())
        } else {
            Err("Синхронизация приостановлена.".into())
        }
    }

    fn permits_ip(&self, ip: Ipv4Addr) -> bool {
        local_ip(ip) && (!self.listen.ip().is_loopback() || ip.is_loopback())
    }

    fn check_peer(&self, address: SocketAddrV4) -> Fallible<()> {
        check_address(address)?;
        if !self.permits_ip(*address.ip()) {
            return Err(
                "Локальный режим loopback допускает только адреса этого компьютера.".into(),
            );
        }
        Ok(())
    }

    fn active(&self) -> bool {
        !self.stopped.load(Ordering::Acquire) && !self.paused.load(Ordering::Acquire)
    }

    fn require_available(&self) -> Fallible<()> {
        if self.active() {
            Ok(())
        } else {
            Err("Синхронизация приостановлена.".into())
        }
    }

    fn sync_context(&self) -> Fallible<(Id, Id, [u8; 32])> {
        let state = self.snapshot();
        let workspace = state
            .workspace
            .as_ref()
            .ok_or("Сначала подключитесь к пространству.")?;
        Ok((state.device, workspace.id, workspace.key))
    }

    fn wake_sync(&self) {
        if let Some(worker) = self
            .inbound
            .get()
            .and_then(|inbound| inbound.sync_thread.get())
        {
            worker.unpark();
        }
    }

    fn receive_delta(&self, peer: SocketAddrV4) -> InboundDelta<'_> {
        self.receiving_deltas
            .lock()
            .unwrap()
            .entry(peer)
            .or_insert_with(|| ReceivingDelta {
                count: 0,
                defer_until: Instant::now() + OVERLAP_DEFER,
            })
            .count += 1;
        InboundDelta { core: self, peer }
    }

    fn snapshot(&self) -> Arc<LocalState> {
        self.reader.snapshot().state
    }

    fn calendar_digest(&self, state: &LocalState) -> Fallible<[u8; 32]> {
        self.calendar_digest_with(state, None)
    }

    fn calendar_digest_with(
        &self,
        state: &LocalState,
        verified: Option<CachedDigest>,
    ) -> Fallible<[u8; 32]> {
        let workspace = state.workspace.as_ref().ok_or("Пространство недоступно.")?;
        let mut cache = self.digest.lock().unwrap();
        let observed = Arc::downgrade(workspace);
        if let Some(record) = cache.as_ref().filter(|r| r.workspace.ptr_eq(&observed)) {
            return Ok(record.digest);
        }
        let digest = match verified.filter(|r| r.workspace.ptr_eq(&observed)) {
            Some(record) => record.digest,
            None => calendar_digest(&workspace.calendar)?,
        };
        // An in-flight exchange may finish after a newer Workspace was published.
        // Check the live Arc while holding the digest lock, before replacing its cache.
        if self
            .snapshot()
            .workspace
            .as_ref()
            .is_some_and(|live| Arc::ptr_eq(live, workspace))
        {
            *cache = Some(CachedDigest {
                workspace: observed,
                digest,
            });
        }
        Ok(digest)
    }

    fn synchronized(&self, device: Id) {
        let mut connected = self.connected.lock().unwrap();
        let now = Instant::now();
        connected.retain(|_, seen| now - *seen < DISCOVERY_TTL);
        let became_online = connected.is_empty();
        if connected.len() < MAX_PEERS || connected.contains_key(&device) {
            connected.insert(device, now);
        }
        drop(connected);
        self.status.lock().unwrap().last_sync = Some(Utc::now().timestamp());
        if became_online {
            self.reader.repaint_window();
        }
    }

    fn exchanged(&self, device: Id, peer: SocketAddrV4) {
        if self.routes.lock().unwrap().remember(device, peer) {
            self.wake_sync();
        }
        self.merge_errors.lock().unwrap().remove(&peer);
        self.synchronized(device);
    }

    fn inbound_schedule(
        &self,
        now: Instant,
    ) -> MutexGuard<'_, BTreeMap<SocketAddrV4, InboundGrace>> {
        let mut incoming = self.inbound_until.lock().unwrap();
        if incoming.is_empty() {
            return incoming;
        }
        let state = self.snapshot();
        let generation = self.generation.load(Ordering::Acquire);
        incoming.retain(|_, entry| {
            entry.until > now
                && entry
                    .confirmed
                    .as_ref()
                    .is_none_or(|confirmed| confirmed.matches(&state, generation))
        });
        incoming
    }

    fn inbound_exchange(
        &self,
        own: Id,
        device: Id,
        peer: SocketAddrV4,
        grace: Duration,
        confirmed: Option<ConfirmedInbound>,
    ) {
        // Ordinary exchanges use the existing device ordering. A peer that
        // proved the current version needs no immediate reverse connection.
        let defer = match &confirmed {
            Some(exact) => exact.matches(&self.snapshot(), self.generation.load(Ordering::Acquire)),
            None => device < own,
        };
        if defer {
            let now = Instant::now();
            let mut inbound_until = self.inbound_schedule(now);
            if inbound_until.len() < MAX_PEERS || inbound_until.contains_key(&peer) {
                let until = now + grace;
                if inbound_until
                    .insert(peer, InboundGrace { until, confirmed })
                    .is_some_and(|old| until < old.until)
                {
                    self.wake_sync();
                }
            }
        }
        self.exchanged(device, peer);
    }

    fn record_merge_error(&self, peer: SocketAddrV4, error: &str) {
        let mut errors = self.merge_errors.lock().unwrap();
        if errors.len() < MAX_PEERS || errors.contains_key(&peer) {
            let readable = error
                .chars()
                .take(250)
                .map(|c| if c.is_control() { ' ' } else { c });
            errors.insert(peer, readable.collect());
        }
    }

    fn merge(&self, packet: SyncPacket) -> Fallible<()> {
        self.merge_in_scope(packet, None)
    }

    fn merge_in_scope(&self, packet: SyncPacket, scope: Option<delta::PeerScope>) -> Fallible<()> {
        self.merge_received(packet, scope, None).map(|_| ())
    }

    // Returns the committed Workspace only for a patch applied to its original base.
    // Callers still have to verify the touched records before reusing a peer digest.
    fn merge_received(
        &self,
        mut packet: SyncPacket,
        scope: Option<delta::PeerScope>,
        patch_base: Option<&Calendar>,
    ) -> Fallible<Option<Arc<Workspace>>> {
        self.require_available()?;
        let now = Utc::now().timestamp();
        validate_incoming_calendar(&packet.calendar, packet.device)?;
        let mut store = self.store.lock().unwrap();
        self.require_available()?;
        let current = store.snapshot();
        if let Some(scope) = scope {
            scope.check(&current)?;
        }
        let unchanged_base = match patch_base {
            Some(base) => std::ptr::eq(base, &workspace(&current)?.calendar),
            None => false,
        };
        if let Some(base) = patch_base.filter(|_| !unchanged_base) {
            // Another writer changed the base. Recreate the verified peer image
            // so the CRDT merge retains the same semantics as a full transfer.
            packet.calendar = delta::restore_plans(base, packet.calendar, None);
            validate_incoming_calendar(&packet.calendar, packet.device)?;
        }
        let changed = merge_validated_calendar(
            &mut store,
            packet.workspace,
            packet.device,
            packet.calendar,
            Some(now),
        )?;
        let committed = if unchanged_base {
            store.snapshot().workspace.clone()
        } else {
            None
        };
        drop(store);
        if changed {
            self.reader.repaint_window();
        }
        Ok(committed)
    }
}

fn validate_incoming_calendar(calendar: &Calendar, source: Id) -> Fallible<()> {
    calendar.validate()?;
    if !calendar.members.contains_key(&source) {
        return Err("В сообщении отсутствует участник.".into());
    }
    Ok(())
}

pub(crate) fn merge_calendar(
    store: &mut Store,
    workspace_id: Id,
    source: Id,
    calendar: Calendar,
    exchanged_at: Option<i64>,
) -> Fallible<bool> {
    validate_incoming_calendar(&calendar, source)?;
    merge_validated_calendar(store, workspace_id, source, calendar, exchanged_at)
}

fn merge_validated_calendar(
    store: &mut Store,
    workspace_id: Id,
    source: Id,
    calendar: Calendar,
    exchanged_at: Option<i64>,
) -> Fallible<bool> {
    let before = store.snapshot();
    let workspace = workspace(&before)?;
    workspace.require_device_access(source)?;
    let local = &workspace.calendar;
    if workspace.id != workspace_id {
        return Err("Устройства относятся к разным пространствам.".into());
    }
    if calendar.latest() > before.latest_revision().saturating_add(MAX_REVISION_LEAD) {
        return Err("Некорректная версия данных у другого устройства.".into());
    }
    let remaining = MAX_PLANS.saturating_sub(local.plans.len());
    if calendar.plans.len() > remaining
        && calendar
            .plans
            .keys()
            .filter(|id| !local.plans.contains_key(id))
            .nth(remaining)
            .is_some()
    {
        return Err(PLAN_LIMIT_ERROR.into());
    }

    let identical = &calendar == local;
    let mut last_sync = before.last_sync;
    if let Some(now) = exchanged_at {
        update_sync_time(&mut last_sync, now);
    }
    if identical && last_sync == before.last_sync {
        return Ok(false);
    }
    store.transaction(|state| {
        state.last_sync = last_sync;
        if identical {
            return Ok(false);
        }
        let calendar_to_update = &mut state.workspace_mut()?.calendar;
        calendar_to_update.merge(calendar);
        Ok(calendar_to_update != local)
    })
}

fn workspace(state: &LocalState) -> Fallible<&Workspace> {
    state
        .workspace
        .as_deref()
        .ok_or_else(|| "Пространство недоступно.".into())
}

fn peer_address(ip: Ipv4Addr, port: u16) -> Fallible<SocketAddrV4> {
    let peer = SocketAddrV4::new(ip, port);
    check_address(peer)?;
    Ok(peer)
}

fn update_sync_time(last: &mut Option<i64>, now: i64) {
    if last.is_none_or(|value| now < value || now.saturating_sub(value) >= 60) {
        *last = Some(now);
    }
}

fn primary_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.set_broadcast(true).ok()?;
    // connect выбирает локальный интерфейс, не отправляя дейтаграмму.
    socket.connect((Ipv4Addr::BROADCAST, DISCOVERY_PORT)).ok()?;
    match socket.local_addr().ok()? {
        SocketAddr::V4(address) if local_ip(*address.ip()) => Some(*address.ip()),
        _ => None,
    }
}

fn local_ip(ip: Ipv4Addr) -> bool {
    let [first, second, _, _] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || (first == 100 && (64..128).contains(&second))
}

fn check_address(address: SocketAddrV4) -> Fallible<()> {
    if local_ip(*address.ip()) && address.port() != 0 {
        Ok(())
    } else {
        Err("Укажите частный IPv4-адрес и порт, например 192.168.1.15:45736.".into())
    }
}

fn listen_forever(core: Arc<Core>, mut wait: Wait) {
    let inbound = core.inbound.get().unwrap();
    // Limit work per source and join handlers before releasing the profile.
    let mut workers: Vec<(Ipv4Addr, thread::JoinHandle<()>)> = Vec::new();
    while !core.stopped.load(Ordering::Acquire) {
        let mut index = 0;
        while index < workers.len() {
            if workers[index].1.is_finished() {
                let _ = workers.swap_remove(index).1.join();
            } else {
                index += 1;
            }
        }
        if !core.active() {
            inbound.wait_for_resume(&core);
            continue;
        }
        let (registered, result) = inbound.accept();
        match result {
            Ok((stream, SocketAddr::V4(address)))
                if core.active()
                    && core.permits_ip(*address.ip())
                    && workers.len() < 8
                    && workers.iter().filter(|(ip, _)| ip == address.ip()).count() < 3 =>
            {
                let core = Arc::clone(&core);
                let ip = *address.ip();
                workers.push((
                    ip,
                    thread::spawn(move || {
                        let _ = respond(&core, stream.into(), ip);
                    }),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait.until(registered, Duration::from_secs(30));
            }
            Err(_) => thread::park_timeout(Duration::from_millis(40)),
        }
    }
    for (_, worker) in workers {
        let _ = worker.join();
    }
}

#[derive(Serialize, Deserialize)]
struct SyncPacket<C = Calendar> {
    device: Id,
    workspace: Id,
    port: u16,
    calendar: C,
}

#[derive(Serialize, Deserialize)]
struct JoinRequest {
    #[serde(default)]
    enforces_managed_access: bool,
    device: Id,
    employee: String,
    proof: [u8; 32],
    port: u16,
}

#[derive(Serialize, Deserialize)]
struct JoinResponse<W = Workspace> {
    device: Id,
    port: u16,
    workspace: W,
}

pub(crate) fn validate_enrollment_size(device: Id, workspace: &Workspace) -> Fallible<()> {
    let envelope = Ok::<_, &str>(JoinResponse {
        device,
        port: u16::MAX,
        workspace,
    });
    let mut writer = LimitedWriter::new(io::sink(), MESSAGE_LIMIT);
    serde_json::to_writer(&mut writer, &envelope)
        .map_err(|_| "Календарь превышает предел передачи в 8 МиБ.")?;
    Ok(())
}

struct JoinCredentials {
    host_public: [u8; 32],
    host: Id,
    id: Id,
    key: Zeroizing<[u8; 32]>,
}

impl JoinCredentials {
    fn parse(code: &str) -> Fallible<(Self, Option<SocketAddrV4>)> {
        let code = code.trim();
        // IPv4 содержит точки. Читаем первые пять полей и оставляем адрес целиком.
        let mut fields = code.splitn(6, '.');
        if code.len() > INVITE_CODE_LIMIT || fields.next() != Some("yr2") {
            return Err(
                "Нужен адресный код приглашения от коллеги с версией 0.5.41 или новее.".into(),
            );
        }
        let host = Id::parse_str(fields.next().ok_or("В коде отсутствует устройство.")?)?;
        let id = Id::parse_str(fields.next().ok_or("В коде отсутствует приглашение.")?)?;
        let encoded = fields.next().ok_or("В коде отсутствует ключ.")?;
        if encoded.len() != INVITE_KEY_TEXT_LEN {
            return Err("Неверная длина ключа.".into());
        }
        let mut key = Zeroizing::new([0; 32]);
        let decoded_len = URL_SAFE_NO_PAD
            .decode_slice(encoded, &mut key[..])
            .map_err(|_| "Некорректный ключ приглашения.")?;
        if decoded_len != key.len() {
            return Err("Неверная длина ключа.".into());
        }
        let host_public =
            pairing::decode_public(fields.next().ok_or("Нет ключа приглашающего устройства.")?)?;
        let embedded = match fields.next().ok_or("Код приглашения неполный.")?
        {
            "-" => None,
            value => Some(value.parse::<SocketAddrV4>()?),
        };
        Ok((
            Self {
                host_public,
                host,
                id,
                key,
            },
            embedded,
        ))
    }
}

#[derive(Clone, Copy)]
struct InvitationFallback<'a> {
    host: Id,
    check_after: Instant,
    attempted: &'a BTreeSet<SocketAddrV4>,
}

fn valid_employee_name(name: &str) -> bool {
    !name.trim().is_empty() && name.chars().count() <= 80
}

fn join(
    core: &Core,
    code: Zeroizing<String>,
    employee: String,
    manual_address: Option<SocketAddrV4>,
    renewal: bool,
) -> Fallible<()> {
    core.require_operation(renewal)?;
    let (credentials, embedded) = JoinCredentials::parse(&code)?;
    drop(code);
    let host = credentials.host;
    let employee = employee.trim().to_owned();
    if !valid_employee_name(&employee) {
        return Err("Введите имя длиной от 1 до 80 символов.".into());
    }
    let (device, proof) = {
        let state = core.snapshot();
        if state.workspace.is_some() != renewal {
            return Err("Выберите подключение для текущего состояния профиля.".into());
        }
        (state.device, state.join_proof)
    };
    // access_job serializes join attempts. Only the active invitation may resolve its host.
    struct Pending<'a>(&'a Core);
    impl Drop for Pending<'_> {
        fn drop(&mut self) {
            self.0.pending_discovery.lock().unwrap().take();
        }
    }
    *core.pending_discovery.lock().unwrap() = Some((
        host,
        discovery::Key::derive(&credentials.key, false, credentials.id),
    ));
    let pending = Pending(core);
    let request = JoinRequest {
        enforces_managed_access: true,
        device,
        employee,
        proof,
        port: core.advertised_port(),
    };
    let deadline = Instant::now() + SESSION_LIMIT;
    let mut attempted = BTreeSet::new();
    let mut last_error = None;
    while Instant::now() < deadline && attempted.len() < 8 {
        core.require_operation(renewal)?;
        let discovered: Vec<_> = core
            .discovered
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(&address, seen)| {
                (seen.device == host && seen.time.elapsed() < DISCOVERY_TTL).then_some(address)
            })
            .collect();
        for address in manual_address.into_iter().chain(embedded).chain(discovered) {
            core.require_operation(renewal)?;
            if Instant::now() >= deadline || attempted.len() >= 8 {
                break;
            }
            if !attempted.insert(address) {
                continue;
            }
            let fallback = manual_address.is_none().then(|| InvitationFallback {
                host,
                check_after: Instant::now() + INVITE_PROBE_LIMIT,
                attempted: &attempted,
            });
            match redeem(core, address, &credentials, &request, deadline, fallback) {
                Ok(Ok(response)) => {
                    drop(pending);
                    drop(credentials);
                    core.require_operation(renewal)?;
                    response.workspace.require_device_access(device)?;
                    response.workspace.require_device_access(response.device)?;
                    response.workspace.require_manager(response.device)?;
                    if renewal && response.workspace.access_manager.is_none() {
                        return Err("Новый код должен выдать управляющий компьютер.".into());
                    }
                    if !response.workspace.valid_key_history() {
                        return Err("Недопустимая история ключей пространства.".into());
                    }
                    if response.workspace.calendar.latest() > u64::MAX / 2 {
                        return Err("Некорректная версия календаря.".into());
                    }
                    let peer = SocketAddrV4::new(*address.ip(), response.port);
                    check_address(peer)?;
                    if renewal {
                        let mut store = core.store.lock().unwrap();
                        core.require_operation(renewal)?;
                        store.accept_renewal(response.workspace, peer)?;
                        core.change_pause(true)?;
                        return Ok(());
                    }
                    core.store.lock().unwrap().transaction(|state| {
                        core.require_operation(renewal)?;
                        if state.workspace.is_some() {
                            return Err("Профиль уже подключён к пространству.".into());
                        }
                        state.workspace = Some(Arc::new(response.workspace));
                        state.peers.insert(peer);
                        state.last_sync = Some(Utc::now().timestamp());
                        Ok(())
                    })?;
                    core.exchanged(response.device, peer);
                    core.wake_sync();
                    return Ok(());
                }
                Ok(Err(reason)) => return Err(reason.into()),
                Err(error) => last_error = Some(error),
            }
        }
        if manual_address.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    let detail = last_error.map_or_else(String::new, |error| format!(" {error}"));
    Err(format!("Подключение не удалось. Проверьте адрес, сеть и срок кода.{detail}").into())
}

fn redeem(
    core: &Core,
    address: SocketAddrV4,
    credentials: &JoinCredentials,
    request: &JoinRequest,
    deadline: Instant,
    fallback: Option<InvitationFallback<'_>>,
) -> Fallible<Result<JoinResponse, String>> {
    let (mut stream, mut transport) = connect(
        core,
        address,
        Operation::Join,
        credentials.id,
        ChannelKey::Invitation {
            secret: &credentials.key,
            peer: &credentials.host_public,
        },
        deadline,
        fallback,
    )?;
    if fallback.is_some() {
        // The peer has proved it knows the invitation key. Give the calendar its own window.
        stream.deadline = Instant::now() + SESSION_LIMIT;
    }
    send_secure(&mut stream, &mut transport, request)?;
    let response = read_secure::<Result<JoinResponse, String>>(&mut stream, &mut transport)?;
    if let Ok(joined) = &response
        && (joined.device != credentials.host
            || !joined
                .workspace
                .calendar
                .members
                .contains_key(&request.device))
    {
        return Err("Приглашение принадлежит другому устройству или не подтверждено.".into());
    }
    Ok(response)
}

#[derive(Debug, Eq, PartialEq)]
enum SyncPace {
    Regular,
    QuietMatch,
}

fn sync_peer(
    core: &Core,
    address: SocketAddrV4,
    protocol: &mut PeerProtocol,
) -> Fallible<SyncPace> {
    core.require_available()?;
    let state = core.snapshot();
    let workspace = workspace(&state)?;
    let deadline = Instant::now() + SESSION_LIMIT;
    if !matches!(protocol, PeerProtocol::LegacyUntil(until) if *until > Instant::now()) {
        match connect(
            core,
            address,
            Operation::DigestSync,
            workspace.id,
            ChannelKey::Group(&workspace.key),
            deadline,
            None,
        ) {
            Ok((mut stream, mut transport)) => {
                *protocol = PeerProtocol::Digest;
                let peer_wakes_on_edit = stream.capabilities.contains(Capabilities::EDIT_WAKE);
                let same_calendar = if stream.capabilities.contains(Capabilities::DELTA) {
                    let capabilities = stream.capabilities;
                    delta::exchange(
                        core,
                        &mut stream,
                        &mut transport,
                        &state,
                        *address.ip(),
                        delta::Role::Initiator,
                        capabilities,
                    )?
                } else {
                    sync_digest(core, &mut stream, &mut transport, &state, *address.ip())?
                };
                return Ok(if peer_wakes_on_edit && same_calendar {
                    SyncPace::QuietMatch
                } else {
                    SyncPace::Regular
                });
            }
            Err(error) if !matches!(protocol, PeerProtocol::Digest) && error.is::<LegacyPeer>() => {
            }
            Err(error) => return Err(error),
        }
    }
    if workspace.access_manager.is_some() {
        return Err("Для этого календаря нужен современный протокол синхронизации.".into());
    }
    let (mut stream, mut transport) = connect(
        core,
        address,
        Operation::LegacySync,
        workspace.id,
        ChannelKey::Group(&workspace.key),
        deadline,
        None,
    )?;
    if !matches!(protocol, PeerProtocol::LegacyUntil(until) if *until > Instant::now()) {
        *protocol = PeerProtocol::LegacyUntil(Instant::now() + LEGACY_RECHECK);
    }
    send_secure(
        &mut stream,
        &mut transport,
        &legacy_packet(&state, core.advertised_port())?,
    )?;
    let packet: SyncPacket = read_secure(&mut stream, &mut transport)?;
    let (device, peer) = merge_legacy_packet(core, packet, state.device, *address.ip())?;
    core.exchanged(device, peer);
    Ok(SyncPace::Regular)
}

#[derive(Debug)]
struct LegacyPeer(io::Error);

impl std::fmt::Display for LegacyPeer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for LegacyPeer {}

fn before_handshake_response(
    error: io::Error,
    kind: Operation,
) -> Box<dyn std::error::Error + Send + Sync> {
    if kind == Operation::DigestSync
        && matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::BrokenPipe
        )
    {
        Box::new(LegacyPeer(error))
    } else {
        Box::new(error)
    }
}

fn connect_from(
    local: Ipv4Addr,
    address: SocketAddrV4,
    timeout: Duration,
) -> io::Result<TcpStream> {
    if local.is_unspecified() {
        return TcpStream::connect_timeout(&SocketAddr::V4(address), timeout);
    }
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket.bind(&SocketAddrV4::new(local, 0).into())?;
    socket.connect_timeout(&address.into(), timeout)?;
    Ok(socket.into())
}

fn connect<'a>(
    core: &'a Core,
    address: SocketAddrV4,
    kind: Operation,
    id: Id,
    key: ChannelKey<'_>,
    deadline: Instant,
    fallback: Option<InvitationFallback<'a>>,
) -> Fallible<(Connection<'a>, TransportState)> {
    let renewing = kind == Operation::Join && core.snapshot().workspace.is_some();
    core.require_operation(renewing)?;
    core.check_peer(address)?;
    let generation = core.generation.load(Ordering::Acquire);
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("Истекло время сетевого обмена".into());
    }
    let tcp = {
        let _dial = core.dialing.lock().unwrap();
        core.require_operation(renewing)?;
        let timeout = remaining.min(Duration::from_millis(700));
        connect_from(*core.listen.ip(), address, timeout)?
    };
    let mut stream = Connection::new(core, tcp, generation, deadline)?;
    stream.allow_paused_renewal = renewing;
    stream.invitation_fallback = fallback;
    let header = header(kind, id);
    stream
        .write_all(&header)
        .map_err(|error| before_handshake_response(error, kind))?;
    let mut session = key.builder(&core.pairing, &header)?.build_initiator()?;
    let mut buffer = [0; 256];
    let offered = if kind == Operation::DigestSync {
        DIGEST_CAPABILITIES
    } else {
        Capabilities::NONE
    };
    let offer = offered.offer();
    let payload = if offered == Capabilities::NONE {
        &[][..]
    } else {
        &offer
    };
    let length = session.write_message(payload, &mut buffer)?;
    write_frame(&mut stream, &buffer[..length])
        .map_err(|error| before_handshake_response(error, kind))?;
    let frame = read_frame(&mut stream, buffer.len())
        .map_err(|error| before_handshake_response(error, kind))?;
    let n = session.read_message(&frame, &mut buffer)?;
    stream.capabilities = offered.negotiate(&buffer[..n]);
    stream.invitation_fallback = None;
    Ok((stream, session.into_transport_mode()?))
}

fn respond(core: &Core, tcp: TcpStream, ip: Ipv4Addr) -> Fallible<()> {
    let mut stream = Connection::new(
        core,
        tcp,
        core.generation.load(Ordering::Acquire),
        Instant::now() + SESSION_LIMIT,
    )?;
    let mut header = [0; 25];
    stream.read_exact(&mut header)?;
    if header[..8] != MAGIC {
        return Err("Версия протокола не поддерживается.".into());
    }
    let id = Id::from_slice(&header[9..])?;
    let operation = Operation::try_from(header[8])?;
    let mut session = match operation {
        Operation::LegacySync | Operation::DigestSync => {
            let (_, workspace, key) = core.sync_context()?;
            if id != workspace {
                return Err("Запрошено другое пространство.".into());
            }
            ChannelKey::Group(&key)
                .builder(&core.pairing, &header)?
                .build_responder()?
        }
        Operation::Join => {
            let invites = core.invites.lock().unwrap();
            let invite = invites
                .get(&id)
                .filter(|invite| invite.expires > Instant::now())
                .ok_or("Приглашение просрочено.")?;
            ChannelKey::Invitation {
                secret: &invite.key,
                peer: &invite.recipient,
            }
            .builder(&core.pairing, &header)?
            .build_responder()?
        }
    };
    let mut buffer = [0; 256];
    let frame = read_frame(&mut stream, buffer.len())?;
    let n = session.read_message(&frame, &mut buffer)?;
    if operation == Operation::DigestSync {
        stream.capabilities = DIGEST_CAPABILITIES.negotiate(&buffer[..n]);
    }
    let response = stream.capabilities.offer();
    let payload = if stream.capabilities == Capabilities::NONE {
        &[][..]
    } else {
        &response
    };
    let length = session.write_message(payload, &mut buffer)?;
    write_frame(&mut stream, &buffer[..length])?;
    let mut transport = session.into_transport_mode()?;
    match operation {
        Operation::Join => {
            let request = read_secure(&mut stream, &mut transport)?;
            let response = register(core, id, request, ip).map_err(|error| error.to_string());
            send_secure(&mut stream, &mut transport, &response)
        }
        Operation::LegacySync => {
            if workspace(&core.snapshot())?.access_manager.is_some() {
                return Err("Для этого календаря нужен современный протокол синхронизации.".into());
            }
            let request: SyncPacket = read_secure(&mut stream, &mut transport)?;
            let (device, peer) = merge_legacy_packet(core, request, core.snapshot().device, ip)?;
            let state = core.snapshot();
            send_secure(
                &mut stream,
                &mut transport,
                &legacy_packet(&state, core.advertised_port())?,
            )?;
            core.inbound_exchange(state.device, device, peer, RECENT_INBOUND, None);
            Ok(())
        }
        Operation::DigestSync if stream.capabilities.contains(Capabilities::DELTA) => {
            let edit_wake = stream.capabilities.contains(Capabilities::EDIT_WAKE);
            let capabilities = stream.capabilities;
            delta::exchange(
                core,
                &mut stream,
                &mut transport,
                &core.snapshot(),
                ip,
                delta::Role::Responder { edit_wake },
                capabilities,
            )
            .map(|_| ())
        }
        Operation::DigestSync => respond_digest(core, &mut stream, &mut transport, ip),
    }
}

fn merge_legacy_packet(
    core: &Core,
    packet: SyncPacket,
    own_device: Id,
    ip: Ipv4Addr,
) -> Fallible<(Id, SocketAddrV4)> {
    let device = packet.device;
    if device == own_device {
        return Err("Это адрес текущего устройства.".into());
    }
    let peer = peer_address(ip, packet.port)?;
    core.merge(packet)
        .inspect_err(|error| core.record_merge_error(peer, &error.to_string()))?;
    Ok((device, peer))
}

fn legacy_packet(state: &LocalState, port: u16) -> Fallible<SyncPacket<&Calendar>> {
    let workspace = workspace(state)?;
    Ok(SyncPacket {
        device: state.device,
        workspace: workspace.id,
        port,
        calendar: &workspace.calendar,
    })
}

#[derive(Clone, Copy)]
struct SyncHello {
    device: Id,
    port: u16,
    digest: [u8; 32],
}

impl SyncHello {
    const LEN: usize = SYNC_HELLO_LEN;

    fn new(core: &Core, state: &Arc<LocalState>) -> Fallible<Self> {
        Ok(Self {
            device: state.device,
            port: core.advertised_port(),
            digest: core.calendar_digest(state)?,
        })
    }

    fn send(self, stream: &mut impl Write, transport: &mut TransportState) -> Fallible<()> {
        let mut plain = [0; Self::LEN];
        plain[..16].copy_from_slice(self.device.as_bytes());
        plain[16..18].copy_from_slice(&self.port.to_be_bytes());
        plain[18..].copy_from_slice(&self.digest);
        send_control(stream, transport, &plain)
    }

    fn read(stream: &mut impl Read, transport: &mut TransportState, own: Id) -> Fallible<Self> {
        let plain = read_control::<{ Self::LEN }>(stream, transport)?;
        let device = Id::from_slice(&plain[..16])?;
        if device == own {
            return Err("Это адрес текущего устройства.".into());
        }
        Ok(Self {
            device,
            port: u16::from_be_bytes(plain[16..18].try_into()?),
            digest: plain[18..].try_into()?,
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
enum Receipt {
    Current = 1,
    SnapshotFollows = 2,
    Rejected = 3,
}

impl Receipt {
    fn read(stream: &mut impl Read, transport: &mut TransportState) -> Fallible<Self> {
        match read_control::<1>(stream, transport)?[0] {
            1 => Ok(Self::Current),
            2 => Ok(Self::SnapshotFollows),
            3 => Ok(Self::Rejected),
            _ => Err("Неизвестное подтверждение обмена.".into()),
        }
    }

    fn send(self, stream: &mut impl Write, transport: &mut TransportState) -> Fallible<()> {
        send_control(stream, transport, &[self as u8])
    }
}

fn finish_remote_rejection<T>(core: &Core, peer: SocketAddrV4) -> Fallible<T> {
    let error = "Устройство отклонило календарь. Проверьте окно синхронизации на нём.";
    let mut errors = core.merge_errors.lock().unwrap();
    // Simultaneous sessions may also have diagnosed the exact failure locally.
    if errors.len() < MAX_PEERS {
        errors.entry(peer).or_insert_with(|| error.into());
    }
    Err(error.into())
}

fn sync_digest(
    core: &Core,
    stream: &mut (impl Read + Write),
    transport: &mut TransportState,
    state: &Arc<LocalState>,
    ip: Ipv4Addr,
) -> Fallible<bool> {
    let local = SyncHello::new(core, state)?;
    let workspace = workspace(state)?;
    local.send(stream, transport)?;
    let remote = SyncHello::read(stream, transport, local.device)?;
    workspace.require_device_access(remote.device)?;
    let peer = peer_address(ip, remote.port)?;
    if remote.digest != local.digest {
        send_calendar(stream, transport, &workspace.calendar)?;
    }
    match Receipt::read(stream, transport)? {
        Receipt::Rejected => return finish_remote_rejection(core, peer),
        Receipt::Current => {
            if !workspace.calendar.members.contains_key(&remote.device) {
                return Err("В сообщении отсутствует участник.".into());
            }
        }
        Receipt::SnapshotFollows if remote.digest != local.digest => {
            accept_calendar(core, stream, transport, remote, workspace.id, peer)?;
            Receipt::Current.send(stream, transport)?;
        }
        Receipt::SnapshotFollows => return Err("Получено неожиданное подтверждение обмена.".into()),
    }
    core.exchanged(remote.device, peer);
    Ok(remote.digest == local.digest)
}

fn respond_digest(
    core: &Core,
    stream: &mut Connection<'_>,
    transport: &mut TransportState,
    ip: Ipv4Addr,
) -> Fallible<()> {
    let state = core.snapshot();
    let local = SyncHello::new(core, &state)?;
    let workspace = workspace(&state)?;
    let remote = SyncHello::read(stream, transport, local.device)?;
    workspace.require_device_access(remote.device)?;
    let peer = peer_address(ip, remote.port)?;
    local.send(stream, transport)?;
    if remote.digest == local.digest {
        if !workspace.calendar.members.contains_key(&remote.device) {
            Receipt::Rejected.send(stream, transport)?;
            return Err("В сообщении отсутствует участник.".into());
        }
        Receipt::Current.send(stream, transport)?;
    } else {
        accept_calendar(core, stream, transport, remote, workspace.id, peer)?;
        let merged = core.snapshot();
        if core.calendar_digest(&merged)? == remote.digest {
            Receipt::Current.send(stream, transport)?;
        } else {
            Receipt::SnapshotFollows.send(stream, transport)?;
            send_calendar(stream, transport, &workspace.calendar)?;
            match Receipt::read(stream, transport)? {
                Receipt::Rejected => return finish_remote_rejection(core, peer),
                Receipt::Current => {}
                Receipt::SnapshotFollows => {
                    return Err("Получено неожиданное подтверждение обмена.".into());
                }
            }
        }
    }
    let grace =
        if stream.capabilities.contains(Capabilities::EDIT_WAKE) && remote.digest == local.digest {
            QUIET_INBOUND
        } else {
            RECENT_INBOUND
        };
    core.inbound_exchange(local.device, remote.device, peer, grace, None);
    Ok(())
}

fn accept_calendar(
    core: &Core,
    stream: &mut (impl Read + Write),
    transport: &mut TransportState,
    remote: SyncHello,
    workspace: Id,
    peer: SocketAddrV4,
) -> Fallible<()> {
    let result = read_calendar(stream, transport, remote.digest).and_then(|calendar| {
        core.merge(SyncPacket {
            device: remote.device,
            workspace,
            port: remote.port,
            calendar,
        })
    });
    reject_failed_snapshot(core, stream, transport, peer, result)
}

fn reject_failed_snapshot<T>(
    core: &Core,
    stream: &mut impl Write,
    transport: &mut TransportState,
    peer: SocketAddrV4,
    result: Fallible<T>,
) -> Fallible<T> {
    result.inspect_err(|error| {
        core.record_merge_error(peer, &error.to_string());
        let _ = Receipt::Rejected.send(stream, transport);
    })
}

fn register(
    core: &Core,
    id: Id,
    request: JoinRequest,
    ip: Ipv4Addr,
) -> Fallible<JoinResponse<Arc<Workspace>>> {
    core.require_available()?;
    let mut invites = core.invites.lock().unwrap();
    let invite = invites
        .get_mut(&id)
        .filter(|invite| invite.expires > Instant::now())
        .ok_or("Приглашение просрочено.")?;
    if invite.device != request.device {
        return Err("Код выдан другому устройству.".into());
    }
    let claimant = (request.device, request.proof);
    if invite.used_by.is_some_and(|user| user != claimant) {
        return Err("Приглашение уже использовано.".into());
    }
    if !valid_employee_name(&request.employee) {
        return Err("Указано неверное имя.".into());
    }
    let peer = SocketAddrV4::new(ip, request.port);
    check_address(peer)?;
    let (response, new_member) = core.store.lock().unwrap().transaction(|state| {
        core.require_available()?;
        let device = state.device;
        if device == request.device {
            return Err("Это текущее устройство.".into());
        }
        let workspace = state.workspace.as_ref().ok_or("Пространство недоступно.")?;
        workspace.require_device_access(request.device)?;
        workspace.require_manager(state.device)?;
        if workspace.access_manager.is_some() && !request.enforces_managed_access {
            return Err(
                "Для этого календаря обновите приложение с поддержкой отзыва устройств.".into(),
            );
        }
        let grant = Id::new_v5(&workspace.id, &request.proof);
        let known_member = workspace.calendar.members.contains_key(&request.device);
        if known_member {
            // A lost response can be recovered with a fresh invitation from the same device.
            let known = state.enrollment_grants.get(&request.device);
            let same_claimant = invite.used_by == Some(claimant)
                || known.is_none_or(|known| bool::from(known.as_bytes().ct_eq(grant.as_bytes())));
            if !same_claimant {
                return Err("Этот идентификатор уже зарегистрирован.".into());
            }
        } else {
            let revision = state.revision()?;
            state
                .workspace_mut()?
                .calendar
                .members
                .insert(request.device, Record::new(revision, request.employee));
        }
        if state
            .workspace
            .as_ref()
            .is_some_and(|w| w.access_manager == Some(device))
        {
            state.admitted_this_generation.insert(request.device);
        }
        state.enrollment_grants.insert(request.device, grant);
        let workspace = state.workspace.as_ref().expect("workspace was checked");
        let response = JoinResponse {
            device,
            port: core.advertised_port(),
            workspace: workspace.clone(),
        };
        update_sync_time(&mut state.last_sync, Utc::now().timestamp());
        Ok((response, !known_member))
    })?;
    invite.used_by = Some(claimant);
    drop(invites);
    if new_member {
        core.reader.repaint_window();
    }
    core.exchanged(request.device, peer);
    core.wake_sync();
    Ok(response)
}

struct Connection<'a> {
    core: &'a Core,
    tcp: TcpStream,
    capabilities: Capabilities,
    allow_paused_renewal: bool,
    deadline: Instant,
    generation: usize,
    invitation_fallback: Option<InvitationFallback<'a>>,
}

impl<'a> Connection<'a> {
    fn new(
        core: &'a Core,
        tcp: TcpStream,
        generation: usize,
        deadline: Instant,
    ) -> io::Result<Self> {
        // Accepted sockets can inherit nonblocking mode from the listener on BSD.
        tcp.set_nonblocking(false)?;
        tcp.set_nodelay(true)?;
        Ok(Self {
            core,
            tcp,
            deadline,
            generation,
            invitation_fallback: None,
            capabilities: Capabilities::NONE,
            allow_paused_renewal: false,
        })
    }

    fn timeout(&self) -> io::Result<Duration> {
        if !self.core.permits_operation(self.allow_paused_renewal)
            || self.core.generation.load(Ordering::Acquire) != self.generation
        {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Синхронизация остановлена",
            ));
        }
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Истекло время сетевого обмена",
            ));
        }
        if let Some(fallback) = self.invitation_fallback
            && Instant::now() >= fallback.check_after
            && self
                .core
                .discovered
                .lock()
                .unwrap()
                .iter()
                .any(|(address, seen)| {
                    seen.device == fallback.host
                        && seen.time.elapsed() < DISCOVERY_TTL
                        && !fallback.attempted.contains(address)
                })
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Автопоиск обнаружил другой адрес устройства",
            ));
        }
        Ok(remaining.min(IO_POLL))
    }

    fn io<T>(
        &mut self,
        mut op: impl FnMut(&mut TcpStream, Duration) -> io::Result<T>,
    ) -> io::Result<T> {
        loop {
            let timeout = self.timeout()?;
            match op(&mut self.tcp, timeout) {
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) => {}
                result => return result,
            }
        }
    }
}

impl Read for Connection<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.io(|tcp, timeout| {
            set_io_timeout(tcp, timeout, TcpStream::set_read_timeout)?;
            tcp.read(output)
        })
    }
}

impl Write for Connection<'_> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.io(|tcp, timeout| {
            set_io_timeout(tcp, timeout, TcpStream::set_write_timeout)?;
            tcp.write(input)
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

fn set_io_timeout(
    tcp: &TcpStream,
    timeout: Duration,
    set_timeout: fn(&TcpStream, Option<Duration>) -> io::Result<()>,
) -> io::Result<()> {
    set_timeout(tcp, Some(timeout)).map_err(|error| {
        // Darwin can reject setsockopt after a peer reset, before read or write sees it.
        #[cfg(target_os = "macos")]
        if error.kind() == io::ErrorKind::InvalidInput
            && let Ok(Some(cause)) = tcp.take_error()
            && cause.kind() == io::ErrorKind::ConnectionReset
        {
            return cause;
        }
        error
    })
}

#[cfg(test)]
mod tests;
