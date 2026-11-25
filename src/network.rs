use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use blake2::{Blake2s256, Digest};
use chrono::Utc;
use flate2::{Compression, bufread::ZlibDecoder, write::ZlibEncoder};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use snow::{Builder, TransportState};
use socket2::{Domain, Protocol, Socket, Type};
use subtle::ConstantTimeEq;

use crate::{
    Fallible,
    model::{Calendar, Id, MAX_PLANS, PLAN_LIMIT_ERROR, Record},
    storage::{
        CALENDAR_MESSAGE_LIMIT, LimitedWriter, LocalState, PeerRoutes, SharedStore, Store,
        StoreReader, Workspace, random_bytes,
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

pub const SYNC_PORT: u16 = 45736;
pub const DISCOVERY_PORT: u16 = 45737;
const MAGIC: [u8; 8] = *b"YRCAL001";
const NOISE: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const FRAME: usize = 60_000;
const MESSAGE_LIMIT: usize = CALENDAR_MESSAGE_LIMIT;
const MAX_PEERS: usize = 64;
const MAX_REVISION_LEAD: u64 = 1_000_000;
const SESSION_LIMIT: Duration = Duration::from_secs(6);
const INVITE_PROBE_LIMIT: Duration = Duration::from_millis(1200);
const IO_POLL: Duration = Duration::from_millis(400);
const DISCOVERY_TTL: Duration = Duration::from_secs(20);
const INVITE_TTL: Duration = Duration::from_secs(15 * 60);
const LEGACY_RECHECK: Duration = Duration::from_secs(60);
// Advertised only inside authenticated digest-sync handshakes. Older peers
// ignore the optional payload and retain their original polling cadence.
const DIGEST_CAPABILITIES: Capabilities = Capabilities::EDIT_WAKE.union(Capabilities::DELTA);
const QUIET_SYNC_INTERVAL: Duration = Duration::from_secs(9);
const QUIET_INBOUND: Duration = Duration::from_secs(11);
const RECENT_INBOUND: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
enum Operation {
    LegacySync = 1,
    Join = 2,
    DigestSync = 3,
}

impl TryFrom<u8> for Operation {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::LegacySync),
            2 => Ok(Self::Join),
            3 => Ok(Self::DigestSync),
            _ => Err("Неизвестный тип подключения."),
        }
    }
}

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
    key: [u8; 32],
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
    inbound_until: Mutex<BTreeMap<SocketAddrV4, Instant>>,
    status: Mutex<Status>,
    digest: Mutex<Option<CachedDigest>>,
    delta_baselines: delta::Baselines,
    merge_errors: Mutex<BTreeMap<SocketAddrV4, String>>,
}

struct CachedDigest {
    workspace: Weak<Workspace>,
    digest: [u8; 32],
}

