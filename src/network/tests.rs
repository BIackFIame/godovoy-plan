use chrono::NaiveDate;

use super::*;
use super::scheduler::next_peer;
use super::wire::{FRAME, decode_calendar, encode_calendar, send_bytes};
use crate::{
    model::{Category, Event, Revision, Task},
    storage::Store,
};

#[test]
fn employee_name_policy_counts_unicode_scalars_without_normalizing() {
    assert!(!valid_employee_name(""));
    assert!(!valid_employee_name(" \t "));
    assert!(valid_employee_name(&"Яя".repeat(40)));
    assert!(!valid_employee_name(&"Я".repeat(81)));
    assert!(valid_employee_name("  Анна  "));
}

#[test]
fn invitation_parser_preserves_the_wire_format_and_rejects_invalid_keys() {
    let host = Id::new_v4();
    let id = Id::new_v4();
    let public = PairingKeys::generate().unwrap().encoded_public();
    let key = [42; 32];
    let encoded = URL_SAFE_NO_PAD.encode(key);
    let code = |key: &str, address: &str| format!(" yr2.{host}.{id}.{key}.{public}.{address} ");

    let (parsed, address) = JoinCredentials::parse(&code(&encoded, "127.0.0.1:45736")).unwrap();
    assert_eq!((parsed.host, parsed.id, *parsed.key), (host, id, key));
    assert_eq!(address, Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45736)));
    assert!(
        JoinCredentials::parse(&code(&encoded, "-"))
            .unwrap()
            .1
            .is_none()
    );
    assert!(JoinCredentials::parse(&code(&encoded, "invalid")).is_err());

    let short = URL_SAFE_NO_PAD.encode([42; 31]);
    let padded = format!("{encoded}=");
    let invalid = format!("{}!", &encoded[..42]);
    for malformed in [&short, &padded, &invalid] {
        assert!(JoinCredentials::parse(&code(malformed, "-")).is_err());
    }
}

#[test]
fn access_jobs_allow_retry_after_work_finishes_and_reject_overlap() {
    const WAIT: Duration = Duration::from_secs(5);

    #[derive(Debug)]
    struct HeldReply {
        disposing: mpsc::SyncSender<()>,
        resume: Mutex<Receiver<()>>,
    }

    impl std::fmt::Display for HeldReply {
        fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            output.write_str("test reply")
        }
    }

    impl std::error::Error for HeldReply {}

    impl Drop for HeldReply {
        fn drop(&mut self) {
            let _ = self.disposing.send(());
            let _ = self.resume.get_mut().unwrap().recv_timeout(WAIT);
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("profile")).unwrap();
    let network = Network {
        core: Arc::new(Core::new(store, SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap()),
        workers: Vec::new(),
        joining: Mutex::default(),
    };
    let (started, work_started) = mpsc::sync_channel(1);
    let (finish, finish_work) = mpsc::sync_channel(1);
    let (disposing, reply_disposing) = mpsc::sync_channel(1);
    let (resume, resume_cleanup) = mpsc::sync_channel(1);
    let reply = network.access_job(move |_| {
        started.send(()).unwrap();
        finish_work.recv_timeout(WAIT).unwrap();
        Err(Box::new(HeldReply {
            disposing,
            resume: Mutex::new(resume_cleanup),
        }))
    });
    work_started.recv_timeout(WAIT).unwrap();
    let overlapping = network.access_job(|_| Ok(()));

    // Discard the result and hold its destructor beyond the end of actual work.
    drop(reply);
    finish.send(()).unwrap();
    reply_disposing.recv_timeout(WAIT).unwrap();
    let retry = network.access_job(|_| Ok(()));
    let before_cleanup = retry.try_recv();
    resume.send(()).unwrap();

    assert_eq!(
        overlapping
            .recv_timeout(WAIT)
            .unwrap()
            .unwrap_err()
            .to_string(),
        "Подключение уже выполняется."
    );
    assert!(
        matches!(before_cleanup, Err(mpsc::TryRecvError::Empty)),
        "{before_cleanup:?}"
    );
    retry.recv_timeout(WAIT).unwrap().unwrap();
}

#[test]
fn network_interface_limits_discovery_and_outgoing_sources() {
    let port = 45737;
    let conf = |ip| Config {
        listen: SocketAddrV4::new(ip, 0),
        discovery_port: Some(port),
    };
    assert_eq!(
        conf(Ipv4Addr::UNSPECIFIED).effective_discovery_port(),
        Some(port)
    );
    assert_eq!(
        conf(Ipv4Addr::LOCALHOST).effective_discovery_port(),
        Some(port)
    );
    assert_eq!(
        conf(Ipv4Addr::new(192, 168, 1, 20)).effective_discovery_port(),
        None
    );
    let udp = discovery::socket(Ipv4Addr::LOCALHOST, 0).unwrap();
    assert_eq!(udp.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
    assert!(!udp.broadcast().unwrap());
    let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let SocketAddr::V4(target) = tcp.local_addr().unwrap() else {
        unreachable!()
    };
    let _client = connect_from(Ipv4Addr::LOCALHOST, target, Duration::from_secs(1)).unwrap();
    assert_eq!(tcp.accept().unwrap().1.ip(), Ipv4Addr::LOCALHOST);
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("node")).unwrap();
    let n = Network::start(
        store,
        Config {
            listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            discovery_port: None,
        },
    )
    .unwrap();
    let foreign = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 45736);
    assert!(n.remember_peer(foreign).is_err());
    assert!(n.core.check_peer(foreign).is_err());
}

