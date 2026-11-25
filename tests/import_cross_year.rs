#[path = "support/secure.rs"]
mod secure;
use std::{
    net::{Ipv4Addr, SocketAddrV4},
    thread,
    time::{Duration, Instant},
};

use godovoy_plan::{
    api::ApiServer,
    model::{Calendar, Id, Record},
    network::{Config, Network},
    storage::{ApiSettings, SharedStore, Store},
};
use serde_json::{Value, json};

struct Device {
    network: Network,
    api: ApiServer,
    store: SharedStore,
    profile: tempfile::TempDir,
}

impl Device {
    fn new() -> Self {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(&profile.path().join("profile")).unwrap();
        store
            .lock()
            .unwrap()
            .transaction(|state| {
                state.api = Some(ApiSettings {
                    token: [11; 32],
                    import_token: Some([22; 32]),
                    port: 0,
                    enabled: true,
                });
                Ok(())
            })
            .unwrap();
        Self::start(profile, store)
    }

    fn start(profile: tempfile::TempDir, store: SharedStore) -> Self {
        let api = ApiServer::start(store.clone(), 0).unwrap();
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
            api,
            store,
            profile,
        }
    }

    fn reopen(self) -> Self {
        let Self {
            network,
            api,
            store,
            profile,
        } = self;
        drop(network);
        drop(api);
        drop(store);
        let store = Store::open(&profile.path().join("profile")).unwrap();
        Self::start(profile, store)
    }

    fn calendar(&self) -> Calendar {
        self.store
            .lock()
            .unwrap()
            .snapshot()
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .clone()
    }

    fn import(&self, source_id: &str, date: &str, title: &str) -> (Id, bool) {
        self.upsert(source_id, date, title, false)
    }

    fn upsert(&self, source_id: &str, date: &str, title: &str, yearly: bool) -> (Id, bool) {
        let response = secure::put(
            self.api.port(),
            22,
            "/v1/imports/qa",
            &json!({"events": [{
                "externalId": source_id,
                "event": {
                    "date": date,
                    "title": title,
                    "yearly": yearly,
                    "preparationDays": 15,
                    "checklist": [{"key": "qa-task", "title": "Подготовить", "daysBefore": 3}]
                }
            }]}),
        )
        .unwrap();
        assert_eq!(response.as_array().unwrap().len(), 1);
        assert_eq!(response[0]["externalId"], source_id);
        assert_eq!(response[0]["year"], date[0..4].parse::<i32>().unwrap());
        (
            response[0]["id"].as_str().unwrap().parse().unwrap(),
            response[0]["changed"].as_bool().unwrap(),
        )
    }

    fn occurrence(&self, id: Id, year: i32) -> Option<Value> {
        match secure::get(self.api.port(), 11, &format!("/v1/events/{id}?year={year}")) {
            Ok(response) => Some(response),
            Err(secure::Error::Status(404)) => None,
            Err(error) => panic!("unable to inspect {id} in {year}: {error:?}"),
        }
    }

    fn set_progress(&self, id: Id, year: i32, note: &str, done: bool) {
        self.store
            .lock()
            .unwrap()
            .transaction(|state| {
                let revision = state.revision()?;
                let calendar = &mut state.workspace_mut()?.calendar;
                let task = calendar
                    .event(id, year)
                    .ok_or("missing occurrence")?
                    .event
                    .tasks[0]
                    .id;
                let record = calendar
                    .plans
                    .get_mut(&id)
                    .ok_or("missing plan")?
                    .years
                    .entry(year)
                    .or_default();
                record.note = Some(Record::new(revision, note.into()));
                record.checks.insert(task, Record::new(revision, done));
                Ok(())
            })
            .unwrap();
    }
}

struct Pair {
    leader: Device,
    peer: Device,
}

impl Pair {
    fn new() -> Self {
        let leader = Device::new();
        let peer = Device::new();
        leader
            .store
            .lock()
            .unwrap()
            .transaction(|state| state.create_workspace("QA".into(), "Анна".into()))
            .unwrap();
        peer.network
            .join_async(
                leader
                    .network
                    .make_invite(&peer.network.pairing_request())
                    .unwrap(),
                "Борис".into(),
                leader.network.address(),
            )
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        Self { leader, peer }
    }

    fn pause(&self) {
        self.leader.network.set_paused(true);
        self.peer.network.set_paused(true);
    }

    fn resume(&self) {
        self.leader.network.set_paused(false);
        self.peer.network.set_paused(false);
    }

    fn distribute_seed(&self) -> Id {
        let (id, changed) = self.leader.import("same-id", "2026-12-17", "Исходное");
        assert!(changed);
        eventually(|| self.peer.calendar().event(id, 2026).is_some());
        id
    }

    fn await_convergence(&self) {
        eventually(|| self.leader.calendar() == self.peer.calendar());
    }

    fn devices(&self) -> [&Device; 2] {
        [&self.leader, &self.peer]
    }
}

fn eventually(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(condition(), "LAN replicas did not converge");
}

fn assert_occurrence(device: &Device, id: Id, year: i32, date: &str, note: &str, done: bool) {
    let event = device
        .occurrence(id, year)
        .expect("current year must be visible");
    assert_eq!(event["id"], id.to_string());
    assert_eq!(event["date"], date);
    assert_eq!(event["diary"], note, "diary must follow the event");
    assert_eq!(
        event["checklist"][0]["done"], done,
        "check must follow the event"
    );
}