pub struct Network {
    core: Arc<Core>,
    workers: Vec<thread::JoinHandle<()>>,
    joining: Mutex<Option<thread::JoinHandle<()>>>,
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
            workers.push(thread::spawn(move || discover(core, udp, port)));
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
        status.recent_peers = self
            .core
            .connected
            .lock()
            .unwrap()
            .values()
            .filter(|seen| seen.elapsed() < DISCOVERY_TTL)
            .count();
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
        let key = random_bytes()?;
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
        Ok(format!(
            "yr2.{device}.{id}.{}.{}.{address}",
            URL_SAFE_NO_PAD.encode(key),
            self.core.pairing.encoded_public()
        ))
    }

    pub fn join_async(
        &self,
        code: String,
        employee: String,
        address: Option<SocketAddrV4>,
    ) -> Receiver<Fallible<()>> {
        self.access_job(move |core| join(core, &code, employee, address, false))
    }

    pub fn rejoin_async(
        &self,
        code: String,
        address: Option<SocketAddrV4>,
    ) -> Receiver<Fallible<()>> {
        let state = self.core.snapshot();
        let name = state
            .workspace
            .as_ref()
            .and_then(|w| w.calendar.members.get(&state.device))
            .map(|m| m.value.clone())
            .unwrap_or_default();
        self.access_job(move |core| {
            core.pause_for_renewal()?;
            join(core, &code, name, address, true)
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
        if joining.as_ref().is_some_and(|job| !job.is_finished()) {
            let _ = send.send(Err("Подключение уже выполняется.".into()));
            return receive;
        }
        if let Some(previous) = joining.take() {
            let _ = previous.join();
        }
        *joining = Some(thread::spawn(move || {
            let result = work(&core);
            let _ = send.send(result);
        }));
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
        if let Some(worker) = self.joining.get_mut().unwrap().take() {
            let _ = worker.join();
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

    fn snapshot(&self) -> Arc<LocalState> {
        self.reader.snapshot().state
    }

    fn calendar_digest(&self, state: &LocalState) -> Fallible<[u8; 32]> {
        let workspace = state.workspace.as_ref().ok_or("Пространство недоступно.")?;
        let mut cache = self.digest.lock().unwrap();
        let observed = Arc::downgrade(workspace);
        if let Some(record) = cache.as_ref().filter(|r| r.workspace.ptr_eq(&observed)) {
            return Ok(record.digest);
        }
        let digest = calendar_digest(&workspace.calendar)?;
        *cache = Some(CachedDigest {
            workspace: observed,
            digest,
        });
        Ok(digest)
    }

    fn synchronized(&self, device: Id) {
        let mut connected = self.connected.lock().unwrap();
        connected.retain(|_, seen| seen.elapsed() < DISCOVERY_TTL);
        if connected.len() < MAX_PEERS || connected.contains_key(&device) {
            connected.insert(device, Instant::now());
        }
        drop(connected);
        self.status.lock().unwrap().last_sync = Some(Utc::now().timestamp());
    }

    fn exchanged(&self, device: Id, peer: SocketAddrV4) {
        if self.routes.lock().unwrap().remember(device, peer) {
            self.wake_sync();
        }
        self.merge_errors.lock().unwrap().remove(&peer);
        self.synchronized(device);
    }

    fn inbound_exchange(&self, own: Id, device: Id, peer: SocketAddrV4, grace: Duration) {
        // The lower device ID prefers initiating once an incoming session has
        // proved the path works. The other side can still dial when it goes quiet.
        if device < own {
            let now = Instant::now();
            let mut inbound_until = self.inbound_until.lock().unwrap();
            inbound_until.retain(|_, expiry| *expiry > now);
            if inbound_until.len() < MAX_PEERS || inbound_until.contains_key(&peer) {
                let expiry = now + grace;
                if inbound_until
                    .insert(peer, expiry)
                    .is_some_and(|old| expiry < old)
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
        self.require_available()?;
        let now = Utc::now().timestamp();
        validate_incoming_calendar(&packet.calendar, packet.device)?;
        let mut store = self.store.lock().unwrap();
        self.require_available()?;
        merge_validated_calendar(
            &mut store,
            packet.workspace,
            packet.device,
            packet.calendar,
            Some(now),
        )?;
        Ok(())
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

fn discovery_socket(ip: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_broadcast(ip.is_unspecified())?;
    socket.bind(&SocketAddrV4::new(ip, port).into())?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

fn discover(core: Arc<Core>, mut wait: Wait, port: u16) {
    let inbound = core.inbound.get().unwrap();
    let mut announce_at = Instant::now();
    // One extra byte distinguishes oversized/truncated packets.
    let mut packet = [0; discovery::LENGTH + 1];
    while !core.stopped.load(Ordering::Relaxed) {
        if !core.active() {
            inbound.wait_for_resume(&core);
            continue;
        }
        if Instant::now() >= announce_at {
            announce_at = Instant::now() + Duration::from_secs(3);
            let state = core.snapshot();
            if let Some(team) = &state.workspace {
                let mut keys = vec![discovery::Key::derive(&team.key, true, team.id)];
                let invites = core.invites.lock().unwrap();
                keys.extend(
                    invites
                        .iter()
                        .filter(|(_, invite)| invite.expires > Instant::now())
                        .map(|(id, invite)| discovery::Key::derive(&invite.key, false, *id)),
                );
                drop(invites);
                for key in keys {
                    if !core.active() {
                        break;
                    }
                    if let Some(message) = key.seal(
                        state.device,
                        team.id,
                        core.advertised_port(),
                        Utc::now().timestamp(),
                    ) {
                        inbound.announce(&message, port);
                    }
                }
            }
        }
        let (registered, result) = inbound.receive(&mut packet);
        let source = match result {
            Ok((discovery::LENGTH, SocketAddr::V4(source))) if core.active() => source,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                wait.until(
                    registered,
                    announce_at.saturating_duration_since(Instant::now()),
                );
                continue;
            }
            Err(_) => {
                thread::park_timeout(Duration::from_millis(40));
                continue;
            }
            _ => continue,
        };
        if !core.active() || !core.permits_ip(*source.ip()) {
            continue;
        }
        let state = core.snapshot();
        let now = Utc::now().timestamp();
        let packet = &packet[..discovery::LENGTH];
        let hint = state
            .workspace
            .as_ref()
            .and_then(|team| {
                discovery::Key::derive(&team.key, true, team.id)
                    .open(packet, now)
                    .filter(|(_, group, _)| *group == team.id)
            })
            .or_else(|| {
                let pending = core.pending_discovery.lock().unwrap();
                let (host, key) = pending.as_ref()?;
                key.open(packet, now).filter(|(from, _, _)| from == host)
            });
        let Some((from, remote_workspace, listen_port)) = hint else {
            continue;
        };
        if from == state.device {
            continue;
        }
        let address = SocketAddrV4::new(*source.ip(), listen_port);
        let mut seen = core.discovered.lock().unwrap();
        let new_route = seen.get(&address).is_none_or(|old| {
            old.workspace != remote_workspace || old.time.elapsed() >= DISCOVERY_TTL
        });
        remember_discovery(
            &mut seen,
            address,
            Seen {
                device: from,
                workspace: remote_workspace,
                time: Instant::now(),
            },
        );
        if new_route && seen.contains_key(&address) {
            core.wake_sync();
        }
    }
}

fn remember_discovery(
    seen: &mut BTreeMap<SocketAddrV4, Seen>,
    address: SocketAddrV4,
    beacon: Seen,
) {
    seen.retain(|previous, record| {
        let same_device = previous.ip() == address.ip()
            && record.device == beacon.device
            && record.workspace == beacon.workspace;
        !same_device && beacon.time.saturating_duration_since(record.time) < DISCOVERY_TTL
    });
    let other_ports = seen.keys().filter(|key| key.ip() == address.ip()).count();
    if seen.contains_key(&address) || (seen.len() < MAX_PEERS && other_ports < 3) {
        seen.insert(address, beacon);
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

#[derive(Clone, Copy, Default)]
enum PeerProtocol {
    #[default]
    Unknown,
    LegacyUntil(Instant),
    Digest,
}

struct Backoff {
    next: Instant,
    last_attempt: Instant,
    protocol: PeerProtocol,
    failures: u8,
    quiet_matches: u8,
}

impl Backoff {
    fn record_exchange(&mut self, result: Fallible<SyncPace>, now: Instant) {
        self.quiet_matches = if matches!(result, Ok(SyncPace::QuietMatch)) {
            self.quiet_matches.saturating_add(1)
        } else {
            0
        };
        self.failures = if result.is_ok() {
            0
        } else {
            (self.failures + 1).min(3)
        };
        let interval = if self.quiet_matches >= 2 {
            QUIET_SYNC_INTERVAL
        } else {
            Duration::from_secs(3 << self.failures.saturating_sub(1))
        };
        self.last_attempt = now;
        self.next = self.last_attempt + interval;
    }
}

fn next_peer(
    peers: &BTreeSet<SocketAddrV4>,
    preferred: &BTreeSet<SocketAddrV4>,
    attempts: &BTreeMap<SocketAddrV4, Backoff>,
    inbound_until: &BTreeMap<SocketAddrV4, Instant>,
    now: Instant,
) -> Option<SocketAddrV4> {
    peers
        .iter()
        .copied()
        .filter(|peer| inbound_until.get(peer).is_none_or(|expiry| *expiry <= now))
        .filter(|peer| attempts.get(peer).is_none_or(|retry| retry.next <= now))
        .min_by_key(|peer| {
            (
                attempts.get(peer).map(|retry| retry.next),
                !preferred.contains(peer),
            )
        })
}

fn sync_forever(core: Arc<Core>) {
    let _ = core
        .inbound
        .get()
        .unwrap()
        .sync_thread
        .set(thread::current());
    let inbound = core.inbound.get().unwrap();
    let calendar_wake = core.store.lock().unwrap().subscribe_calendar();
    let mut attempts = BTreeMap::<SocketAddrV4, Backoff>::new();
    let mut generation = core.generation.load(Ordering::Acquire);
    while !core.stopped.load(Ordering::Relaxed) {
        inbound.retry(&core);
        let current_generation = core.generation.load(Ordering::Acquire);
        if generation != current_generation {
            let now = Instant::now();
            for attempt in attempts.values_mut() {
                // Recheck reachability on resume without forgetting a known
                // digest peer and allowing legacy fallback on its address.
                attempt.last_attempt = now;
                attempt.next = now;
                attempt.failures = 0;
                attempt.quiet_matches = 0;
            }
            core.inbound_until.lock().unwrap().clear();
            generation = current_generation;
        }
        let mut delay = Duration::from_secs(3);
        if core.active()
            && let Ok((_, workspace, _)) = core.sync_context()
        {
            let mut preferred: BTreeSet<_> = core.routes.lock().unwrap().addresses().collect();
            preferred.extend(core.discovered.lock().unwrap().iter().filter_map(
                |(&address, seen)| {
                    (seen.workspace == workspace && seen.time.elapsed() < DISCOVERY_TTL)
                        .then_some(address)
                },
            ));
            let peers: BTreeSet<_> = core
                .snapshot()
                .peers
                .union(&preferred)
                .filter(|peer| core.permits_ip(*peer.ip()))
                .copied()
                .collect();
            attempts.retain(|address, _| peers.contains(address));
            let now = Instant::now();
            for address in &peers {
                attempts.entry(*address).or_insert(Backoff {
                    // A newly admitted peer must finish saving its workspace
                    // before it can authenticate a separate sync connection.
                    next: now + Duration::from_millis(300),
                    last_attempt: now,
                    failures: 0,
                    quiet_matches: 0,
                    protocol: PeerProtocol::Unknown,
                });
            }
            if calendar_wake.take_change() {
                // A real edit can use either direction. Preserve the failure backoff
                // and coalesce rapid writes to a healthy peer into one exchange.
                core.inbound_until.lock().unwrap().clear();
                for attempt in attempts
                    .values_mut()
                    .filter(|attempt| attempt.failures == 0)
                {
                    attempt.quiet_matches = 0;
                    attempt.next = attempt
                        .next
                        .min(attempt.last_attempt + Duration::from_millis(300));
                }
            }
            let address = next_peer(
                &peers,
                &preferred,
                &attempts,
                &core.inbound_until.lock().unwrap(),
                now,
            );
            if let Some(address) = address {
                let retry = attempts.get_mut(&address).expect("known peer");
                let result = sync_peer(&core, address, &mut retry.protocol);
                retry.record_exchange(result, Instant::now());
            }
            // Respect the existing 300 ms pacing between distinct ready peers.
            // With nothing due, sleep until the earliest possible connection.
            let now = Instant::now();
            let incoming = core.inbound_until.lock().unwrap();
            if let Some(earliest) = peers
                .iter()
                .map(|peer| {
                    let retry = attempts.get(peer).map_or(now, |entry| entry.next);
                    retry.max(incoming.get(peer).copied().unwrap_or(now))
                })
                .min()
            {
                delay = earliest
                    .saturating_duration_since(now)
                    .max(Duration::from_millis(300));
            }
        }
        let retry = inbound
            .retry_at()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        match (core.active(), retry) {
            (true, Some(retry)) => thread::park_timeout(delay.min(retry)),
            (true, None) => thread::park_timeout(delay),
            (false, Some(retry)) => thread::park_timeout(retry),
            (false, None) => thread::park(),
        }
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
    key: [u8; 32],
}

#[derive(Clone, Copy)]
struct InvitationFallback<'a> {
    host: Id,
    check_after: Instant,
    attempted: &'a BTreeSet<SocketAddrV4>,
}

fn join(
    core: &Core,
    code: &str,
    employee: String,
    manual_address: Option<SocketAddrV4>,
    renewal: bool,
) -> Fallible<()> {
    core.require_operation(renewal)?;
    let code = code.trim();
    // IPv4 содержит точки. Читаем первые пять полей и оставляем адрес целиком.
    let mut fields = code.splitn(6, '.');
    if code.len() > 300 || fields.next() != Some("yr2") {
        return Err("Нужен адресный код приглашения от коллеги с версией 0.5.41 или новее.".into());
    }
    let host = Id::parse_str(fields.next().ok_or("В коде отсутствует устройство.")?)?;
    let id = Id::parse_str(fields.next().ok_or("В коде отсутствует приглашение.")?)?;
    let encoded = fields.next().ok_or("В коде отсутствует ключ.")?;
    let key: [u8; 32] = URL_SAFE_NO_PAD
        .decode(encoded)?
        .try_into()
        .map_err(|_| "Неверная длина ключа.")?;
    let host_public =
        pairing::decode_public(fields.next().ok_or("Нет ключа приглашающего устройства.")?)?;
    let credentials = JoinCredentials {
        host,
        id,
        key,
        host_public,
    };
    let embedded = match fields.next().ok_or("Код приглашения неполный.")? {
        "-" => None,
        value => Some(value.parse::<SocketAddrV4>()?),
    };
    let employee = employee.trim().to_owned();
    if employee.is_empty() || employee.chars().count() > 80 {
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
    *core.pending_discovery.lock().unwrap() = Some((host, discovery::Key::derive(&key, false, id)));
    let _pending = Pending(core);
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
                    delta::exchange(
                        core,
                        &mut stream,
                        &mut transport,
                        &state,
                        *address.ip(),
                        delta::Role::Initiator,
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
    if packet.device == state.device {
        return Err("Это адрес текущего устройства.".into());
    }
    let device = packet.device;
    let peer = peer_address(*address.ip(), packet.port)?;
    if let Err(error) = core.merge(packet) {
        core.record_merge_error(peer, &error.to_string());
        return Err(error);
    }
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
    let (secret, recipient) = match operation {
        Operation::LegacySync | Operation::DigestSync => {
            let (_, workspace, key) = core.sync_context()?;
            if id != workspace {
                return Err("Запрошено другое пространство.".into());
            }
            (key, None)
        }
        Operation::Join => core
            .invites
            .lock()
            .unwrap()
            .get(&id)
            .filter(|invite| invite.expires > Instant::now())
            .map(|invite| (invite.key, Some(invite.recipient)))
            .ok_or("Приглашение просрочено.")?,
    };
    let key = match &recipient {
        Some(peer) => ChannelKey::Invitation {
            secret: &secret,
            peer,
        },
        None => ChannelKey::Group(&secret),
    };
    let mut session = key.builder(&core.pairing, &header)?.build_responder()?;
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
            if request.device == core.snapshot().device {
                return Err("Это адрес текущего устройства.".into());
            }
            let device = request.device;
            let peer = peer_address(ip, request.port)?;
            if let Err(error) = core.merge(request) {
                core.record_merge_error(peer, &error.to_string());
                return Err(error);
            }
            let state = core.snapshot();
            send_secure(
                &mut stream,
                &mut transport,
                &legacy_packet(&state, core.advertised_port())?,
            )?;
            core.inbound_exchange(state.device, device, peer, RECENT_INBOUND);
            Ok(())
        }
        Operation::DigestSync if stream.capabilities.contains(Capabilities::DELTA) => {
            let edit_wake = stream.capabilities.contains(Capabilities::EDIT_WAKE);
            delta::exchange(
                core,
                &mut stream,
                &mut transport,
                &core.snapshot(),
                ip,
                delta::Role::Responder { edit_wake },
            )
            .map(|_| ())
        }
        Operation::DigestSync => respond_digest(core, &mut stream, &mut transport, ip),
    }
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
    const LEN: usize = 50;

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

fn send_control(
    stream: &mut impl Write,
    transport: &mut TransportState,
    data: &[u8],
) -> Fallible<()> {
    let mut wire = [0; SyncHello::LEN + 18];
    send_encrypted(stream, transport, data, &mut wire)
}

fn read_control<const N: usize>(
    stream: &mut impl Read,
    transport: &mut TransportState,
) -> Fallible<[u8; N]> {
    let mut wire = [0; SyncHello::LEN + 16];
    let wire = read_frame_into(stream, &mut wire[..N + 16])?;
    let mut plain = [0; N];
    if transport.read_message(wire, &mut plain)? != N {
        return Err("Неверный размер управляющего сообщения.".into());
    }
    Ok(plain)
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
    core.inbound_exchange(local.device, remote.device, peer, grace);
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
    if let Err(error) = result {
        core.record_merge_error(peer, &error.to_string());
        let _ = Receipt::Rejected.send(stream, transport);
        return Err(error);
    }
    Ok(())
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
    if request.employee.trim().is_empty() || request.employee.chars().count() > 80 {
        return Err("Указано неверное имя.".into());
    }
    let peer = SocketAddrV4::new(ip, request.port);
    check_address(peer)?;
    let response = core.store.lock().unwrap().transaction(|state| {
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
        if workspace.calendar.members.contains_key(&request.device) {
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
        Ok(response)
    })?;
    invite.used_by = Some(claimant);
    core.exchanged(request.device, peer);
    core.wake_sync();
    Ok(response)
}

fn header(kind: Operation, id: Id) -> [u8; 25] {
    let mut result = [0; 25];
    result[..8].copy_from_slice(&MAGIC);
    result[8] = kind as u8;
    result[9..].copy_from_slice(id.as_bytes());
    result
}

fn noise<'a>(header: &'a [u8], key: &'a [u8; 32]) -> Fallible<Builder<'a>> {
    Ok(Builder::new(NOISE.parse()?).prologue(header)?.psk(0, key)?)
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

fn write_frame(stream: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let length = u16::try_from(bytes.len()).map_err(io::Error::other)?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(bytes)
}

fn read_frame(stream: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let length = read_frame_length(stream, max)?;
    let mut frame = vec![0; length];
    stream.read_exact(&mut frame)?;
    Ok(frame)
}

fn read_frame_into<'a>(stream: &mut impl Read, buffer: &'a mut [u8]) -> io::Result<&'a [u8]> {
    let length = read_frame_length(stream, buffer.len())?;
    stream.read_exact(&mut buffer[..length])?;
    Ok(&buffer[..length])
}

fn read_frame_length(stream: &mut impl Read, max: usize) -> io::Result<usize> {
    let mut length = [0; 2];
    stream.read_exact(&mut length)?;
    let length = usize::from(u16::from_be_bytes(length));
    if length == 0 || length > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Неверный размер сетевого сообщения",
        ));
    }
    Ok(length)
}

fn send_secure<T: Serialize>(
    stream: &mut impl Write,
    transport: &mut TransportState,
    value: &T,
) -> Fallible<()> {
    let mut json = Vec::new();
    serde_json::to_writer(LimitedWriter::new(&mut json, MESSAGE_LIMIT), value)?;
    send_bytes(stream, transport, &json)
}

fn send_encrypted(
    stream: &mut impl Write,
    transport: &mut TransportState,
    data: &[u8],
    wire: &mut [u8],
) -> Fallible<()> {
    let written = transport.write_message(data, &mut wire[2..])?;
    wire[..2].copy_from_slice(&u16::try_from(written)?.to_be_bytes());
    stream.write_all(&wire[..2 + written])?;
    Ok(())
}

fn send_bytes(
    stream: &mut impl Write,
    transport: &mut TransportState,
    bytes: &[u8],
) -> Fallible<()> {
    let length = u32::try_from(bytes.len())?.to_be_bytes();
    send_control(stream, transport, &length)?;
    let mut buffer = vec![0; bytes.len().min(FRAME) + 18];
    for plaintext in bytes.chunks(FRAME) {
        send_encrypted(stream, transport, plaintext, &mut buffer)?;
    }
    Ok(())
}

fn read_secure<T: DeserializeOwned>(
    stream: &mut impl Read,
    transport: &mut TransportState,
) -> Fallible<T> {
    Ok(serde_json::from_slice(&read_bytes(stream, transport)?)?)
}

fn message_length(stream: &mut impl Read, transport: &mut TransportState) -> Fallible<usize> {
    let length = read_control::<4>(stream, transport)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MESSAGE_LIMIT {
        return Err("Размер календаря превышает лимит синхронизации.".into());
    }
    Ok(length)
}

fn read_bytes(stream: &mut impl Read, transport: &mut TransportState) -> Fallible<Vec<u8>> {
    let length = message_length(stream, transport)?;
    let mut message = vec![0; length];
    let mut buffer = vec![0; length.min(FRAME) + 16];
    for destination in message.chunks_mut(FRAME) {
        let chunk = read_frame_into(stream, &mut buffer[..destination.len() + 16])?;
        if transport.read_message(chunk, destination)? != destination.len() {
            return Err("Получено неполное сообщение.".into());
        }
    }
    Ok(message)
}

struct DigestWriter(Blake2s256);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Digest::update(&mut self.0, bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn calendar_digest(calendar: &Calendar) -> Fallible<[u8; 32]> {
    let mut digest = DigestWriter(Blake2s256::new());
    let mut writer = BufWriter::new(LimitedWriter::new(&mut digest, MESSAGE_LIMIT));
    serde_json::to_writer(&mut writer, calendar)?;
    writer.into_inner().map_err(|e| e.into_error())?;
    Ok(digest.0.finalize().into())
}

fn encode_calendar(calendar: &Calendar) -> Fallible<Vec<u8>> {
    let mut compressed = Vec::new();
    let mut output = LimitedWriter::new(&mut compressed, MESSAGE_LIMIT);
    let mut encoder = ZlibEncoder::new(&mut output, Compression::fast());
    let mut writer = BufWriter::new(LimitedWriter::new(&mut encoder, MESSAGE_LIMIT));
    serde_json::to_writer(&mut writer, calendar)?;
    writer.into_inner().map_err(|e| e.into_error())?;
    encoder.finish()?;
    Ok(compressed)
}

fn send_calendar(
    stream: &mut impl Write,
    transport: &mut TransportState,
    calendar: &Calendar,
) -> Fallible<()> {
    send_bytes(stream, transport, &encode_calendar(calendar)?)
}

struct CalendarFrames<'a, R> {
    stream: &'a mut R,
    transport: &'a mut TransportState,
    encrypted: Vec<u8>,
    decoded: Vec<u8>,
    unread: std::ops::Range<usize>,
    remaining: usize,
}

impl<'a, R> CalendarFrames<'a, R> {
    fn new(stream: &'a mut R, transport: &'a mut TransportState, length: usize) -> Self {
        let capacity = length.min(FRAME);
        Self {
            stream,
            transport,
            encrypted: vec![0; capacity + 16],
            decoded: vec![0; capacity],
            unread: 0..0,
            remaining: length,
        }
    }
}

impl<R: Read> BufRead for CalendarFrames<'_, R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.unread.is_empty() && self.remaining > 0 {
            let size = self.remaining.min(FRAME);
            let encrypted = read_frame_into(self.stream, &mut self.encrypted[..size + 16])?;
            let count = self
                .transport
                .read_message(encrypted, &mut self.decoded[..size])
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Не удалось прочитать кадр календаря.",
                    )
                })?;
            if count != size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Получено неполное сообщение.",
                ));
            }
            self.remaining -= count;
            self.unread = 0..count;
        }
        Ok(&self.decoded[self.unread.clone()])
    }

    fn consume(&mut self, amount: usize) {
        self.unread.start += amount.min(self.unread.len());
    }
}