#[test]
fn loopback_discovery_uses_pending_invite_then_group_and_closes_during_pause() {
    let directory = tempfile::tempdir().unwrap();
    let host_store = Store::open(&directory.path().join("host")).unwrap();
    host_store
        .lock()
        .unwrap()
        .transaction(|s| s.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    let peer_store = Store::open(&directory.path().join("peer")).unwrap();
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let host = Network::start(host_store.clone(), config).unwrap();
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let peer = Network::start(
        peer_store,
        Config {
            discovery_port: Some(udp),
            ..config
        },
    )
    .unwrap();
    let address = (Ipv4Addr::LOCALHOST, udp);
    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let host_address = host.address().unwrap();
    let snapshot = host.core.snapshot();
    let group = snapshot.workspace.as_ref().unwrap();
    let invite = host.make_invite(&peer.pairing_request()).unwrap();
    let mut fields = invite.splitn(6, '.').collect::<Vec<_>>();
    fields[5] = "-";
    let secret: [u8; 32] = URL_SAFE_NO_PAD
        .decode(fields[3])
        .unwrap()
        .try_into()
        .unwrap();
    let invite_key = discovery::Key::derive(&secret, false, Id::parse_str(fields[2]).unwrap());
    let group_key = discovery::Key::derive(&group.key, true, group.id);
    let beacon = |key: &discovery::Key| {
        key.seal(
            snapshot.device,
            group.id,
            host_address.port(),
            Utc::now().timestamp(),
        )
        .unwrap()
    };
    let wait = |stage: &str, timeout: Duration, f: &dyn Fn() -> bool| {
        let deadline = Instant::now() + timeout;
        while !f() {
            assert!(
                Instant::now() < deadline,
                "Discovery timed out at {stage}, port={udp}, paused={}, warning={:?}",
                peer.paused(),
                peer.core.inbound.get().unwrap().warning(),
            );
            thread::sleep(Duration::from_millis(5));
        }
    };
    let seen = || {
        peer.core
            .discovered
            .lock()
            .unwrap()
            .contains_key(&host_address)
    };
    // A group beacon alone cannot complete enrollment.
    sender.send_to(&beacon(&group_key), address).unwrap();
    let join = peer.join_async(fields.join("."), "Борис".into(), None);
    let discovery_timeout = Duration::from_secs(2);
    wait("pending invitation", discovery_timeout, &|| {
        peer.core.pending_discovery.lock().unwrap().is_some()
    });
    sender.send_to(&beacon(&group_key), address).unwrap();
    assert!(matches!(
        join.recv_timeout(Duration::from_millis(150)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    sender.send_to(&beacon(&invite_key), address).unwrap();
    join.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    assert!(peer.core.pending_discovery.lock().unwrap().is_none());
    peer.core.discovered.lock().unwrap().clear();
    sender.send_to(&beacon(&group_key), address).unwrap();
    wait("group beacon after enrollment", discovery_timeout, &seen);
    let inbound = peer.core.inbound.get().unwrap();
    assert!(inbound.discovery_bound());
    peer.set_paused(true);
    assert!(peer.paused());
    assert!(!inbound.discovery_bound());
    assert!(!peer.can_sync_automatically());
    peer.core.discovered.lock().unwrap().clear();
    // A concurrent process launch on macOS can retain a closed socket until exec.
    // The application must drop its own socket synchronously regardless.
    let deadline = Instant::now() + Duration::from_secs(1);
    let monitor = loop {
        match UdpSocket::bind(address) {
            Ok(socket) => break socket,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("Could not bind the released UDP port {udp}: {error}"),
        }
    };
    monitor
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let packet = beacon(&group_key);
    sender.send_to(&packet, address).unwrap();
    let mut buffer = [0; discovery::LENGTH + 1];
    assert_eq!(monitor.recv(&mut buffer).unwrap(), discovery::LENGTH);
    assert!(!seen());
    drop(monitor);
    peer.set_paused(false);
    wait("UDP socket after resume", Duration::from_secs(5), &|| {
        inbound.discovery_bound()
    });
    sender.send_to(&beacon(&group_key), address).unwrap();
    wait("group beacon after resume", discovery_timeout, &seen);
}

#[test]
fn retired_key_cannot_sync_with_new_generation_in_either_wire_protocol() {
    let directory = tempfile::tempdir().unwrap();
    let previous = Store::open(&directory.path().join("host")).unwrap();
    previous
        .lock()
        .unwrap()
        .transaction(|s| s.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    let old = previous.lock().unwrap().state().workspace.clone().unwrap();
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let old_peer = Store::open(&directory.path().join("old-peer")).unwrap();
    old_peer
        .lock()
        .unwrap()
        .transaction(|s| {
            s.workspace = Some(old);
            let device = s.device;
            let revision = s.revision()?;
            s.workspace_mut()?
                .calendar
                .members
                .insert(device, Record::new(revision, "Иван".into()));
            Ok(())
        })
        .unwrap();
    let stale = Network::start(old_peer.clone(), config).unwrap();
    let before = old_peer.lock().unwrap().snapshot();
    let host = Network::start(previous.clone(), config).unwrap();
    host.renew_access_async()
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let current =
        Store::open_existing(&previous.lock().unwrap().successor_directory().unwrap()).unwrap();
    let new_host = Network::start(current.clone(), config).unwrap();
    let target = current.lock().unwrap().snapshot();
    for mut protocol in [
        PeerProtocol::Digest,
        PeerProtocol::LegacyUntil(Instant::now() + LEGACY_RECHECK),
    ] {
        assert!(sync_peer(&stale.core, new_host.address().unwrap(), &mut protocol).is_err());
    }
    assert_eq!(&*old_peer.lock().unwrap().snapshot(), &*before);
    assert_eq!(&*current.lock().unwrap().snapshot(), &*target);
}

#[test]
fn merge_calendar_reports_calendar_changes_independently_of_network_time() {
    let directory = tempfile::tempdir().unwrap();
    let shared = Store::open(&directory.path().join("profile")).unwrap();
    let mut store = shared.lock().unwrap();
    store
        .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    let source = store.state().device;
    let local = workspace(store.state()).unwrap();
    let id = local.id;
    let old = local.calendar.clone();
    let mut update = old.clone();
    update.members.insert(
        source,
        Record::new(store.state().revision().unwrap(), "Вера".into()),
    );

    assert!(merge_calendar(&mut store, id, source, update.clone(), None).unwrap());
    assert_eq!(store.state().last_sync, None);
    let snapshot = store.snapshot();
    assert!(!merge_calendar(&mut store, id, source, old, None).unwrap());
    assert!(Arc::ptr_eq(&snapshot, &store.snapshot()));

    assert!(!merge_calendar(&mut store, id, source, update, Some(123)).unwrap());
    assert_eq!(store.state().last_sync, Some(123));
    assert_eq!(
        workspace(store.state()).unwrap().calendar.members[&source].value,
        "Вера"
    );
}

#[test]
fn removing_a_saved_address_reopens_capacity_without_changing_learned_routes() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let store = Store::open(&profile).unwrap();
    let address = |port| SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let removed = address(2000);
    let replacement = address(9000);
    store
        .lock()
        .unwrap()
        .transaction(|state| {
            state
                .peers
                .extend((2000..2000 + MAX_PEERS as u16).map(address));
            Ok(())
        })
        .unwrap();
    let network = Network {
        core: Arc::new(Core::new(store, address(0)).unwrap()),
        workers: Vec::new(),
        joining: Mutex::default(),
    };
    network
        .core
        .routes
        .lock()
        .unwrap()
        .remember(Id::new_v4(), removed);
    let route_file = profile.join("peer-routes-v1.json");
    let routes_before = std::fs::read(&route_file).unwrap();

    assert!(network.remember_peer(replacement).is_err());
    network.remove_saved_address(removed).unwrap();
    network.remember_peer(replacement).unwrap();
    let state = network.core.snapshot();
    assert!(!state.peers.contains(&removed));
    assert!(state.peers.contains(&replacement));
    assert_eq!(state.peers.len(), MAX_PEERS);
    assert_eq!(
        network
            .core
            .routes
            .lock()
            .unwrap()
            .addresses()
            .collect::<Vec<_>>(),
        [removed]
    );
    assert_eq!(std::fs::read(&route_file).unwrap(), routes_before);

    drop(network);
    let reopened = Store::open(&profile).unwrap();
    assert_eq!(reopened.lock().unwrap().state().peers, state.peers);
}

#[test]
fn restore_does_not_reject_previously_observed_revisions() {
    let directory = tempfile::tempdir().unwrap();
    let profile = directory.path().join("profile");
    let shared = Store::open(&profile).unwrap();
    let peer = Id::new_v4();
    let prior = Revision {
        sequence: 2 * MAX_REVISION_LEAD,
        operation: Id::new_v4(),
    };
    let previous = {
        let mut store = shared.lock().unwrap();
        store
            .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
            .unwrap();
        let backup = store.create_backup().unwrap();
        store
            .transaction(|state| {
                state
                    .workspace_mut()?
                    .calendar
                    .members
                    .insert(peer, Record::new(prior, "Борис".into()));
                Ok(())
            })
            .unwrap();
        let previous = store.snapshot();
        store.restore_open_backup(&backup).unwrap();
        assert!(store.state().latest_revision() >= prior.sequence);
        assert!(
            workspace(store.state()).unwrap().calendar.latest() + MAX_REVISION_LEAD
                < prior.sequence
        );
        previous
    };
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let network = Network::start(shared, config).unwrap();
    assert!(network.paused());
    network.set_profile_paused(false).unwrap();
    let previous = workspace(&previous).unwrap();
    let packet = |calendar| SyncPacket {
        device: peer,
        workspace: previous.id,
        port: SYNC_PORT,
        calendar,
    };

    let mut excessive = previous.calendar.clone();
    excessive.members.get_mut(&peer).unwrap().revision.sequence =
        prior.sequence + MAX_REVISION_LEAD + 1;
    let before = network.core.snapshot();
    let path = profile.join("calendar.json");
    let bytes_before = std::fs::read(&path).unwrap();
    assert_eq!(
        network
            .core
            .merge(packet(excessive))
            .unwrap_err()
            .to_string(),
        "Некорректная версия данных у другого устройства."
    );
    assert!(Arc::ptr_eq(&before, &network.core.snapshot()));
    assert_eq!(std::fs::read(path).unwrap(), bytes_before);

    network
        .core
        .merge(packet(previous.calendar.clone()))
        .unwrap();
    let current = network.core.snapshot();
    assert_eq!(workspace(&current).unwrap().calendar, previous.calendar);
}

#[test]
fn repeated_port_changes_keep_one_beacon_per_device_on_an_ip() {
    let address = |port| SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let original = Seen {
        device: Id::from_u128(1),
        workspace: Id::from_u128(2),
        time: Instant::now(),
    };
    let mut seen = BTreeMap::new();
    for (port, device) in [(1000, 3), (1001, 4)] {
        discovery::remember(
            &mut seen,
            address(port),
            Seen {
                device: Id::from_u128(device),
                ..original
            },
        );
    }
    let alternate = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 2), 2000);
    discovery::remember(&mut seen, alternate, original);
    for port in 2000..2005 {
        discovery::remember(&mut seen, address(port), original);
    }
    discovery::remember(
        &mut seen,
        address(1002),
        Seen {
            device: Id::from_u128(5),
            ..original
        },
    );
    assert_eq!(
        seen.keys().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([address(1000), address(1001), address(2004), alternate])
    );
}

struct Replica {
    core: Core,
    listener: TcpListener,
    _profile: tempfile::TempDir,
}

impl Replica {
    fn new() -> Self {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(&profile.path().join("profile")).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
            unreachable!("IPv4 listener");
        };
        Self {
            core: Core::new(store, address).unwrap(),
            listener,
            _profile: profile,
        }
    }
}

#[test]
fn invitation_probe_waits_for_a_fresh_untried_address() {
    let replica = Replica::new();
    let tcp = TcpStream::connect(replica.core.listen).unwrap();
    let mut connection = Connection::new(
        &replica.core,
        tcp,
        replica.core.generation.load(Ordering::Acquire),
        Instant::now() + SESSION_LIMIT,
    )
    .unwrap();
    let host = Id::from_u128(1);
    let attempted = BTreeSet::from([replica.core.listen]);
    connection.invitation_fallback = Some(InvitationFallback {
        host,
        check_after: Instant::now(),
        attempted: &attempted,
    });
    let mut beacon = Seen {
        device: host,
        workspace: Id::from_u128(2),
        time: Instant::now(),
    };
    replica
        .core
        .discovered
        .lock()
        .unwrap()
        .insert(replica.core.listen, beacon);
    assert_eq!(connection.timeout().unwrap(), IO_POLL);
    let alternate_port = replica.core.advertised_port().wrapping_add(1).max(1);
    let alternate = SocketAddrV4::new(Ipv4Addr::LOCALHOST, alternate_port);
    let expired = Seen {
        time: beacon.time - DISCOVERY_TTL,
        ..beacon
    };
    replica
        .core
        .discovered
        .lock()
        .unwrap()
        .insert(alternate, expired);
    assert_eq!(connection.timeout().unwrap(), IO_POLL);
    beacon.time = Instant::now();
    replica
        .core
        .discovered
        .lock()
        .unwrap()
        .insert(alternate, beacon);
    assert_eq!(
        connection.timeout().unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
    connection.invitation_fallback = None;
    assert_eq!(connection.timeout().unwrap(), IO_POLL);
}

fn team(events: usize) -> (Replica, Replica) {
    let leader = Replica::new();
    let colleague = Replica::new();
    let colleague_id = colleague.core.snapshot().device;
    let shared_workspace = leader
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.create_workspace("Тестовая команда".into(), "Анна".into())?;
            let revision = state.revision()?;
            let workspace = state.workspace_mut()?;
            workspace
                .calendar
                .members
                .insert(colleague_id, Record::new(revision, "Борис".to_owned()));
            for index in 0..events {
                let event = Event {
                    title: format!("Подготовка события №{index}"),
                    description: "Проверить смету, список участников и дату подтверждения. "
                        .repeat(8),
                    category: Category::Team,
                    date: NaiveDate::from_ymd_opt(2026, 12, 15).unwrap(),
                    yearly: true,
                    owner: Some(colleague_id),
                    preparation_days: 30,
                    tasks: (0..4)
                        .map(|task| Task {
                            id: Id::new_v4(),
                            title: format!("Согласовать пункт №{task}"),
                            days_before: 7,
                        })
                        .collect(),
                };
                workspace
                    .calendar
                    .plans
                    .entry(Id::new_v4())
                    .or_default()
                    .templates
                    .insert(2026, Record::new(revision, Some(event.into())));
            }
            Ok(workspace.clone())
        })
        .unwrap();
    colleague
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.workspace = Some(Arc::new(shared_workspace));
            Ok(())
        })
        .unwrap();
    (leader, colleague)
}

