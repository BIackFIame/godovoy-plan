use godovoy_plan::{
    exchange::{export_file, import_file},
    model::{Category, Event, Id, Record},
    network::{Config, Network},
    storage::{SharedStore, Store},
};
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::Path,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn open(path: &Path) -> (SharedStore, Network) {
    let store = Store::open(path).unwrap();
    let net = Network::start(
        store.clone(),
        Config {
            listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            discovery_port: None,
        },
    )
    .unwrap();
    (store, net)
}

fn finish(result: Receiver<godovoy_plan::Fallible<()>>) -> godovoy_plan::Fallible<()> {
    result.recv_timeout(Duration::from_secs(14)).unwrap()
}

fn join(host: &Network, peer: &Network, name: &str) {
    let invite = host.make_invite(&peer.pairing_request()).unwrap();
    finish(peer.join_async(invite, name.to_owned(), host.address())).unwrap();
}

fn add_event(store: &SharedStore, id: Id, title: &str) {
    store
        .lock()
        .unwrap()
        .transaction(|s| {
            let revision = s.revision()?;
            let event = Event {
                title: title.into(),
                description: String::new(),
                category: Category::Campaign,
                date: chrono::NaiveDate::from_ymd_opt(2026, 12, 20).unwrap(),
                yearly: true,
                preparation_days: 45,
                owner: Some(s.device),
                tasks: Vec::new(),
            };
            s.workspace_mut()?
                .calendar
                .plans
                .entry(id)
                .or_default()
                .templates
                .insert(2026, Record::new(revision, Some(event.into())));
            Ok(())
        })
        .unwrap();
}

fn has_event(store: &SharedStore, id: Id) -> bool {
    store
        .lock()
        .unwrap()
        .state()
        .workspace
        .as_ref()
        .unwrap()
        .calendar
        .event(id, 2026)
        .is_some()
}