#[test]
fn late_offline_progress_follows_nonrecurring_import_forward_and_backward() {
    let pair = Pair::new();
    let id = pair.distribute_seed();

    pair.pause();
    let first = pair
        .leader
        .import("same-id", "2027-02-20", "Перенесено вперёд");
    assert_eq!(first, (id, true));
    assert_eq!(
        pair.leader
            .import("same-id", "2027-02-20", "Перенесено вперёд"),
        (id, false)
    );
    assert!(pair.leader.occurrence(id, 2026).is_none());
    pair.peer.set_progress(id, 2026, "Офлайн у коллеги", true);
    assert_occurrence(&pair.peer, id, 2026, "2026-12-17", "Офлайн у коллеги", true);
    pair.resume();
    pair.await_convergence();
    for device in pair.devices() {
        assert!(device.occurrence(id, 2026).is_none());
        assert_occurrence(device, id, 2027, "2027-02-20", "Офлайн у коллеги", true);
    }

    pair.pause();
    assert_eq!(
        pair.leader
            .import("same-id", "2025-11-01", "Перенесено назад"),
        (id, true)
    );
    assert_eq!(
        pair.leader
            .import("same-id", "2025-11-01", "Перенесено назад"),
        (id, false)
    );
    pair.peer
        .set_progress(id, 2027, "Последнее изменение", false);
    pair.resume();
    pair.await_convergence();
    for device in pair.devices() {
        assert!(device.occurrence(id, 2026).is_none());
        assert!(device.occurrence(id, 2027).is_none());
        assert_occurrence(device, id, 2025, "2025-11-01", "Последнее изменение", false);
    }
    let reopened = pair.peer.reopen();
    assert!(reopened.occurrence(id, 2026).is_none());
    assert!(reopened.occurrence(id, 2027).is_none());
    assert_occurrence(
        &reopened,
        id,
        2025,
        "2025-11-01",
        "Последнее изменение",
        false,
    );
}

#[test]
fn independently_moved_imported_id_converges_to_one_occurrence() {
    let pair = Pair::new();
    let id = pair.distribute_seed();

    pair.pause();
    // Guarantee a later revision for the chronologically earlier date.
    pair.leader
        .import("separate-event", "2026-01-10", "Отдельное событие");
    assert_eq!(
        pair.leader
            .import("same-id", "2027-02-20", "Победившая версия"),
        (id, true)
    );
    assert_eq!(
        pair.peer
            .import("same-id", "2028-06-08", "Автономная версия"),
        (id, true)
    );
    pair.resume();
    pair.await_convergence();

    for device in pair.devices() {
        let exposed: Vec<_> = (2026..=2028)
            .filter(|year| device.occurrence(id, *year).is_some())
            .collect();
        assert_eq!(exposed, vec![2027], "a one-time event must appear once");
        assert_occurrence(device, id, 2027, "2027-02-20", "", false);
    }
    assert_eq!(
        pair.peer
            .import("same-id", "2027-02-20", "Победившая версия"),
        (id, false),
        "replaying the effective import must be a no-op"
    );
}

#[test]
fn imported_yearly_plan_keeps_separate_notes_and_checks() {
    let pair = Pair::new();
    let (id, changed) = pair
        .leader
        .upsert("annual-id", "2026-12-17", "Ежегодное", true);
    assert!(changed);
    eventually(|| pair.peer.calendar().event(id, 2026).is_some());

    pair.pause();
    pair.leader.set_progress(id, 2026, "Запись за 2026", true);
    pair.peer.set_progress(id, 2027, "Запись за 2027", false);
    pair.resume();
    pair.await_convergence();

    for device in pair.devices() {
        assert_occurrence(device, id, 2026, "2026-12-17", "Запись за 2026", true);
        assert_occurrence(device, id, 2027, "2027-12-17", "Запись за 2027", false);
        assert_occurrence(device, id, 2028, "2028-12-17", "", false);
    }
    assert_eq!(
        pair.peer
            .upsert("annual-id", "2026-12-17", "Ежегодное", true),
        (id, false)
    );
}

#[test]
fn moved_import_uses_a_new_destination_revision_and_keeps_explicit_removal() {
    let pair = Pair::new();
    let id = pair.distribute_seed();
    let device = &pair.leader;
    pair.pause();
    device.set_progress(id, 2026, "До переноса", true);
    device.import("same-id", "2027-01-20", "Вперёд");
    device.import("same-id", "2026-12-18", "Назад");

    // Match records produced by old versions when returning to a former year,
    // followed by receipt of a stale cancellation from a different replica.
    device
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            use godovoy_plan::model::YearChange;
            let plan = state.workspace_mut()?.calendar.plans.get_mut(&id).unwrap();
            let prior = plan.templates[&2026].revision;
            let older = godovoy_plan::model::Revision {
                sequence: prior.sequence - 1,
                ..prior
            };
            plan.years.entry(2026).or_default().change =
                Some(Record::new(older, YearChange::Cancel));
            Ok(())
        })
        .unwrap();
    assert_occurrence(device, id, 2026, "2026-12-18", "До переноса", true);
    assert!(device.occurrence(id, 2027).is_none());

    let result = secure::put(
        device.api.port(),
        22,
        "/v1/imports/qa",
        &json!({"events": [{
            "externalId":"same-id", "event":{"title":"Годовой", "date":"2026-12-18", "yearly":true}
        }]}),
    );
    assert!(matches!(result, Err(secure::Error::Status(422))));
    assert_occurrence(device, id, 2026, "2026-12-18", "До переноса", true);

    device
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let plan = state.workspace_mut()?.calendar.plans.get_mut(&id).unwrap();
            plan.templates.insert(2026, Record::new(revision, None));
            Ok(())
        })
        .unwrap();
    assert!(device.occurrence(id, 2026).is_none());
    assert!(device.occurrence(id, 2027).is_none());
}