#[test]
fn publishing_a_saved_address_wakes_every_transport_using_the_profile() {
    let (office, laptop) = team(1);
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let store = &office.core.store;
    let first = Network::start(store.clone(), config).unwrap();
    let second = Network::start(store.clone(), config).unwrap();
    let remote = Network::start(laptop.core.store.clone(), config).unwrap();
    let target = remote.address().unwrap();

    // An ordinary store transaction bypasses Network::remember_peer.
    // Both workers start without a route and the remote knows neither port.
    for network in [&first, &second] {
        let deadline = Instant::now() + Duration::from_secs(2);
        while network
            .core
            .inbound
            .get()
            .unwrap()
            .sync_thread
            .get()
            .is_none()
        {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }
    thread::sleep(Duration::from_millis(50));
    store
        .lock()
        .unwrap()
        .transaction(|s| {
            s.peers.insert(target);
            Ok(())
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !first.has_recent_peer() || !second.has_recent_peer() {
        assert!(
            Instant::now() < deadline,
            "saved address did not reach both workers"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(remote.has_recent_peer());
}

struct Counted<T> {
    stream: T,
    read: usize,
    written: usize,
}

impl<T: Read> Read for Counted<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let length = self.stream.read(buf)?;
        self.read += length;
        Ok(length)
    }
}

impl<T: Write> Write for Counted<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let length = self.stream.write(buf)?;
        self.written += length;
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn measured_exchange(sender: &Replica, receiver: &Replica) -> (usize, usize) {
    thread::scope(|scope| {
        let server = scope.spawn(|| {
            let (stream, _) = receiver.listener.accept().unwrap();
            respond(&receiver.core, stream, Ipv4Addr::LOCALHOST)
        });
        let state = sender.core.snapshot();
        let workspace = workspace(&state).unwrap();
        let (stream, mut transport) = connect(
            &sender.core,
            receiver.core.listen,
            Operation::DigestSync,
            workspace.id,
            ChannelKey::Group(&workspace.key),
            Instant::now() + SESSION_LIMIT,
            None,
        )
        .unwrap();
        let delta_negotiated = stream.capabilities.contains(Capabilities::DELTA);
        let capabilities = stream.capabilities;
        let mut counted = Counted {
            stream,
            read: 0,
            written: 0,
        };
        if delta_negotiated {
            delta::exchange(
                &sender.core,
                &mut counted,
                &mut transport,
                &state,
                Ipv4Addr::LOCALHOST,
                delta::Role::Initiator,
                capabilities,
            )
            .unwrap();
        } else {
            sync_digest(
                &sender.core,
                &mut counted,
                &mut transport,
                &state,
                Ipv4Addr::LOCALHOST,
            )
            .unwrap();
        }
        server.join().unwrap().unwrap();
        (counted.written, counted.read)
    })
}

#[test]
fn four_lan_devices_converge_after_edits_during_a_partition() {
    let (first, second) = team(4);
    let profiles = [first, second, Replica::new(), Replica::new()]
        .map(|replica| (replica._profile, replica.core.store.clone()));
    let ids = profiles
        .each_ref()
        .map(|(_, store)| store.lock().unwrap().state().device);
    let group = profiles[0]
        .1
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let workspace = state.workspace_mut()?;
            for (name, device) in ["Виктор", "Дарья"].into_iter().zip(&ids[2..]) {
                workspace
                    .calendar
                    .members
                    .insert(*device, Record::new(revision, name.into()));
            }
            Ok(workspace.clone())
        })
        .unwrap();
    for (_, store) in &profiles[1..] {
        store
            .lock()
            .unwrap()
            .transaction(|state| {
                state.workspace = Some(Arc::new(group.clone()));
                Ok(())
            })
            .unwrap();
    }
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let nodes = profiles
        .each_ref()
        .map(|(_, store)| Network::start(store.clone(), config).unwrap());
    let addresses = nodes.each_ref().map(|node| node.address().unwrap());
    for (index, (_, store)) in profiles.iter().enumerate() {
        store
            .lock()
            .unwrap()
            .transaction(|state| {
                // Endpoints have only their immediate neighbors' addresses.
                for (neighbor, address) in addresses.iter().enumerate() {
                    if index.abs_diff(neighbor) == 1 {
                        state.peers.insert(*address);
                    }
                }
                Ok(())
            })
            .unwrap();
    }
    let note = |device: usize, index: usize, text: &str| {
        profiles[device]
            .1
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                let plan = state
                    .workspace_mut()?
                    .calendar
                    .plans
                    .values_mut()
                    .nth(index)
                    .unwrap();
                plan.years.entry(2026).or_default().note =
                    Some(Record::new(revision, text.to_string()));
                Ok(())
            })
            .unwrap();
    };
    let wait_until = |expectation: &str, check: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(35);
        while !check() {
            assert!(Instant::now() < deadline, "{expectation}");
            thread::sleep(Duration::from_millis(50));
        }
    };
    let has = |device: usize, index: usize, text: &str| {
        let current = nodes[device].core.snapshot();
        workspace(&current)
            .unwrap()
            .calendar
            .plans
            .values()
            .nth(index)
            .unwrap()
            .years
            .get(&2026)
            .and_then(|record| record.note.as_ref())
            .is_some_and(|note| note.value == text)
    };
    note(0, 0, "Открыли подготовку");
    wait_until(
        "первая запись дошла через соседей",
        &|| has(3, 0, "Открыли подготовку"),
    );
    nodes[3].set_paused(true);
    for (index, text) in [
        "Уточнили дату",
        "Заказали зал",
        "Проверили список",
        "Подписали письмо",
    ]
    .into_iter()
    .enumerate()
    {
        note(index, index, text);
    }
    wait_until(
        "три сотрудника увидели правки коллег",
        &|| {
            has(0, 2, "Проверили список")
                && has(2, 0, "Уточнили дату")
                && has(0, 1, "Заказали зал")
        },
    );
    assert!(has(3, 0, "Открыли подготовку"));
    nodes[3].set_paused(false);
    wait_until(
        "четыре календаря сошлись после возобновления работы",
        &|| {
            let current = nodes[0].core.snapshot();
            let full = &workspace(&current).unwrap().calendar;
            has(0, 3, "Подписали письмо")
                && nodes[1..]
                    .iter()
                    .all(|node| &workspace(&node.core.snapshot()).unwrap().calendar == full)
        },
    );
    for (index, text) in [
        "Уточнили дату",
        "Заказали зал",
        "Проверили список",
        "Подписали письмо",
    ]
    .into_iter()
    .enumerate()
    {
        assert!(has(3, index, text));
    }
}
fn attempt_delta(from: &Replica, to: &Replica) -> (bool, bool) {
    thread::scope(|threads| {
        let inbound = threads.spawn(|| {
            let (socket, _) = to.listener.accept().unwrap();
            respond(&to.core, socket, Ipv4Addr::LOCALHOST).is_ok()
        });
        let sent = sync_peer(&from.core, to.core.listen, &mut PeerProtocol::Digest).is_ok();
        (sent, inbound.join().unwrap())
    })
}

