use std::{
    net::{Ipv4Addr, SocketAddrV4},
    thread,
    time::{Duration, Instant},
};

use chrono::NaiveDate;
use godovoy_plan::{
    model::{Category, Event, Id, Record, Task},
    network::{Config, Network},
    storage::{ProfileOpenIssue, SharedStore, Store},
};
use tempfile::TempDir;

struct Device {
    network: Network,
    store: SharedStore,
    profile: TempDir,
}

impl Device {
    fn new() -> Self {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(&profile.path().join("profile")).unwrap();
        let network = Network::start(
            store.clone(),
            Config {
                listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
                discovery_port: None,
            },
        )
        .unwrap();
        Self {
            network,
            store,
            profile,
        }
    }

    fn join(
        &self,
        code: &str,
        name: &str,
        address: Option<SocketAddrV4>,
    ) -> godovoy_plan::Fallible<()> {
        self.network
            .join_async(code.to_owned(), name.to_owned(), address)
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
    }

    fn completed(&self, event: Id) -> Option<usize> {
        self.store
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .event(event, 2026)
            .map(|event| event.completed())
    }
}

fn eventually(condition: impl Fn() -> bool) {
    let limit = Instant::now() + Duration::from_secs(12);
    while Instant::now() < limit {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(condition(), "изменения не пришли за 12 секунд");
}

#[test]
fn one_use_invite_offline_edits_and_relay_to_another_peer() {
    let host = Device::new();
    let colleague = Device::new();
    let third = Device::new();
    host.store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();

    let code = host
        .network
        .make_invite(&colleague.network.pairing_request())
        .unwrap();
    // A forwarded invitation does not grant access, even before its intended recipient uses it.
    assert!(third.join(&code, "Вера", host.network.address()).is_err());
    assert!(third.store.lock().unwrap().state().workspace.is_none());
    assert_eq!(
        host.store
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .members
            .len(),
        1
    );
    colleague.join(&code, "Борис", None).unwrap();
    assert!(third.join(&code, "Вера", host.network.address()).is_err());

    host.network.set_paused(true);
    let next_code = colleague
        .network
        .make_invite(&third.network.pairing_request())
        .unwrap();
    third.join(&next_code, "Вера", None).unwrap();
    third.network.set_paused(true);

    let event = Id::new_v4();
    let first = Id::new_v4();
    let second = Id::new_v4();
    host.store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let owner = state.device;
            let calendar = &mut state.workspace_mut()?.calendar;
            let plan = calendar.plans.entry(event).or_default();
            plan.templates.insert(
                2026,
                Record::new(
                    revision,
                    Some(std::sync::Arc::new(Event {
                        title: "Годовая акция".into(),
                        description: String::new(),
                        category: Category::Campaign,
                        date: NaiveDate::from_ymd_opt(2026, 12, 20).unwrap(),
                        yearly: true,
                        preparation_days: 45,
                        owner: Some(owner),
                        tasks: vec![
                            Task {
                                id: first,
                                title: "Утвердить план".into(),
                                days_before: 30,
                            },
                            Task {
                                id: second,
                                title: "Заказать материалы".into(),
                                days_before: 20,
                            },
                        ],
                    })),
                ),
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(colleague.completed(event), None);
    host.network.set_paused(false);
    eventually(|| colleague.completed(event) == Some(0));

    host.network.set_paused(true);
    colleague.network.set_paused(true);
    for (device, task) in [(&host, first), (&colleague, second)] {
        device
            .store
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                let plan = state
                    .workspace_mut()?
                    .calendar
                    .plans
                    .get_mut(&event)
                    .unwrap();
                plan.years
                    .entry(2026)
                    .or_default()
                    .checks
                    .insert(task, Record::new(revision, true));
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(host.completed(event), Some(1));
    assert_eq!(colleague.completed(event), Some(1));
    assert_eq!(third.completed(event), None);

    host.network.set_paused(false);
    colleague.network.set_paused(false);
    eventually(|| host.completed(event) == Some(2) && colleague.completed(event) == Some(2));
    host.network.set_paused(true);
    third.network.set_paused(false);
    eventually(|| third.completed(event) == Some(2) && third.network.status().recent_peers == 1);

    let disk = std::fs::read_to_string(third.profile.path().join("profile/calendar.json")).unwrap();
    let persisted: godovoy_plan::storage::LocalState = serde_json::from_str(&disk).unwrap();
    let calendar = persisted.workspace.unwrap().calendar;
    assert_eq!(calendar.event(event, 2026).unwrap().completed(), 2);
    assert_eq!(calendar.event(event, 2027).unwrap().completed(), 0);
}

#[test]
fn a_profile_is_saved_atomically_and_locked_for_single_process() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let store = Store::open(&profile).unwrap();
    let id = store.lock().unwrap().state().device;
    let error = Store::open(&profile)
        .err()
        .expect("profile lock should block a second open");
    assert_eq!(
        error.downcast_ref::<ProfileOpenIssue>(),
        Some(&ProfileOpenIssue::Locked)
    );
    store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    drop(store);
    let reopened = Store::open(&profile).unwrap();
    let store = reopened.lock().unwrap();
    assert_eq!(store.state().device, id);
    assert_eq!(store.state().workspace.as_ref().unwrap().name, "Команда");
}

#[test]
fn failed_enrollment_response_can_be_recovered_with_fresh_code() {
    let host = Device::new();
    let client = Device::new();
    host.store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    let initial = host
        .network
        .make_invite(&client.network.pairing_request())
        .unwrap();
    client.join(&initial, "Борис", None).unwrap();
    // Model a synthetic receiver that has retained its identity but could not save the response.
    client
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.workspace = None;
            state.peers.clear();
            Ok(())
        })
        .unwrap();
    let Device {
        network,
        store,
        profile,
    } = host;
    drop(network);
    drop(store);
    let host_store = Store::open(&profile.path().join("profile")).unwrap();
    let network = Network::start(
        host_store.clone(),
        Config {
            listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            discovery_port: None,
        },
    )
    .unwrap();
    let fresh = network
        .make_invite(&client.network.pairing_request())
        .unwrap();
    client.join(&fresh, "Борис", None).unwrap();
    assert_eq!(
        host_store
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .members
            .len(),
        2
    );
}