#[test]
fn trusted_device_rejoins_and_preserves_offline_changes_across_key_epochs() {
    let sandbox = TempDir::new().unwrap();
    let host_dir = sandbox.path().join("host");
    let peer_dir = sandbox.path().join("office");
    let (host_store, host) = open(&host_dir);
    host_store
        .lock()
        .unwrap()
        .transaction(|s| s.create_workspace("Офис".into(), "Анна".into()))
        .unwrap();
    let (peer_store, peer) = open(&peer_dir);
    let (lost_store, lost) = open(&sandbox.path().join("lost"));
    join(&host, &peer, "Иван");
    join(&host, &lost, "Иван"); // Display names can legitimately match on different computers.
    let (spare_store, spare) = open(&sandbox.path().join("spare"));
    join(&host, &spare, "Архивный ПК");
    spare.set_profile_paused(true).unwrap();
    let spare_id = spare_store.lock().unwrap().state().device;
    peer.set_profile_paused(true).unwrap();
    lost.set_profile_paused(true).unwrap();
    let old_key = host_store
        .lock()
        .unwrap()
        .state()
        .workspace
        .as_ref()
        .unwrap()
        .key;
    let prior = sandbox.path().join("prior.yrplan");
    export_file(&host_store, &prior).unwrap();
    let old_backup = host_store.lock().unwrap().create_backup().unwrap();
    let offline = Id::new_v4();
    add_event(&peer_store, offline, "Изменение без сети");

    let lost_id = lost_store.lock().unwrap().state().device;
    let host_id = host_store.lock().unwrap().state().device;
    assert!(finish(host.revoke_devices_async([host_id].into_iter().collect())).is_err());
    assert!(!host.paused());
    finish(host.revoke_devices_async([lost_id, spare_id].into_iter().collect())).unwrap();
    let generation = Store::active_directory(&host_dir).unwrap();
    assert_ne!(generation, host_dir);
    assert!(
        host_store
            .lock()
            .unwrap()
            .restore_open_backup(&old_backup)
            .is_err()
    );
    assert!(
        host_store
            .lock()
            .unwrap()
            .transaction(|s| s.revision())
            .is_err()
    );
    drop(host);
    drop(host_store);
    assert!(Store::open_existing(&host_dir).is_err());
    let (host_store, host) = open(&generation);
    let rejection = host.make_invite(&lost.pairing_request()).unwrap_err();
    assert!(rejection.to_string().contains("отозван"));
    assert!(host.make_invite(&spare.pairing_request()).is_err());
    assert_ne!(
        host_store
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .key,
        old_key
    );
    assert!(
        host_store
            .lock()
            .unwrap()
            .restore_open_backup(&old_backup)
            .is_err()
    );
    host_store.lock().unwrap().create_backup().unwrap();
    assert!(import_file(&host_store, &prior).is_err());
    let fresh = sandbox.path().join("new.yrplan");
    let local = Id::new_v4();
    add_event(&host_store, local, "После перевыпуска");
    export_file(&host_store, &fresh).unwrap();
    assert!(import_file(&lost_store, &fresh).is_err());
    assert!(!has_event(&lost_store, local));
    let invite = host.make_invite(&peer.pairing_request()).unwrap();
    let peer_id = peer_store.lock().unwrap().state().device;
    assert!(
        host_store
            .lock()
            .unwrap()
            .state()
            .admitted_this_generation
            .is_empty()
    );
    finish(peer.rejoin_async(invite, host.address())).unwrap();
    assert_eq!(
        host_store
            .lock()
            .unwrap()
            .state()
            .admitted_this_generation
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![peer_id]
    );
    drop(peer);
    drop(peer_store);
    let (peer_store, peer) = open(&Store::active_directory(&peer_dir).unwrap());
    let (fresh_store, fresh) = open(&sandbox.path().join("new-profile"));
    assert!(
        peer.make_invite(&fresh.pairing_request())
            .unwrap_err()
            .to_string()
            .contains("управляет")
    );
    assert!(
        finish(peer.renew_access_async())
            .unwrap_err()
            .to_string()
            .contains("управляет")
    );
    assert!(finish(peer.revoke_devices_async([host_id].into_iter().collect())).is_err());
    assert!(!peer.paused());
    join(&host, &fresh, "Новый профиль");
    let fresh_workspace = fresh_store
        .lock()
        .unwrap()
        .state()
        .workspace
        .clone()
        .unwrap();
    assert_eq!(fresh_workspace.access_manager, Some(host_id));
    assert!(fresh_workspace.is_revoked(lost_id));
    assert!(fresh.make_invite(&peer.pairing_request()).is_err());
    assert!(
        peer.make_invite(&lost.pairing_request())
            .unwrap_err()
            .to_string()
            .contains("отозван")
    );
    assert!(
        peer_store
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .members
            .contains_key(&lost_id)
    );
    assert!(has_event(&peer_store, local));
    assert!(has_event(&peer_store, offline));
    let end = Instant::now() + Duration::from_secs(12);
    while !has_event(&host_store, offline) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(40));
    }
    assert!(has_event(&host_store, offline));
    lost.set_profile_paused(false).unwrap();
    let stale_invite = lost.make_invite(&peer.pairing_request()).unwrap();
    assert!(finish(peer.rejoin_async(stale_invite, lost.address())).is_err());
    assert!(peer_store.lock().unwrap().successor_directory().is_none());
    assert!(!has_event(&lost_store, local));
    assert!(peer.paused());

    // Rotating more than once leaves the standard launch path pointing to the newest profile.
    finish(host.renew_access_async()).unwrap();
    let newest = Store::active_directory(&host_dir).unwrap();
    assert_ne!(newest, generation);
    assert_eq!(newest.parent(), generation.parent());
    let newest = Store::open_existing(&newest).unwrap();
    assert!(
        newest
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .is_revoked(lost_id)
    );
    assert_eq!(
        newest
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .key_generation,
        2
    );
    assert!(
        newest
            .lock()
            .unwrap()
            .state()
            .admitted_this_generation
            .is_empty()
    );
    assert!(has_event(&newest, offline));
}

#[test]
fn failed_preparation_keeps_old_calendar_and_safe_retry_path() {
    let sandbox = TempDir::new().unwrap();
    let directory = sandbox.path().join("host");
    let (store, network) = open(&directory);
    store
        .lock()
        .unwrap()
        .transaction(|s| s.create_workspace("Команда".into(), "Анна".into()))
        .unwrap();
    let event = Id::new_v4();
    add_event(&store, event, "Встреча");
    let old = store.lock().unwrap().state().workspace.clone();
    let obstruction = directory.join("access-profiles-v1");
    std::fs::write(&obstruction, b"keep").unwrap();
    assert!(finish(network.renew_access_async()).is_err());
    assert!(network.paused());
    assert_eq!(Store::active_directory(&directory).unwrap(), directory);
    assert_eq!(store.lock().unwrap().state().workspace, old);
    assert_eq!(std::fs::read(&obstruction).unwrap(), b"keep");
    std::fs::rename(obstruction, directory.join("saved-note")).unwrap();
    finish(network.renew_access_async()).unwrap();
    let active = Store::open_existing(&Store::active_directory(&directory).unwrap()).unwrap();
    assert!(has_event(&active, event));
}