#[test]
fn relayed_roster_respects_enrollment_and_retired_group_keys() {
    let (mut manager, mut colleague) = team(20);
    let third = Replica::new();
    let id = third.core.snapshot().device;
    let enrolled = manager
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let rev = state.revision()?;
            let team = state.workspace_mut()?;
            team.calendar
                .members
                .insert(id, Record::new(rev, "Третий сотрудник".into()));
            Ok(team.clone())
        })
        .unwrap();
    third
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.workspace = Some(Arc::new(enrolled));
            Ok(())
        })
        .unwrap();
    assert_eq!(attempt_delta(&third, &colleague), (false, false));
    assert_eq!(attempt_delta(&manager, &colleague), (true, true));
    assert_eq!(attempt_delta(&third, &colleague), (true, true));

    let outsider = Replica::new();
    let outsider_id = outsider.core.snapshot().device;
    outsider
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.create_workspace("Другая компания".into(), "Новый сотрудник".into())
        })
        .unwrap();
    third
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let rev = state.revision()?;
            state
                .workspace_mut()?
                .calendar
                .members
                .insert(outsider_id, Record::new(rev, "Имя в списке".into()));
            Ok(())
        })
        .unwrap();
    assert_eq!(attempt_delta(&third, &colleague), (true, true));
    assert!(
        workspace(&colleague.core.snapshot())
            .unwrap()
            .calendar
            .members
            .contains_key(&outsider_id)
    );
    assert_eq!(attempt_delta(&outsider, &colleague), (false, false));

    manager
        .core
        .store
        .lock()
        .unwrap()
        .renew_workspace_key(&BTreeSet::from([id]))
        .unwrap();
    let active = manager
        .core
        .store
        .lock()
        .unwrap()
        .successor_directory()
        .unwrap();
    manager.core =
        Core::new(Store::open_existing(&active).unwrap(), manager.core.listen).unwrap();
    let renewed = workspace(&manager.core.snapshot()).unwrap().clone();
    colleague
        .core
        .store
        .lock()
        .unwrap()
        .accept_renewal(renewed, manager.core.listen)
        .unwrap();
    let active = colleague
        .core
        .store
        .lock()
        .unwrap()
        .successor_directory()
        .unwrap();
    colleague.core = Core::new(
        Store::open_existing(&active).unwrap(),
        colleague.core.listen,
    )
    .unwrap();
    assert_eq!(attempt_delta(&manager, &colleague), (true, true));
    let current = colleague.core.snapshot();
    let team = workspace(&current).unwrap();
    assert!(team.calendar.members.contains_key(&id));
    assert!(team.require_device_access(id).is_err());
    assert_eq!(team.access_manager, Some(manager.core.snapshot().device));
    assert_eq!(attempt_delta(&third, &colleague), (false, false));
    assert_eq!(&colleague.core.snapshot().workspace, &current.workspace);
}

#[test]
fn delta_handles_late_low_revisions_and_cold_or_conflicting_peers() {
    let (a, b) = team(1000);
    measured_exchange(&a, &b);
    let initial = workspace(&a.core.snapshot()).unwrap().calendar.latest();
    let edit = |local: &Replica, index: usize, text: &str, old: bool| {
        local
            .core
            .store
            .lock()
            .unwrap()
            .transaction(|state| {
                let mut rev = state.revision()?;
                if old {
                    rev.sequence = initial - 1;
                }
                let p = state
                    .workspace_mut()?
                    .calendar
                    .plans
                    .values_mut()
                    .nth(index)
                    .unwrap();
                p.years.entry(2026).or_default().note = Some(Record::new(rev, text.to_owned()));
                Ok(())
            })
            .unwrap();
    };
    let same = || {
        assert_eq!(
            workspace(&a.core.snapshot()).unwrap().calendar,
            workspace(&b.core.snapshot()).unwrap().calendar
        )
    };
    edit(&a, 10, "Импорт старой правки", true);
    let transferred = measured_exchange(&a, &b);
    assert!(transferred.0 > 100000); // incorrect inferred subset triggers verified full retry
    same();
    for i in 0..12 {
        edit(&a, i, "Двенадцать пропущенных событий", false);
    }
    let (sent, received) = measured_exchange(&b, &a);
    assert!(sent + received < 20000);
    same();
    edit(&a, 300, "Изменение А", false);
    edit(&b, 320, "Изменение Б", false);
    let (sent, received) = measured_exchange(&a, &b);
    assert!(sent + received < 12000);
    same();
    edit(&a, 85, "Возврат после перезапуска", false);
    a.core.delta_baselines.clear_for_test();
    b.core.delta_baselines.clear_for_test();
    let (sent, _) = measured_exchange(&a, &b);
    assert!(sent > 100000);
    same();
}

#[test]
fn digest_exchange_uses_only_controls_until_calendar_changes() {
    let (leader, colleague) = team(1_950);
    assert_eq!(measured_exchange(&leader, &colleague), (68, 87));

    leader
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let plan = state
                .workspace_mut()?
                .calendar
                .plans
                .values_mut()
                .next()
                .unwrap();
            let event = plan.templates.get_mut(&2026).unwrap();
            Arc::make_mut(event.value.as_mut().unwrap()).title =
                "Новая дата согласования".into();
            event.revision = revision;
            Ok(())
        })
        .unwrap();

    let started = Instant::now();
    let (sent, received) = measured_exchange(&leader, &colleague);
    let duration = started.elapsed();
    let source = leader.core.snapshot();
    let target = colleague.core.snapshot();
    assert_eq!(
        workspace(&source).unwrap().calendar,
        workspace(&target).unwrap().calendar
    );
    let old_snapshot =
        serde_json::to_vec(&legacy_packet(&source, leader.core.advertised_port()).unwrap())
            .unwrap();
    assert!(sent < old_snapshot.len() / 3);
    // The responder supplies an eight-byte revision hint and confirms reconstruction.
    assert_eq!(received, 133);
    assert_eq!(measured_exchange(&leader, &colleague), (68, 87));
    eprintln!(
        "1,950 plans: legacy JSON {} bytes per calendar; changed transfer {} B + {} B; unchanged 155 B, after Noise handshake; changed exchange {:?}",
        old_snapshot.len(),
        sent,
        received,
        duration,
    );
}