#[test]
fn divergent_profiles_over_plan_limit_keep_both_copies_and_report_reason() {
    use godovoy_plan::model::PLAN_LIMIT_ERROR;

    let host = Device::new();
    let colleague = Device::new();
    host.store
        .lock()
        .unwrap()
        .transaction(|state| state.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    let code = host
        .network
        .make_invite(&colleague.network.pairing_request())
        .unwrap();
    colleague.join(&code, "Борис", None).unwrap();
    host.network.set_paused(true);
    colleague.network.set_paused(true);

    for (device, start) in [(&host, 1), (&colleague, 2000)] {
        device
            .store
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                let calendar = &mut state.workspace_mut()?.calendar;
                for key in start..start + 1100 {
                    calendar
                        .plans
                        .entry(Id::from_u128(key))
                        .or_default()
                        .templates
                        .insert(2026, Record::new(revision, None));
                }
                Ok(())
            })
            .unwrap();
    }
    host.network.set_paused(false);
    colleague.network.set_paused(false);
    eventually(|| {
        [&host, &colleague].iter().any(|device| {
            device
                .network
                .status()
                .merge_error
                .as_deref()
                .is_some_and(|error| error.ends_with(PLAN_LIMIT_ERROR))
        })
    });
    for (device, start) in [(&host, 1), (&colleague, 2000)] {
        let snapshot = device.store.lock().unwrap().snapshot();
        let calendar = &snapshot.workspace.as_ref().unwrap().calendar;
        assert_eq!(calendar.plans.len(), 1100);
        assert!(calendar.plans.contains_key(&Id::from_u128(start)));
    }
}

#[test]
fn paused_transport_releases_both_ports_and_failed_resume_can_be_cancelled() {
    use std::net::{TcpListener, UdpSocket};
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(&tmp.path().join("profile")).unwrap();
    store.lock().unwrap().set_sync_paused(true).unwrap();
    let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let tcp_addr = tcp.local_addr().unwrap();
    let udp_addr = udp.local_addr().unwrap();
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, tcp_addr.port()),
        discovery_port: Some(udp_addr.port()),
    };
    // Both ports already belong to other sockets. A saved pause must still open.
    let network = Network::start(store, config).unwrap();
    assert!(network.paused());
    assert!(network.address().is_none());
    assert!(network.set_profile_paused(false).is_err());
    assert!(network.paused());
    assert!(network.status().warning.is_some());
    network.set_profile_paused(true).unwrap();
    drop(tcp);
    drop(udp);
    thread::sleep(Duration::from_millis(3100));
    let tcp = TcpListener::bind(tcp_addr).unwrap();
    let udp = UdpSocket::bind(udp_addr).unwrap();
    assert!(network.set_profile_paused(false).is_err());
    drop(tcp);
    // Allowed resume retries when the explicitly configured TCP port becomes free.
    eventually(|| !network.paused());
    assert!(network.address().is_some());
    assert!(TcpListener::bind(tcp_addr).is_err());
    assert!(network.status().warning.is_some());
    drop(udp);
    eventually(|| network.status().warning.is_none());
    assert!(UdpSocket::bind(udp_addr).is_err());
    network.set_paused(true);
    let tcp = TcpListener::bind(tcp_addr).unwrap();
    let udp = UdpSocket::bind(udp_addr).unwrap();
    assert!(network.address().is_none());
    drop((tcp, udp));
    network.set_paused(false);
    assert!(!network.paused());
    assert!(TcpListener::bind(tcp_addr).is_err());
}