impl<R: Read> Read for CalendarFrames<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let count = self.fill_buf()?.read(output)?;
        self.consume(count);
        Ok(count)
    }
}

struct DigestReader<R> {
    source: R,
    digest: Blake2s256,
}

impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.source.read(bytes)?;
        Digest::update(&mut self.digest, &bytes[..count]);
        Ok(count)
    }
}

fn decode_calendar(
    compressed: impl BufRead,
    length: usize,
    expected: [u8; 32],
) -> Fallible<Calendar> {
    decode_calendar_with_limit(compressed, length, expected, MESSAGE_LIMIT)
}

fn decode_calendar_with_limit(
    compressed: impl BufRead,
    length: usize,
    expected: [u8; 32],
    max: usize,
) -> Fallible<Calendar> {
    let decoder = ZlibDecoder::new(compressed);
    let reader = DigestReader {
        source: decoder.take(max as u64 + 1),
        digest: Blake2s256::new(),
    };
    let mut buffered = BufReader::new(reader);
    let calendar = serde_json::from_reader(&mut buffered)?;
    let reader = buffered.into_inner();
    if reader.source.limit() == 0 || reader.source.into_inner().total_in() != length as u64 {
        return Err("Неверный размер сжатого календаря.".into());
    }
    let actual: [u8; 32] = reader.digest.finalize().into();
    if actual != expected {
        return Err("Календарь не совпадает с контрольной суммой обмена.".into());
    }
    Ok(calendar)
}

fn read_calendar(
    stream: &mut impl Read,
    transport: &mut TransportState,
    expected: [u8; 32],
) -> Fallible<Calendar> {
    let length = message_length(stream, transport)?;
    decode_calendar(
        CalendarFrames::new(stream, transport, length),
        length,
        expected,
    )
}

#[cfg(test)]
mod tests;