#[test]
fn streamed_calendar_leaves_the_next_control_frame_intact() {
    let (leader, _) = team(450);
    let snapshot = leader.core.snapshot();
    let workspace = workspace(&snapshot).unwrap();
    let calendar = &workspace.calendar;
    let preamble = header(Operation::DigestSync, workspace.id);
    let mut sender = noise(&preamble, &workspace.key)
        .unwrap()
        .build_initiator()
        .unwrap();
    let mut receiver = noise(&preamble, &workspace.key)
        .unwrap()
        .build_responder()
        .unwrap();
    let (mut wire, mut plain) = ([0; 256], [0; 256]);
    let first = sender.write_message(&[], &mut wire).unwrap();
    receiver.read_message(&wire[..first], &mut plain).unwrap();
    let second = receiver.write_message(&[], &mut wire).unwrap();
    sender.read_message(&wire[..second], &mut plain).unwrap();
    let mut sender = sender.into_transport_mode().unwrap();
    let mut receiver = receiver.into_transport_mode().unwrap();

    let compressed = encode_calendar(calendar).unwrap();
    assert!(compressed.len() > FRAME);
    let mut message = Vec::new();
    send_bytes(&mut message, &mut sender, &compressed).unwrap();
    Receipt::Current.send(&mut message, &mut sender).unwrap();

    let mut stream = io::Cursor::new(message);
    let digest = calendar_digest(calendar).unwrap();
    assert_eq!(
        read_calendar(&mut stream, &mut receiver, digest).unwrap(),
        *calendar
    );
    assert!(Receipt::read(&mut stream, &mut receiver).unwrap() == Receipt::Current);
}

#[test]
fn compressed_calendar_checks_the_complete_stream_and_digest() {
    let calendar = Calendar::default();
    let bytes = encode_calendar(&calendar).unwrap();
    let digest = calendar_digest(&calendar).unwrap();
    let short = &bytes[..bytes.len() - 4];
    assert!(decode_calendar(short, short.len(), digest).is_err());
    assert!(decode_calendar(bytes.as_slice(), bytes.len(), [0; 32]).is_err());
    let mut trailing = bytes;
    trailing.push(0);
    assert!(decode_calendar(trailing.as_slice(), trailing.len(), digest).is_err());
}

#[test]
fn a_recent_incoming_session_defers_only_its_own_reverse_connection() {
    let local = Replica::new();
    let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45100);
    let other = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45101);
    let peers = BTreeSet::from([peer, other]);
    let preferred = BTreeSet::new();
    let no_attempts = BTreeMap::new();

    local
        .core
        .inbound_exchange(Id::from_u128(1), Id::from_u128(2), peer, RECENT_INBOUND, None);
    assert!(local.core.inbound_until.lock().unwrap().is_empty());
    local
        .core
        .inbound_exchange(Id::from_u128(2), Id::from_u128(1), peer, RECENT_INBOUND, None);

    let incoming = local.core.inbound_until.lock().unwrap();
    let expiry = incoming[&peer].until;
    assert_eq!(
        next_peer(
            peers.iter().copied(),
            &preferred,
            &no_attempts,
            &incoming,
            expiry - Duration::from_millis(1)
        ),
        Some(other)
    );
    assert_eq!(
        next_peer(
            peers.iter().copied(),
            &preferred,
            &no_attempts,
            &incoming,
            expiry
        ),
        Some(peer)
    );
}

#[test]
fn confirmed_inbound_patch_leaves_other_peers_ready_and_releases_on_local_edit() {
    let (left, right) = team(1);
    let (sender, receiver) = if left.core.snapshot().device > right.core.snapshot().device {
        (&left, &right)
    } else {
        (&right, &left)
    };
    let peer = sender.core.listen;
    let other = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1);
    let peers = BTreeSet::from([peer, other]);
    let preferred = BTreeSet::from([peer]);
    let attempts = BTreeMap::new();
    let wake = receiver.core.store.lock().unwrap().subscribe_calendar();
    measured_exchange(sender, receiver);
    let change = |replica: &Replica, year, text: &str| {
        replica
            .core
            .store
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                let plan = state
                    .workspace_mut()?
                    .calendar
                    .plans
                    .values_mut()
                    .next()
                    .unwrap();
                plan.years.entry(year).or_default().note =
                    Some(Record::new(revision, text.to_owned()));
                Ok(())
            })
            .unwrap();
    };
    change(sender, 2026, "Исходная правка");
    measured_exchange(sender, receiver);
    assert!(wake.take_change());
    let now = Instant::now();
    {
        let incoming = receiver.core.inbound_schedule(now);
        assert!(incoming[&peer].confirmed.is_some());
        assert_eq!(
            next_peer(peers.iter().copied(), &preferred, &attempts, &incoming, now),
            Some(other)
        );
    }

    change(receiver, 2027, "Конкурентная правка");
    assert!(wake.take_change());
    let now = Instant::now();
    {
        let incoming = receiver.core.inbound_schedule(now);
        assert!(incoming.is_empty());
        assert_eq!(
            next_peer(peers.iter().copied(), &preferred, &attempts, &incoming, now),
            Some(peer)
        );
    }
    measured_exchange(receiver, sender);
    assert_eq!(
        workspace(&receiver.core.snapshot()).unwrap().calendar,
        workspace(&sender.core.snapshot()).unwrap().calendar
    );
}

#[test]
fn stale_receipt_cannot_defer_a_new_calendar_or_resumed_network() {
    let (local, remote) = team(1);
    let peer = remote.core.listen;
    let capture = || {
        let snapshot = local.core.snapshot();
        ConfirmedInbound {
            workspace: Arc::downgrade(snapshot.workspace.as_ref().unwrap()),
            generation: local.core.generation.load(Ordering::Acquire),
        }
    };
    let record = |proof| {
        local.core.inbound_exchange(
            local.core.snapshot().device,
            remote.core.snapshot().device,
            peer,
            RECENT_INBOUND,
            Some(proof),
        );
    };

    let prior = capture();
    local
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let id = state.device;
            state
                .workspace_mut()?
                .calendar
                .members
                .insert(id, Record::new(revision, "Новое имя".into()));
            Ok(())
        })
        .unwrap();
    record(prior);
    assert!(local.core.inbound_schedule(Instant::now()).is_empty());

    let before_pause = capture();
    local.core.change_pause(true).unwrap();
    local.core.change_pause(false).unwrap();
    record(before_pause);
    assert!(local.core.inbound_schedule(Instant::now()).is_empty());
}

#[test]
fn overlapping_inbound_delta_holds_a_peer_until_last_guard() {
    let local = Replica::new();
    let peer = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 45100);
    let first = local.core.receive_delta(peer);
    let deadline = local.core.receiving_deltas.lock().unwrap()[&peer].defer_until;
    let second = local.core.receive_delta(peer);
    assert_eq!(local.core.receiving_deltas.lock().unwrap()[&peer].defer_until, deadline);
    drop(first);
    assert!(local.core.receiving_deltas.lock().unwrap().contains_key(&peer));
    drop(second);
    assert!(local.core.receiving_deltas.lock().unwrap().is_empty());
}

fn accept_test_connection(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + SESSION_LIMIT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("Сетевой тест не дождался подключения: {error}"),
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn setting_a_timeout_preserves_a_pending_peer_reset() {
    for set_timeout in [TcpStream::set_read_timeout, TcpStream::set_write_timeout] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut receiver, _) = listener.accept().unwrap();
        sender.set_read_timeout(Some(IO_POLL)).unwrap();
        receiver.set_read_timeout(Some(IO_POLL)).unwrap();
        sender.write_all(b"test session").unwrap();
        receiver.read_exact(&mut [0; 4]).unwrap();
        // Dropping the peer with unread data makes it reset the test connection.
        drop(receiver);

        let deadline = Instant::now() + IO_POLL;
        let error = loop {
            match set_io_timeout(&sender, IO_POLL, set_timeout) {
                Err(error) => break error,
                Ok(()) => {
                    assert!(Instant::now() < deadline, "still waiting for a reset");
                    thread::yield_now();
                }
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }
}

#[test]
fn legacy_peer_is_rechecked_after_upgrade() {
    let (leader, colleague) = team(0);
    colleague.listener.set_nonblocking(true).unwrap();
    let mut protocol = PeerProtocol::Unknown;
    thread::scope(|scope| {
        let old_listener = scope.spawn(|| {
            let mut probe = accept_test_connection(&colleague.listener);
            let mut preamble = [0; 25];
            probe.set_read_timeout(Some(SESSION_LIMIT)).unwrap();
            probe.read_exact(&mut preamble).unwrap();
            assert_eq!(preamble[8], Operation::DigestSync as u8);
            // A 0.4 listener closes an unrecognized operation before Noise.
            drop(probe);
            let legacy = accept_test_connection(&colleague.listener);
            respond(&colleague.core, legacy, Ipv4Addr::LOCALHOST).unwrap();
        });
        sync_peer(&leader.core, colleague.core.listen, &mut protocol).unwrap();
        assert!(matches!(protocol, PeerProtocol::LegacyUntil(_)));
        old_listener.join().unwrap();
    });

    protocol = PeerProtocol::LegacyUntil(Instant::now());
    thread::scope(|scope| {
        let upgraded = scope.spawn(|| {
            let stream = accept_test_connection(&colleague.listener);
            respond(&colleague.core, stream, Ipv4Addr::LOCALHOST).unwrap();
        });
        sync_peer(&leader.core, colleague.core.listen, &mut protocol).unwrap();
        assert!(matches!(protocol, PeerProtocol::Digest));
        upgraded.join().unwrap();
    });
}

#[test]
fn mutual_delta_reproduces_both_complete_snapshots_before_merging() {
    use crate::model::{Plan, YearChange};
    let (left, right) = team(1000);
    let initial = workspace(&left.core.snapshot()).unwrap().calendar.latest();
    let common = *workspace(&left.core.snapshot())
        .unwrap()
        .calendar
        .plans
        .keys()
        .next()
        .unwrap();
    let expected_merge = || {
        let mut expected = workspace(&left.core.snapshot()).unwrap().calendar.clone();
        expected.merge(workspace(&right.core.snapshot()).unwrap().calendar.clone());
        expected
    };
    let check = |expect_small: bool| {
        let expected = expected_merge();
        let (sent, received) = measured_exchange(&left, &right);
        assert!(workspace(&left.core.snapshot()).unwrap().calendar == expected, "инициатор должен сохранить обе истории");
        assert!(workspace(&right.core.snapshot()).unwrap().calendar == expected, "ответчик должен сохранить обе истории");
        if expect_small {
            assert!(sent + received < 12_000, "{sent} + {received}");
        } else {
            assert!(sent + received > 100_000, "{sent} + {received}");
        }
        assert_eq!(measured_exchange(&left, &right), (68, 87));
    };
    assert_eq!(measured_exchange(&left, &right), (68, 87));
    for (index, local) in [&left, &right].into_iter().enumerate() {
        local
            .core
            .store
            .lock()
            .unwrap()
            .transaction(|s| {
                let stamp = s.revision()?;
                let me = s.device;
                let cal = &mut s.workspace_mut()?.calendar;
                let plan = cal.plans.get_mut(&common).unwrap();
                let task_id = plan.templates[&2026].value.as_ref().unwrap().tasks[index].id;
                let year = plan.years.entry(2026).or_default();
                year.checks.insert(task_id, Record::new(stamp, true));
                year.note = Some(Record::new(stamp, format!("Согласование {index}")));
                if index == 0 {
                    year.change = Some(Record::new(stamp, YearChange::Cancel));
                }
                let mut added = Plan::default();
                added.years.entry(2027).or_default().note =
                    Some(Record::new(stamp, format!("Новый план {index}")));
                cal.plans.insert(Id::from_u128(11 + index as u128), added);
                cal.members
                    .insert(me, Record::new(stamp, format!("Сотрудник {index}")));
                Ok(())
            })
            .unwrap();
    }
    check(true);
    for (index, local) in [&left, &right].into_iter().enumerate() {
        local
            .core
            .store
            .lock()
            .unwrap()
            .transaction(|s| {
                let mut stamp = s.revision()?;
                if index == 1 {
                    stamp.sequence = initial;
                }
                let cal = &mut s.workspace_mut()?.calendar;
                let mut late = Plan::default();
                late.years.entry(2025).or_default().note =
                    Some(Record::new(stamp, format!("Поздний импорт {index}")));
                cal.plans.insert(Id::from_u128(21 + index as u128), late);
                Ok(())
            })
            .unwrap();
    }
    check(false);
    for local in [&left, &right] {
        local
            .core
            .store
            .lock()
            .unwrap()
            .transaction(|s| {
                let stamp = s.revision()?;
                for plan in s.workspace_mut()?.calendar.plans.values_mut().take(65) {
                    plan.years.entry(2028).or_default().note =
                        Some(Record::new(stamp, "Большое обновление".into()));
                }
                Ok(())
            })
            .unwrap();
    }
    check(false);
}

#[test]
fn verified_calendar_from_a_prior_generation_cannot_be_applied() {
    let (left, right) = team(20);
    let state = right.core.snapshot();
    let remote = workspace(&state).unwrap();
    let packet = SyncPacket {
        device: state.device,
        workspace: remote.id,
        port: right.core.listen.port(),
        calendar: remote.calendar.clone(),
    };
    let mut previous = remote.clone();
    previous.key_generation += 1;
    assert!(
        left.core
            .merge_in_scope(packet, Some(delta::PeerScope::new(&previous, state.device)))
            .is_err()
    );
    assert_eq!(
        workspace(&left.core.snapshot()).unwrap().calendar,
        remote.calendar
    );
}

#[test]
fn a_verified_one_way_image_cannot_hide_a_local_winning_record() {
    let (sender, receiver) = team(32);
    let id = *workspace(&sender.core.snapshot())
        .unwrap()
        .calendar
        .plans
        .keys()
        .next()
        .unwrap();
    receiver
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            state
                .workspace_mut()?
                .calendar
                .plans
                .get_mut(&id)
                .unwrap()
                .years
                .entry(2027)
                .or_default()
                .note = Some(Record::new(revision, "Сохранить и после обмена".into()));
            Ok(())
        })
        .unwrap();
    measured_exchange(&receiver, &sender);
    assert_eq!(measured_exchange(&sender, &receiver), (68, 87));

    // An imported replacement for one plan can omit part of the previous local history.
    sender
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let plan = state.workspace_mut()?.calendar.plans.get_mut(&id).unwrap();
            plan.years.remove(&2027);
            plan.years.entry(2028).or_default().note =
                Some(Record::new(revision, "Согласована дата".into()));
            Ok(())
        })
        .unwrap();
    let offered_digest = sender
        .core
        .calendar_digest(&sender.core.snapshot())
        .unwrap();
    measured_exchange(&sender, &receiver);
    for replica in [&sender, &receiver] {
        let state = replica.core.snapshot();
        let calendar = &workspace(&state).unwrap().calendar;
        let years = &calendar.plans[&id].years;
        assert_eq!(
            years[&2027].note.as_ref().unwrap().value,
            "Сохранить и после обмена"
        );
        assert_eq!(
            years[&2028].note.as_ref().unwrap().value,
            "Согласована дата"
        );
        let actual = calendar_digest(calendar).unwrap();
        assert_ne!(offered_digest, actual);
        assert_eq!(replica.core.calendar_digest(&state).unwrap(), actual);
    }
    assert_eq!(measured_exchange(&sender, &receiver), (68, 87));
}

#[test]
fn publishing_a_new_workspace_fences_cached_and_verified_digests() {
    let (local, _) = team(1);
    let core = &local.core;
    let original = core.snapshot();
    let original_workspace = original.workspace.as_ref().unwrap();
    let prior_digest = core.calendar_digest(&original).unwrap();
    let approved = || CachedDigest {
        workspace: Arc::downgrade(original_workspace),
        digest: prior_digest,
    };
    let barrier = std::sync::Barrier::new(2);
    thread::scope(|scope| {
        let writer = scope.spawn(|| {
            core.store
                .lock()
                .unwrap()
                .transaction(|state| {
                    let employee = state.device;
                    let revision = state.revision()?;
                    state
                        .workspace_mut()?
                        .calendar
                        .members
                        .insert(employee, Record::new(revision, "Новый участник".to_owned()));
                    Ok(())
                })
                .unwrap();
            let newer = core.snapshot();
            let digest = core.calendar_digest(&newer).unwrap();
            barrier.wait();
            (newer, digest)
        });
        barrier.wait();
        let (newer, new_digest) = writer.join().unwrap();
        assert_ne!(prior_digest, new_digest);
        // Completing the older exchange must preserve the newer cached revision.
        assert_eq!(
            core.calendar_digest_with(&original, Some(approved()))
                .unwrap(),
            prior_digest
        );
        {
            let cache = core.digest.lock().unwrap();
            let entry = cache.as_ref().unwrap();
            assert!(
                entry
                    .workspace
                    .ptr_eq(&Arc::downgrade(newer.workspace.as_ref().unwrap()))
            );
            assert_eq!(entry.digest, new_digest);
        }
        // Even after eviction the older proof cannot label the new allocation.
        *core.digest.lock().unwrap() = None;
        assert_eq!(
            core.calendar_digest_with(&newer, Some(approved())).unwrap(),
            new_digest
        );
    });
}

#[test]
fn verified_one_way_patch_reconstructs_omitted_plans_after_base_changes() {
    let (local, peer) = team(2);
    let initial = local.core.snapshot();
    let base = &workspace(&initial).unwrap().calendar;
    let omitted = *base.plans.keys().next().unwrap();
    let changed = *base.plans.keys().last().unwrap();
    let remote = peer.core.snapshot();
    let remote_workspace = workspace(&remote).unwrap();
    let mut patch = Calendar {
        members: base.members.clone(),
        ..Calendar::default()
    };
    let mut plan = base.plans[&changed].clone();
    plan.years.entry(2026).or_default().note = Some(Record::new(
        remote.revision().unwrap(),
        "Синхронизированная запись".into(),
    ));
    patch.plans.insert(changed, plan);
    let image = delta::restore_plans(base, patch.clone(), None);
    let packet = SyncPacket {
        device: remote.device,
        workspace: remote_workspace.id,
        port: peer.core.listen.port(),
        calendar: patch,
    };
    let scope = delta::PeerScope::new(remote_workspace, remote.device);
    let mut store = local.core.store.lock().unwrap();
    thread::scope(|threads| {
        let (waiting, started) = mpsc::sync_channel(0);
        let core = &local.core;
        let pending = threads.spawn(move || {
            waiting.send(()).unwrap();
            core.merge_received(packet, Some(scope), Some(base))
        });
        started.recv().unwrap();
        store
            .transaction(|state| {
                // A concurrent replacement can omit a plan from the verified old base.
                state.workspace_mut()?.calendar.plans.remove(&omitted);
                Ok(())
            })
            .unwrap();
        assert!(
            !workspace(&store.snapshot())
                .unwrap()
                .calendar
                .plans
                .contains_key(&omitted)
        );
        drop(store);
        assert!(pending.join().unwrap().unwrap().is_none());
    });
    assert_eq!(workspace(&local.core.snapshot()).unwrap().calendar, image);
}

#[test]
fn verified_one_way_patch_enforces_final_roster_and_plan_history_limits() {
    use crate::model::Plan;

    let (local, peer) = team(2);
    let peer_state = peer.core.snapshot();
    let peer_id = peer_state.device;
    let remote = workspace(&peer_state).unwrap();
    let scope = delta::PeerScope::new(remote, peer_id);
    let profile = local._profile.path().join("profile").join("calendar.json");
    for fill_roster in [true, false] {
        local
            .core
            .store
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                let calendar = &mut state.workspace_mut()?.calendar;
                if fill_roster {
                    for index in 0..254 {
                        let id = Id::from_u128(10_000 + index);
                        calendar
                            .members
                            .insert(id, Record::new(revision, "Анна".into()));
                    }
                } else {
                    let plan = calendar.plans.values_mut().next().unwrap();
                    for year in 1900..2050 {
                        plan.templates.insert(year, Record::new(revision, None));
                    }
                }
                Ok(())
            })
            .unwrap();
        let baseline = local.core.snapshot();
        let base = &workspace(&baseline).unwrap().calendar;
        let revision = baseline.revision().unwrap();
        let mut patch = Calendar {
            members: base.members.clone(),
            ..Calendar::default()
        };
        if fill_roster {
            patch.members.retain(|&id, _| id == peer_id);
            let new_device = Id::from_u128(20_000);
            patch
                .members
                .insert(new_device, Record::new(revision, "Дарья".into()));
            patch
                .device_people
                .insert(new_device, Record::new(revision, peer_id));
        } else {
            let (&plan_id, _) = base.plans.first_key_value().unwrap();
            let mut plan = Plan::default();
            plan.templates.insert(2050, Record::new(revision, None));
            patch.plans.insert(plan_id, plan);
        }
        delta::restore_plans(base, patch.clone(), None)
            .validate()
            .unwrap();
        let before = std::fs::read(&profile).unwrap();
        let error = local
            .core
            .merge_received(
                SyncPacket {
                    device: peer_id,
                    workspace: remote.id,
                    port: peer.core.listen.port(),
                    calendar: patch,
                },
                Some(scope),
                Some(base),
            )
            .unwrap_err();
        let expected = if fill_roster {
            "числа участников"
        } else {
            "истории события"
        };
        assert!(error.to_string().contains(expected), "{error}");
        assert!(Arc::ptr_eq(&baseline, &local.core.snapshot()));
        assert_eq!(std::fs::read(&profile).unwrap(), before);
    }
}

#[test]
fn verified_one_way_patch_cannot_commit_behind_revocation_holding_the_store_lock() {
    let (local, peer) = team(2);
    let initial = local.core.snapshot();
    let base = &workspace(&initial).unwrap().calendar;
    let peer_state = peer.core.snapshot();
    let remote = workspace(&peer_state).unwrap();
    let mut patch = Calendar {
        members: base.members.clone(),
        ..Calendar::default()
    };
    let (&id, plan) = base.plans.first_key_value().unwrap();
    let mut updated = plan.clone();
    updated.years.entry(2026).or_default().note = Some(Record::new(
        peer_state.revision().unwrap(),
        "Правка, ожидающая сохранения".into(),
    ));
    patch.plans.insert(id, updated);
    let scope = delta::PeerScope::new(remote, peer_state.device);
    let packet = SyncPacket {
        device: peer_state.device,
        workspace: remote.id,
        port: peer.core.listen.port(),
        calendar: patch,
    };
    let mut store = local.core.store.lock().unwrap();
    let successor = thread::scope(|threads| {
        let (waiting, started) = mpsc::sync_channel(0);
        let core = &local.core;
        let writer = threads.spawn(move || {
            waiting.send(()).unwrap();
            core.merge_received(packet, Some(scope), Some(base))
        });
        started.recv().unwrap();
        // An already admitted network operation may still point at the
        // retired store. The mutex and retired-profile fence must reject it.
        store
            .renew_workspace_key(&BTreeSet::from([peer_state.device]))
            .unwrap();
        let successor = store.successor_directory().unwrap();
        drop(store);
        let error = writer.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("Доступ перевыпущен"), "{error}");
        successor
    });
    assert_eq!(&workspace(&local.core.snapshot()).unwrap().calendar, base);
    let active = Store::open_existing(&successor).unwrap();
    let state = active.lock().unwrap().snapshot();
    let renewed = workspace(&state).unwrap();
    assert!(renewed.is_revoked(peer_state.device));
    assert_eq!(&renewed.calendar, base);
}

#[test]
fn revocation_while_negotiating_cancels_both_mutual_and_full_fallback() {
    // The first frame is SyncHello. The second is this initiator's accepted
    // ID union or decline. Revoke after that frame reached the socket, before
    // the unchanged transport can write any snapshot to the former member.
    struct RevokeAfterAgreement<'a> {
        connection: Connection<'a>,
        remote: Id,
        frames: usize,
    }
    impl Read for RevokeAfterAgreement<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.connection.read(buf)
        }
    }
    impl Write for RevokeAfterAgreement<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.connection.write_all(buf)?;
            self.frames += 1;
            if self.frames == 2 {
                let core = self.connection.core;
                core.pause_for_renewal().unwrap();
                core.store
                    .lock()
                    .unwrap()
                    .renew_workspace_key(&BTreeSet::from([self.remote]))
                    .unwrap();
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.connection.flush()
        }
    }
    for (know_base, decline) in [(true, false), (false, false), (false, true)] {
        let (left, right) = team(80);
        measured_exchange(&left, &right);
        if !know_base {
            left.core.delta_baselines.clear_for_test();
        }
        for (idx, p) in [&left, &right].into_iter().enumerate() {
            p.core
                .store
                .lock()
                .unwrap()
                .transaction(|state| {
                    let rev = state.revision()?;
                    let plan = state
                        .workspace_mut()?
                        .calendar
                        .plans
                        .values_mut()
                        .nth(idx)
                        .unwrap();
                    plan.years.entry(2026).or_default().note =
                        Some(Record::new(rev, format!("Локальная запись {idx}")));
                    Ok(())
                })
                .unwrap();
        }
        if decline {
            // More than 64 local changes force the requester to decline the ID
            // union even if the responder can supply its previous confirmation.
            left.core
                .store
                .lock()
                .unwrap()
                .transaction(|state| {
                    let revision = state.revision()?;
                    for plan in state.workspace_mut()?.calendar.plans.values_mut().take(65) {
                        plan.years.entry(2028).or_default().note =
                            Some(Record::new(revision, "Большое изменение".to_owned()));
                    }
                    Ok(())
                })
                .unwrap();
        }
        let prior = right.core.snapshot();
        let captured = left.core.snapshot();
        let ws = workspace(&captured).unwrap();
        thread::scope(|threads| {
            let inbound = threads.spawn(|| {
                let (socket, _) = right.listener.accept().unwrap();
                respond(&right.core, socket, Ipv4Addr::LOCALHOST)
            });
            let (connection, mut transport) = connect(
                &left.core,
                right.core.listen,
                Operation::DigestSync,
                ws.id,
                ChannelKey::Group(&ws.key),
                Instant::now() + SESSION_LIMIT,
                None,
            )
            .unwrap();
            assert!(connection.capabilities.contains(Capabilities::MUTUAL_DELTA));
            let capabilities = connection.capabilities;
            let mut peer = RevokeAfterAgreement {
                connection,
                remote: prior.device,
                frames: 0,
            };
            assert!(
                delta::exchange(
                    &left.core,
                    &mut peer,
                    &mut transport,
                    &captured,
                    Ipv4Addr::LOCALHOST,
                    delta::Role::Initiator,
                    capabilities
                )
                .is_err()
            );
            assert_eq!(peer.frames, 2);
            assert_eq!(
                peer.connection.timeout().unwrap_err().kind(),
                io::ErrorKind::ConnectionAborted
            );
            drop(peer);
            assert!(inbound.join().unwrap().is_err());
        });
        assert_eq!(
            workspace(&right.core.snapshot()).unwrap().calendar,
            workspace(&prior).unwrap().calendar
        );
        let successor = left
            .core
            .store
            .lock()
            .unwrap()
            .successor_directory()
            .unwrap();
        let active = Store::open_existing(&successor).unwrap();
        let stored = active.lock().unwrap().snapshot();
        assert!(
            workspace(&stored)
                .unwrap()
                .require_device_access(prior.device)
                .is_err()
        );
        assert_eq!(workspace(&stored).unwrap().calendar, ws.calendar);
    }
}

#[test]
fn one_cached_peer_handles_cold_changes_in_either_direction() {
    for cold_initiates in [true, false] {
        let (office, laptop) = team(1000);
        measured_exchange(&office, &laptop);
        laptop.core.delta_baselines.clear_for_test();
        for (index, peer) in [&office, &laptop].into_iter().enumerate() {
            peer.core
                .store
                .lock()
                .unwrap()
                .transaction(|state| {
                    let revision = state.revision()?;
                    let book = &mut state.workspace_mut()?.calendar;
                    let plan = book.plans.values_mut().nth(index).unwrap();
                    plan.years.entry(2026).or_default().note =
                        Some(Record::new(revision, format!("Автономная запись {index}")));
                    Ok(())
                })
                .unwrap();
        }
        let mut expected = workspace(&office.core.snapshot()).unwrap().calendar.clone();
        expected.merge(workspace(&laptop.core.snapshot()).unwrap().calendar.clone());
        let (sent, received) = if cold_initiates {
            measured_exchange(&laptop, &office)
        } else {
            measured_exchange(&office, &laptop)
        };
        assert!(sent + received < 12_000, "{sent} + {received}");
        for peer in [&office, &laptop] {
            assert_eq!(workspace(&peer.core.snapshot()).unwrap().calendar, expected);
        }
    }
}

#[test]
fn office_forwards_an_independent_record_at_the_confirmed_sequence() {
    let (office, colleague) = team(1000);
    // team() creates its initial bulk import in a single revision. A normal
    // subsequent office edit establishes a distinct, recent peer baseline.
    office
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let plan = state
                .workspace_mut()?
                .calendar
                .plans
                .values_mut()
                .next()
                .unwrap();
            plan.years.entry(2026).or_default().note =
                Some(Record::new(revision, "Проверено офисом".into()));
            Ok(())
        })
        .unwrap();
    measured_exchange(&office, &colleague);
    let floor = workspace(&office.core.snapshot())
        .unwrap()
        .calendar
        .latest();
    office
        .core
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let mut revision = state.revision()?;
            revision.sequence = floor;
            let plan = state
                .workspace_mut()?
                .calendar
                .plans
                .values_mut()
                .nth(3)
                .unwrap();
            plan.years.entry(2026).or_default().note =
                Some(Record::new(revision, "Подтверждено до подключения".into()));
            Ok(())
        })
        .unwrap();
    let (sent, received) = measured_exchange(&office, &colleague);
    assert!(sent + received < 12000, "{sent} + {received}");
    assert_eq!(
        workspace(&office.core.snapshot()).unwrap().calendar,
        workspace(&colleague.core.snapshot()).unwrap().calendar
    );
}

#[cfg(feature = "desktop")]
#[test]
fn window_observes_published_changes_and_first_peer_outside_locks() {
    let (leader, member) = team(1);
    let core = &leader.core;
    let weak = Arc::downgrade(&core.store);
    let count = Arc::new(AtomicUsize::default());
    let seen = Arc::clone(&count);
    let observer: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let store = weak.upgrade().unwrap();
        assert!(store.try_lock().is_ok());
        seen.fetch_add(1, Ordering::Relaxed);
    });
    core.reader.observe_window(&observer);
    let state = member.core.snapshot();
    let remote = workspace(&state).unwrap();
    let packet = |calendar| SyncPacket {
        device: state.device,
        workspace: remote.id,
        port: member.core.listen.port(),
        calendar,
    };
    core.merge(packet(remote.calendar.clone())).unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 0);
    let mut changed = remote.calendar.clone();
    let revision = state.revision().unwrap();
    changed
        .members
        .insert(state.device, Record::new(revision, "Изменение".into()));
    let mut outdated = remote.clone();
    outdated.key_generation += 1;
    assert!(
        core.merge_in_scope(
            packet(changed.clone()),
            Some(delta::PeerScope::new(&outdated, state.device))
        )
        .is_err()
    );
    assert_eq!(count.load(Ordering::Relaxed), 0);
    core.merge(packet(changed.clone())).unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    core.merge(packet(changed)).unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    core.synchronized(state.device);
    core.synchronized(state.device);
    assert_eq!(count.load(Ordering::Relaxed), 2);
    core.connected
        .lock()
        .unwrap()
        .insert(state.device, Instant::now() - DISCOVERY_TTL);
    core.synchronized(state.device);
    assert_eq!(count.load(Ordering::Relaxed), 3);
    drop(observer);
    assert_eq!(Arc::strong_count(&count), 1);
}
