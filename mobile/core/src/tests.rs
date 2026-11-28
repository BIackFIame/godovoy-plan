use super::ffi::*;
use chrono::{Datelike, Local};
use godovoy_plan::{
    exchange,
    model::Id,
    network::{Config, Network},
    storage::Store,
};
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString, c_void},
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::Path,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Phone(*mut c_void);

impl Phone {
    fn open(path: &Path) -> Self {
        let path = CString::new(path.to_str().unwrap()).unwrap();
        let handle = unsafe { gp_open(path.as_ptr()) };
        assert!(!handle.is_null(), "open in private container");
        Self(handle)
    }

    fn send(&self, v: Value) -> Value {
        let json = CString::new(v.to_string()).unwrap();
        let ptr = unsafe { gp_request(self.0, json.as_ptr()) };
        assert!(!ptr.is_null());
        let response = serde_json::from_slice(unsafe { CStr::from_ptr(ptr) }.to_bytes()).unwrap();
        unsafe { gp_string_free(ptr) };
        response
    }

    fn ok(&self, v: Value) -> Value {
        let res = self.send(v);
        assert_eq!(res["ok"], true, "{res}");
        res["data"].clone()
    }

    fn reject(&self, v: Value) {
        let res = self.send(v);
        assert_eq!(res["ok"], false, "{res}");
        assert!(res["error"].is_string());
    }

    fn state(&self, year: i32) -> Value {
        self.ok(json!({"op":"state","year":year}))
    }
}

impl Drop for Phone {
    fn drop(&mut self) {
        unsafe { gp_close(self.0) };
    }
}

fn event(owner: &Value) -> Value {
    json!({"op":"createEvent","title":"Поздравления партнёрам","date":"2027-01-01","yearly":true,
        "ownerId":owner,"category":"Partners","preparationDays":90,"description":"Приглашения",
        "checklist":[{"title":"Согласовать список","daysBefore":60}]})
}

#[test]
fn offline_calendar_file_and_abi_contracts() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("phone");
    let phone = Phone::open(&path);
    let state = phone.state(2027);
    assert_eq!(state["notificationDates"], json!([]));
    assert_eq!(
        phone.ok(json!({"op":"state","year":2027,"ifToken":state["stateToken"]})),
        json!({"unchanged":true})
    );
    assert!(
        phone.ok(json!({"op":"state","year":2028,"ifToken":state["stateToken"]}))["stateToken"]
            .is_string()
    );
    let guarded = phone.state(2027);
    phone.reject(json!({"op":"state","year":"2027"}));
    phone.reject(json!({"op":"createWorkspace","employee":"Я","name":" "}));
    phone.ok(json!({"op":"createWorkspace","employee":"Ольга","name":"Офис"}));
    assert_eq!(
        phone.ok(
            json!({"op":"state","year":2027,"ifToken":guarded["stateToken"],"acceptSyncStamp":true})
        )["workspaceName"],
        "Офис"
    );
    phone.ok(event(&state["deviceId"]));
    let before_check = phone.state(2027);
    let first = &before_check["events"][0];
    assert_eq!(first["preparationDate"], "2026-10-03");
    assert_eq!(first["checklist"][0]["dueDate"], "2026-11-02");
    let task = &first["checklist"][0]["id"];
    let id = &first["id"];
    phone.ok(json!({"op":"check","year":2027,"eventId":id,"taskId":task,"done":true}));
    let changed = phone.ok(json!({"op":"state","year":2027,"ifToken":before_check["stateToken"],"acceptSyncStamp":true}));
    assert_ne!(changed["stateToken"], before_check["stateToken"]);
    assert_eq!(changed["events"][0]["checklist"][0]["done"], true);
    let note = json!({"op":"note","year":2027,"eventId":id,"text":"Готово","base":""});
    phone.ok(note.clone());
    phone.ok(note);
    phone.reject(json!({"op":"note","year":2027,"eventId":id,"text":"Переписать","base":""}));
    let repeated = &phone.state(2028)["events"][0];
    assert_eq!(repeated["note"], "");
    assert_eq!(repeated["checklist"][0]["done"], false);
    phone.reject(json!({"op":"import","path":path.join("calendar.json")}));
    let export = phone.ok(json!({"op":"export"}));
    let file = Path::new(export.as_str().unwrap());
    let bytes = fs::read(file).unwrap();
    assert!(bytes.starts_with(b"YRFILE01"));
    assert!(
        !bytes
            .windows("Поздравления".len())
            .any(|b| b == "Поздравления".as_bytes())
    );
    phone.ok(json!({"op":"import","path":file}));
    #[cfg(unix)]
    {
        let link = path
            .join("exchange")
            .join(format!("{}.yrplan", Id::new_v4()));
        std::os::unix::fs::symlink(file, &link).unwrap();
        phone.reject(json!({"op":"import","path":link}));
        fs::remove_file(&link).unwrap();
        fs::hard_link(file, &link).unwrap();
        phone.reject(json!({"op":"import","path":link}));
    }
    phone.reject(json!({"op":"exportProfile"}));
    assert!(phone.state(2027).get("key").is_none());
    drop(phone);
    let phone = Phone::open(&path);
    assert_eq!(phone.state(2027)["events"][0]["note"], "Готово");
    assert!(!file.exists());
}

#[test]
fn actual_desktop_peer_syncs_with_mobile_and_retires_its_key() {
    let tmp = TempDir::new().unwrap();
    let desktop_dir = tmp.path().join("office");
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let store = Store::open(&desktop_dir).unwrap();
    store
        .lock()
        .unwrap()
        .transaction(|s| s.create_workspace("Команда".into(), "Настольный".into()))
        .unwrap();
    let desktop = Network::start(store.clone(), config).unwrap();
    let profile = tmp.path().join("mobile");
    let phone = Phone::open(&profile);
    let request = phone.ok(json!({"op":"pairingRequest"}));
    phone.ok(json!({"op":"background"}));
    phone.ok(json!({"op":"foreground"}));
    assert_eq!(phone.ok(json!({"op":"pairingRequest"})), request);
    let invite = desktop.make_invite(request.as_str().unwrap()).unwrap();
    phone.ok(json!({"op":"join","code":invite,"employee":"Телефон","address":desktop.address()}));
    let state = phone.state(2027);
    assert_eq!(state["members"].as_array().unwrap().len(), 2);
    let id = Id::parse_str(state["deviceId"].as_str().unwrap()).unwrap();
    phone.ok(event(&state["deviceId"]));
    let limit = Instant::now() + Duration::from_secs(20);
    while store
        .lock()
        .unwrap()
        .state()
        .workspace
        .as_ref()
        .unwrap()
        .calendar
        .year(2027)
        .is_empty()
    {
        assert!(
            Instant::now() < limit,
            "Событие с телефона не попало на настольный узел"
        );
        thread::sleep(Duration::from_millis(80));
    }
    phone.ok(json!({"op":"pause","paused":true}));
    phone.ok(json!({"op":"background"}));
    phone.ok(json!({"op":"foreground"}));
    assert_eq!(phone.state(2027)["paused"], true);
    desktop
        .renew_access_async()
        .recv_timeout(Duration::from_secs(15))
        .unwrap()
        .unwrap();
    drop(desktop);
    drop(store);
    let active = Store::active_directory(&desktop_dir).unwrap();
    let store = Store::open_existing(&active).unwrap();
    let desktop = Network::start(store.clone(), config).unwrap();
    let invite = desktop.make_invite(request.as_str().unwrap()).unwrap();
    phone.ok(json!({"op":"rejoin","code":invite,"address":desktop.address()}));
    let renewed = phone.state(2027);
    assert_eq!(renewed["canManage"], false);
    assert_eq!(renewed["events"][0]["title"], "Поздравления партнёрам");
    phone.reject(json!({"op":"renew"}));
    phone.reject(
        json!({"op":"linkEmployeeDevice","deviceId":id,"employeeId":renewed["accessManagerId"]}),
    );
    desktop
        .revoke_devices_async([id].into_iter().collect())
        .recv_timeout(Duration::from_secs(15))
        .unwrap()
        .unwrap();
    drop(desktop);
    drop(store);
    let active = Store::active_directory(&desktop_dir).unwrap();
    let store = Store::open_existing(&active).unwrap();
    let desktop = Network::start(store.clone(), config).unwrap();
    let request = phone.ok(json!({"op":"pairingRequest"}));
    assert!(desktop.make_invite(request.as_str().unwrap()).is_err());
    let staged = profile
        .join("exchange")
        .join(format!("{}.yrplan", Id::new_v4()));
    exchange::export_file(&store, &staged).unwrap();
    phone.reject(json!({"op":"import","path":staged}));
}

#[test]
fn linked_employees_share_personal_work_but_keep_independent_device_access() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("manager");
    let phone = Phone::open(&path);
    phone.ok(json!({"op":"createWorkspace","name":"Отдел","employee":"Руководитель"}));
    let state = phone.state(2027);
    phone.reject(json!({"op":"revoke","deviceIds":[state["deviceId"]]}));
    let store = Store::open(&tmp.path().join("desktop")).unwrap();
    let desktop = Network::start(
        store.clone(),
        Config {
            listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            discovery_port: None,
        },
    )
    .unwrap();
    let request = desktop.pairing_request();
    let invite = phone.ok(json!({"op":"invite","request":request}));
    let address = phone.state(2027)["address"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    desktop
        .join_async(
            invite.as_str().unwrap().into(),
            "Руководитель".into(),
            Some(address),
        )
        .recv_timeout(Duration::from_secs(15))
        .unwrap()
        .unwrap();
    let id = store.lock().unwrap().state().device;
    desktop.set_profile_paused(true).unwrap();
    let own = &state["deviceId"];
    let today = Local::now().date_naive();
    let year = today.year();
    let due = |owner: Value| {
        json!({"op":"createEvent","title":"Общая подготовка",
        "date":today,"yearly":false,"ownerId":owner,"category":"Partners","preparationDays":0,
        "checklist":[{"title":"Отправить","daysBefore":0}]})
    };
    phone.ok(due(json!(id)));
    let before = phone.state(year);
    assert_eq!(
        before["employees"].as_array().unwrap().len(),
        2,
        "equal display names must stay separate"
    );
    assert_eq!(before["tasks"], json!([]));
    assert_eq!(before["notificationDates"], json!([]));

    let link = json!({"op":"linkEmployeeDevice","deviceId":own,"employeeId":id});
    phone.ok(link.clone());
    let joined = phone.state(year);
    assert_ne!(before["stateToken"], joined["stateToken"]);
    assert_eq!(joined["employeeId"], json!(id));
    assert_eq!(joined["employees"].as_array().unwrap().len(), 1);
    assert_eq!(joined["members"].as_array().unwrap().len(), 2);
    assert_eq!(joined["tasks"].as_array().unwrap().len(), 1);
    assert!(!joined["reminders"].as_array().unwrap().is_empty());
    assert_eq!(joined["notificationDates"], json!([today]));
    let browsing = year + 1;
    let linked_view = phone.state(browsing);
    let row = &joined["tasks"][0];
    let check = |view: &Value| {
        json!({"op":"check","viewYear":browsing,"year":year,
        "eventId":row["id"],"taskId":row["checklist"][0]["id"],"done":true,"baseToken":view["stateToken"]})
    };
    phone.ok(json!({"op":"linkEmployeeDevice","deviceId":own,"employeeId":own}));
    phone.reject(check(&linked_view));
    let unlinked = phone.state(browsing);
    phone.reject(check(&unlinked));
    assert_eq!(phone.state(year)["tasks"], json!([]));
    phone.ok(link);
    let before_revocation = phone.state(browsing);

    let stale = phone.ok(json!({"op":"export"}));
    phone.ok(json!({"op":"revoke","deviceIds":[id]}));
    phone.reject(check(&before_revocation));
    let current = phone.state(year);
    assert_eq!(
        current["employees"][0]["id"],
        json!(id),
        "revoked anchor is still a person with an active phone"
    );
    assert_eq!(current["tasks"].as_array().unwrap().len(), 1);
    phone.ok(due(json!(id)));
    phone.reject(json!({"op":"linkEmployeeDevice","deviceId":id,"employeeId":own}));
    assert_eq!(current["accessManagerId"], state["deviceId"]);
    assert_eq!(current["canManage"], true);
    assert!(
        current["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == json!(id) && m["revoked"] == true)
    );
    phone.reject(json!({"op":"invite","request":request}));
    phone.reject(json!({"op":"import","path":stale}));
    drop(phone);
    let phone = Phone::open(&path);
    let reopened = phone.state(year);
    assert_eq!(reopened["accessManagerId"], state["deviceId"]);
    assert_eq!(reopened["tasks"].as_array().unwrap().len(), 2);
    phone.reject(json!({"op":"invite","request":request}));
}

#[test]
fn reopening_a_session_keeps_background_pause_until_foreground() {
    let tmp = TempDir::new().unwrap();
    let mut phone = super::Mobile::open(tmp.path().join("profile")).unwrap();
    phone
        .request(json!({"op":"createWorkspace","name":"Команда","employee":"Анна"}))
        .unwrap();
    phone.request(json!({"op":"pause","paused":false})).unwrap();
    phone.request(json!({"op":"background"})).unwrap();
    phone.session = None;
    let paused =
        |phone: &mut super::Mobile| phone.refresh().unwrap().network.as_ref().unwrap().paused();
    assert!(paused(&mut phone));
    phone.request(json!({"op":"pause","paused":false})).unwrap();
    assert!(paused(&mut phone));
    phone.request(json!({"op":"foreground"})).unwrap();
    assert!(!paused(&mut phone));
}

#[test]
fn fresh_profile_waits_for_an_explicit_local_choice() {
    let tmp = TempDir::new().unwrap();
    let mut phone = super::Mobile::open(tmp.path().join("profile")).unwrap();
    for op in ["foreground", "background", "foreground"] {
        phone.request(json!({"op":op})).unwrap();
        assert!(phone.session.as_ref().unwrap().network.is_none());
    }
    let response = phone.request(json!({"op":"state","year":2027})).unwrap();
    let envelope: Value = serde_json::from_str(&response).unwrap();
    assert!(envelope["data"].get("address").unwrap().is_null());
    phone.request(json!({"op":"pairingRequest"})).unwrap();
    let session = phone.session.as_ref().unwrap();
    assert!(session.network.is_some());
}

#[test]
fn partial_check_requires_exact_view_and_falls_back_after_an_intervening_edit() {
    let tmp = TempDir::new().unwrap();
    let phone = Phone::open(&tmp.path().join("profile"));
    phone.ok(json!({"op":"createWorkspace","employee":"Мария","name":"Офис"}));
    let device = phone.state(2027)["deviceId"].clone();
    phone.ok(event(&device));
    phone.ok(event(&device));
    let base = phone.state(2027);
    let row = base["events"][0].clone();
    let other = base["events"][1].clone();
    let command = |base: &Value, done| json!({"op":"check","year":2027,"eventId":row["id"],"taskId":row["checklist"][0]["id"],"done":done,"baseToken":base["stateToken"]});
    let read = |base: &Value| json!({"op":"state","year":2027,"ifToken":base["stateToken"],"acceptCheckPatch":true});
    phone.ok(command(&base, true));
    let mut patch = phone.ok(read(&base));
    assert_eq!(patch["checkPatch"], true);
    assert_eq!(patch["baseToken"], base["stateToken"]);
    assert_eq!(patch["events"].as_array().unwrap().len(), 1);
    patch.as_object_mut().unwrap().remove("checkPatch");
    patch.as_object_mut().unwrap().remove("baseToken");
    let mut events = base["events"].clone();
    events[0] = patch["events"][0].clone();
    patch["events"] = events;
    for (field, positions, count, key) in [
        ("tasks", "taskPositions", "taskCount", "id"),
        ("reminders", "reminderPositions", "reminderCount", "eventId"),
    ] {
        let object = patch.as_object_mut().unwrap();
        let positions = object.remove(positions).unwrap();
        let updates = object.remove(field).unwrap();
        let updated: std::collections::BTreeMap<_, _> = positions
            .as_array()
            .unwrap()
            .iter()
            .zip(updates.as_array().unwrap())
            .map(|(i, v)| (i.as_u64().unwrap(), v.clone()))
            .collect();
        let count = object.remove(count).unwrap().as_u64().unwrap();
        let mut other_rows = base[field]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e[key] != row["id"]);
        object.insert(
            field.to_owned(),
            Value::Array(
                (0..count)
                    .map(|i| {
                        updated
                            .get(&i)
                            .unwrap_or_else(|| other_rows.next().unwrap())
                            .clone()
                    })
                    .collect(),
            ),
        );
        assert!(other_rows.next().is_none());
    }
    assert_eq!(patch, phone.state(2027));
    phone.ok(command(&patch, false));
    phone.ok(json!({"op":"note","year":2027,"eventId":other["id"],"text":"Коллега изменил дневник","base":""}));
    let full = phone.ok(read(&patch));
    assert!(full.get("checkPatch").is_none());
    assert_eq!(full["events"][1]["note"], "Коллега изменил дневник");
    phone.reject(command(&patch, true));
    assert_eq!(
        phone.state(2027)["events"][0]["checklist"][0]["done"],
        false
    );
    phone.ok(command(&full, false));
    assert_eq!(phone.ok(read(&full)), json!({"unchanged":true}));
    let mobile = unsafe { &*phone.0.cast::<std::sync::Mutex<super::Mobile>>() };
    let store = mobile
        .lock()
        .unwrap()
        .session
        .as_ref()
        .unwrap()
        .store
        .clone();
    let member: Id = serde_json::from_value(device).unwrap();
    store
        .lock()
        .unwrap()
        .transaction(|s| {
            s.workspace_mut()?.calendar.members.remove(&member);
            Ok(())
        })
        .unwrap();
    phone.reject(command(&full, true));
    phone.reject(read(&full));
}

#[test]
fn previous_generation_check_cannot_change_the_replacement_profile() {
    let tmp = TempDir::new().unwrap();
    let phone = Phone::open(&tmp.path().join("office"));
    phone.ok(json!({"op":"createWorkspace","name":"Группа","employee":"Наталья"}));
    let device = phone.state(2027)["deviceId"].clone();
    phone.ok(event(&device));
    let before = phone.state(2027);
    let e = &before["events"][0];
    let command = json!({"op":"check","eventId":e["id"],"year":2027,"taskId":e["checklist"][0]["id"],"done":true,"baseToken":before["stateToken"]});
    let m = unsafe { &*phone.0.cast::<std::sync::Mutex<super::Mobile>>() };
    let store = m.lock().unwrap().session.as_ref().unwrap().store.clone();
    let network = Network::start(
        store.clone(),
        Config {
            listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            discovery_port: None,
        },
    )
    .unwrap();
    network
        .renew_access_async()
        .recv_timeout(Duration::from_secs(8))
        .unwrap()
        .unwrap();
    assert!(store.lock().unwrap().successor_directory().is_some());
    drop(network);
    phone.reject(command);
    let after = phone.ok(
        json!({"op":"state","year":2027,"ifToken":before["stateToken"],"acceptCheckPatch":true}),
    );
    assert!(after.get("checkPatch").is_none());
    assert_ne!(before["stateToken"], after["stateToken"]);
    assert_eq!(after["events"][0]["checklist"][0]["done"], false);
}

#[test]
fn compact_check_is_bound_to_the_requested_year_token_and_client_capability() {
    let tmp = TempDir::new().unwrap();
    let phone = Phone::open(&tmp.path().join("profile"));
    phone.ok(json!({"op":"createWorkspace","employee":"Ольга","name":"Офис"}));
    let device = phone.state(2027)["deviceId"].clone();
    phone.ok(event(&device));
    phone.ok(event(&device));

    for (index, (read_year, supported, correct_token)) in
        [(2028, true, true), (2027, false, true), (2027, true, false)]
            .into_iter()
            .enumerate()
    {
        let prior = phone.state(2027);
        let source = &prior["events"][0];
        let token = &prior["stateToken"];
        let mut check = json!({"op":"check","eventId":source["id"],"year":2028,
            "taskId":source["checklist"][0]["id"],"done":index % 2 == 0,"baseToken":token});
        phone.reject(check.clone());
        check["year"] = json!(2027);
        phone.ok(check);
        let supplied = if correct_token {
            token.clone()
        } else {
            json!(Id::new_v4())
        };
        let result = phone.ok(json!({"op":"state","year":read_year,"ifToken":supplied,
            "acceptCheckPatch":supported}));
        assert!(result.get("checkPatch").is_none());
        assert_eq!(result["year"], read_year);
        assert_eq!(result["events"].as_array().unwrap().len(), 2);
        assert_eq!(result, phone.state(read_year));
    }
}

#[test]
fn reminders_identify_the_exact_checklist_item_even_when_titles_match() {
    let tmp = TempDir::new().unwrap();
    let phone = Phone::open(&tmp.path().join("phone"));
    phone.ok(json!({"op":"createWorkspace","employee":"Анна","name":"Офис"}));
    let today = Local::now().date_naive();
    let year = today.year();
    let owner = phone.state(year)["deviceId"].clone();
    phone.ok(
        json!({"op":"createEvent","title":"Подготовка","date":today,"ownerId":owner,
        "yearly":false,"category":"Partners","preparationDays":0,
        "checklist":[{"title":"Проверка","daysBefore":0},{"title":"Проверка","daysBefore":0}]}),
    );
    let state = phone.state(year);
    let event = &state["events"][0];
    let reminders = state["reminders"].as_array().unwrap();
    assert!(
        reminders
            .iter()
            .any(|r| r["taskId"].is_null() && r["eventId"] == event["id"])
    );
    for task in event["checklist"].as_array().unwrap() {
        assert_eq!(
            reminders
                .iter()
                .filter(|r| r["taskId"] == task["id"] && r["eventId"] == event["id"])
                .count(),
            1
        );
    }
}

#[test]
fn checklist_tuple_snapshots_have_the_same_fields_and_a_distinct_conditional_base() {
    let tmp = TempDir::new().unwrap();
    let phone = Phone::open(&tmp.path().join("profile"));
    phone.ok(json!({"op":"createWorkspace","employee":"Мария","name":"Офис"}));
    let device = phone.state(2027)["deviceId"].clone();
    phone.ok(event(&device));
    let objects = phone.state(2027);
    let compact = phone.ok(
        json!({"op":"state","year":2027,"ifToken":objects["stateToken"],
        "acceptChecklistTuples":true,"acceptSyncStamp":true,"acceptCheckPatch":true}),
    );
    assert_ne!(compact["stateToken"], objects["stateToken"]);
    assert_eq!(compact["checklistFormat"], "tuple-v1");
    let plain = &objects["events"][0]["checklist"][0];
    assert_eq!(
        compact["events"][0]["checklist"][0],
        json!([
            plain["id"],
            plain["title"],
            plain["dueDate"],
            plain["daysBefore"],
            plain["done"]
        ])
    );
    let token = &compact["stateToken"];
    assert_eq!(
        phone.ok(json!({"op":"state","year":2027,"ifToken":token,"acceptChecklistTuples":true})),
        json!({"unchanged":true})
    );
    phone.ok(
        json!({"op":"check","year":2027,"eventId":objects["events"][0]["id"],
        "taskId":plain["id"],"done":true,"baseToken":token}),
    );
    let patch = phone.ok(json!({"op":"state","year":2027,"ifToken":token,
        "acceptCheckPatch":true,"acceptChecklistTuples":true}));
    assert_eq!(patch["checkPatch"], true);
    assert_eq!(patch["events"][0]["checklist"][0][4], true);
    let full = phone.ok(
        json!({"op":"state","year":2027,"ifToken":patch["stateToken"],
        "acceptCheckPatch":true,"acceptSyncStamp":true}),
    );
    assert!(full.get("checklistFormat").is_none() && full.get("checkPatch").is_none());
    assert_eq!(full["events"][0]["checklist"][0]["done"], true);
}

fn repeated_personal_profile(path: &Path, events: usize, tasks: usize, description: &str) {
    let today = Local::now().date_naive();
    let year = today.year();
    let phone = Phone::open(path);
    phone.ok(json!({"op":"createWorkspace","employee":"Ольга","name":"Отдел"}));
    let owner = phone.state(year)["deviceId"].clone();
    let mut command = event(&owner);
    command["date"] = json!(today);
    command["checklist"] = Value::Array(
        (0..tasks)
            // The two earliest tasks share a deadline. The index must preserve checklist order.
            .map(|i| json!({"title":format!("Пункт {i}"),"daysBefore":i.min(tasks.saturating_sub(2))}))
            .collect(),
    );
    command["description"] = json!(description);
    phone.ok(command);
    drop(phone);
    let file = path.join("calendar.json");
    let mut profile: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let plans = profile["workspace"]["calendar"]["plans"]
        .as_object_mut()
        .unwrap();
    let template = plans.values().next().unwrap().clone();
    for _ in 1..events {
        plans.insert(Id::new_v4().to_string(), template.clone());
    }
    fs::write(file, serde_json::to_vec(&profile).unwrap()).unwrap();
}

#[test]
fn text_budget_changes_the_view_for_the_same_number_of_personal_tasks() {
    let tmp = TempDir::new().unwrap();
    let year = Local::now().year();
    for (name, description, indexed) in [
        ("short", "".to_owned(), false),
        ("long", "Щ".repeat(4000), true),
    ] {
        let path = tmp.path().join(name);
        repeated_personal_profile(&path, 150, 1, &description);
        let phone = Phone::open(&path);
        let state = phone.ok(json!({"op":"state","year":year,"acceptAgendaIndex":true}));
        assert_eq!(state["agendaIndex"].as_bool().unwrap_or(false), indexed);
        assert_eq!(state["events"].as_array().unwrap().len(), 150);
        assert_eq!(state["tasks"].as_array().unwrap().len(), 150);
    }
}

#[test]
fn indexed_cards_and_reminders_reconstruct_the_snapshot_and_reject_stale_reads() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("large-profile");
    let year = Local::now().year();
    repeated_personal_profile(&path, 211, 40, "Описание");
    let phone = Phone::open(&path);
    let full = phone.state(year);
    let request = json!({"op":"state","year":year,"acceptAgendaIndex":true});
    let indexed = phone.ok(request.clone());
    let token = &indexed["stateToken"];
    assert_eq!(indexed["agendaIndex"], true);
    assert_eq!(indexed["notificationDates"], full["notificationDates"]);
    for group in ["events", "tasks"] {
        let rows = full[group].as_array().unwrap();
        let summaries = indexed[group].as_array().unwrap();
        assert_eq!(rows.len(), summaries.len());
        for (row, summary) in rows.iter().zip(summaries) {
            for field in ["id", "year", "date", "title", "ownerId", "employeeId"] {
                assert_eq!(summary[field], row[field]);
            }
            assert!(summary.get("checklist").is_none());
            if group == "tasks" {
                let next = row["checklist"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|t| t["done"] == false)
                    .min_by_key(|t| t["dueDate"].as_str().unwrap())
                    .unwrap();
                assert_eq!(summary["nextOpenTask"]["id"], next["id"]);
                assert_eq!(summary["nextOpenTask"]["dueDate"], next["dueDate"]);
            }
        }
    }
    let first = &full["events"][0];
    let card = |token: &Value| json!({"op":"agendaCard","year":year,"eventYear":year,"eventId":first["id"],"baseToken":token});
    let page = |token: &Value, offset: usize, limit: usize| json!({"op":"reminderPage","year":year,"baseToken":token,"offset":offset,"limit":limit});
    assert_eq!(phone.ok(card(token))["events"][0], *first);
    let reminders = full["reminders"].as_array().unwrap();
    assert!(reminders.len() > 64);
    assert_eq!(indexed["reminderCount"], reminders.len());
    for offset in [0, 63, 64, reminders.len() - 1, reminders.len(), usize::MAX] {
        let reply = phone.ok(page(token, offset, 64));
        let start = offset.min(reminders.len());
        assert_eq!(
            reply["reminders"],
            json!(&reminders[start..start.saturating_add(64).min(reminders.len())])
        );
    }
    phone.reject(page(token, 0, 65));
    phone.reject(page(token, 0, 0));
    let mut wrong = card(token);
    wrong["year"] = json!(year + 1);
    phone.reject(wrong);
    let mut invisible = card(token);
    invisible["eventYear"] = json!(year + 5);
    phone.reject(invisible);
    phone.reject(card(&full["stateToken"]));
    phone.ok(json!({"op":"check","eventId":first["id"],"year":year,"taskId":first["checklist"][0]["id"],"done":true,"baseToken":token}));
    phone.reject(card(token));
    phone.reject(page(token, 0, 1));
    let updated = phone.ok(request);
    let fresh = &updated["stateToken"];
    assert_ne!(fresh, token);
    assert_eq!(
        phone.ok(card(fresh))["events"][0]["checklist"][0]["done"],
        true
    );
    let mobile = unsafe { &*phone.0.cast::<std::sync::Mutex<super::Mobile>>() };
    let store = mobile
        .lock()
        .unwrap()
        .session
        .as_ref()
        .unwrap()
        .store
        .clone();
    store
        .lock()
        .unwrap()
        .transaction(|state| {
            let member = state.device;
            state.workspace_mut()?.calendar.members.remove(&member);
            Ok(())
        })
        .unwrap();
    phone.reject(json!({"op":"state","year":year}));
    phone.reject(json!({"op":"state","year":year,"acceptAgendaIndex":true}));
    phone.reject(card(fresh));
    phone.reject(page(fresh, 0, 64));
}

#[test]
fn indexed_check_receipt_matches_the_fresh_index_and_rejects_a_different_base() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("indexed-receipt");
    repeated_personal_profile(&path, 150, 1, &"Я".repeat(4000));
    let year = Local::now().year();
    let phone = Phone::open(&path);
    let request =
        json!({"op":"state","year":year,"acceptAgendaIndex":true,"acceptChecklistTuples":true});
    let mut previous = phone.ok(request.clone());
    let e = previous["tasks"][0].clone();
    let id = e["id"].clone();
    let task = e["nextOpenTask"]["id"].clone();
    let check = |view: &Value, done: bool| json!({"op":"check","year":year,"eventId":id,"taskId":task,"done":done,"baseToken":view["stateToken"]});
    let read = |view: &Value| json!({"op":"state","year":year,"acceptAgendaIndex":true,"acceptChecklistTuples":true,"ifToken":view["stateToken"],"acceptIndexCheckPatch":true});
    for done in [true, false] {
        phone.ok(check(&previous, done));
        let patch = phone.ok(read(&previous));
        assert_eq!(patch["checkPatch"], true);
        assert_eq!(patch["baseToken"], previous["stateToken"]);
        assert_eq!(patch["events"].as_array().unwrap().len(), 1);
        assert_eq!(patch["events"][0]["id"], id);
        assert_eq!(patch["events"][0]["checklist"][0]["done"], done);
        let fresh = phone.ok(request.clone());
        let selected: Vec<_> = fresh["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, row)| row["id"] == id)
            .collect();
        assert_eq!(
            patch["taskPositions"],
            json!(selected.iter().map(|(p, _)| p).collect::<Vec<_>>())
        );
        assert_eq!(
            patch["tasks"],
            json!(selected.iter().map(|(_, row)| row).collect::<Vec<_>>())
        );
        assert_eq!(patch["taskCount"], fresh["tasks"].as_array().unwrap().len());
        assert_eq!(fresh["events"], previous["events"]);
        for field in [
            "reminders",
            "reminderCount",
            "stateToken",
            "notificationDates",
            "members",
            "employees",
            "canManage",
            "workspaceName",
        ] {
            assert_eq!(fresh[field], patch[field]);
        }
        assert!(patch.get("reminderPositions").is_none());
        previous = fresh;
    }
    phone.ok(check(&previous, true));
    let mut legacy = read(&previous);
    legacy
        .as_object_mut()
        .unwrap()
        .remove("acceptIndexCheckPatch");
    legacy["acceptCheckPatch"] = json!(true);
    let fallback = phone.ok(legacy);
    assert!(fallback.get("checkPatch").is_none());
    phone.ok(check(&fallback, false));
    let other = &fallback["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] != id)
        .unwrap()["id"];
    phone.ok(
        json!({"op":"note","eventId":other,"year":year,"text":"Одновременная правка","base":""}),
    );
    let concurrent = phone.ok(read(&fallback));
    assert!(concurrent.get("checkPatch").is_none());
    phone.reject(check(&fallback, true));
    phone.ok(check(&concurrent, true));
    let mut other_year = read(&concurrent);
    other_year["year"] = json!(year + 1);
    assert!(phone.ok(other_year).get("checkPatch").is_none());
    drop(phone);
    let reopened = Phone::open(&path);
    let fresh = reopened.ok(request);
    assert!(fresh.get("checkPatch").is_none());
    let card=reopened.ok(json!({"op":"agendaCard","year":year,"eventYear":year,"eventId":id,"baseToken":fresh["stateToken"]}));
    assert_eq!(card["events"][0]["checklist"][0]["done"], true);
    reopened.ok(check(&fresh, false));
    let previous_card = &card["events"][0];
    reopened.ok(
        json!({"op":"editEvent","eventId":id,"year":year,"revision":previous_card["revision"],
        "scope":"future","title":previous_card["title"],"description":previous_card["description"],
        "date":previous_card["date"],"yearly":true,"category":previous_card["category"],
        "preparationDays":previous_card["preparationDays"],"ownerId":null,
        "checklist":[{"id":task,"title":previous_card["checklist"][0]["title"],"daysBefore":0}]}),
    );
    let unassigned = reopened.ok(read(&fresh));
    assert!(unassigned.get("checkPatch").is_none());
    assert!(
        unassigned["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != id)
    );
    assert!(
        unassigned["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == id && row["ownerId"].is_null())
    );
    reopened.reject(check(&fresh, true));
    let mobile = unsafe { &*reopened.0.cast::<std::sync::Mutex<super::Mobile>>() };
    let store = mobile
        .lock()
        .unwrap()
        .session
        .as_ref()
        .unwrap()
        .store
        .clone();
    store
        .lock()
        .unwrap()
        .transaction(|s| {
            let member = s.device;
            s.workspace_mut()?.calendar.members.remove(&member);
            Ok(())
        })
        .unwrap();
    reopened.reject(read(&unassigned));
    reopened.reject(check(&unassigned, true));
    reopened.reject(json!({"op":"agendaCard","year":year,"eventYear":year,"eventId":id,"baseToken":unassigned["stateToken"]}));
}

#[test]
fn checking_the_last_task_across_the_view_budget_returns_a_full_view_both_ways() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("boundary");
    repeated_personal_profile(&path, 150, 1, "");
    let year = Local::now().year();
    let file = path.join("calendar.json");
    let mut profile: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let request = json!({"op":"state","year":year,"acceptAgendaIndex":true});
    let mut set_description = |length: usize| {
        for plan in profile["workspace"]["calendar"]["plans"]
            .as_object_mut()
            .unwrap()
            .values_mut()
        {
            let template = plan["templates"]
                .as_object_mut()
                .unwrap()
                .values_mut()
                .next()
                .unwrap();
            template["value"]["description"] = json!("Я".repeat(length));
        }
        fs::write(&file, serde_json::to_vec(&profile).unwrap()).unwrap();
    };
    let (mut low, mut high) = (0, 4000);
    while low < high {
        let mid = (low + high) / 2;
        set_description(mid);
        let phone = Phone::open(&path);
        if phone.ok(request.clone())["agendaIndex"] == true {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    set_description(low);
    let phone = Phone::open(&path);
    let index = phone.ok(request);
    assert_eq!(index["agendaIndex"], true);
    let row = &index["tasks"][0];
    let check = |token: &Value, done| json!({"op":"check","year":year,"taskId":row["nextOpenTask"]["id"],"eventId":row["id"],"done":done,"baseToken":token});
    let read = |token: &Value| json!({"op":"state","year":year,"ifToken":token,"acceptAgendaIndex":true,"acceptCheckPatch":true,"acceptIndexCheckPatch":true});
    phone.ok(check(&index["stateToken"], true));
    let eager = phone.ok(read(&index["stateToken"]));
    assert!(eager.get("agendaIndex").is_none() && eager.get("checkPatch").is_none());
    assert_eq!(eager["events"].as_array().unwrap().len(), 150);
    phone.ok(check(&eager["stateToken"], false));
    let back = phone.ok(read(&eager["stateToken"]));
    assert_eq!(back["agendaIndex"], true);
    assert!(back.get("checkPatch").is_none());
    assert_eq!(back["events"], index["events"]);
    assert_eq!(back["tasks"], index["tasks"]);
}

#[test]
fn adjacent_check_requires_current_personal_occurrence_and_keeps_the_browsed_year() {
    let tmp = TempDir::new().unwrap();
    let phone = Phone::open(&tmp.path().join("profile"));
    phone.ok(json!({"op":"createWorkspace","employee":"Анна","name":"Офис"}));
    let today = Local::now().date_naive();
    let occurrence = today.year();
    let browsing = occurrence + 1;
    let me = phone.state(occurrence)["deviceId"].clone();
    for owner in [me, Value::Null] {
        let mut create = event(&owner);
        create["date"] = json!(today);
        phone.ok(create);
    }
    let here = phone.state(occurrence);
    let own = &here["tasks"][0];
    let other = here["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] != own["id"])
        .unwrap();
    let origin = phone.state(browsing);
    let check = |row: &Value| {
        json!({"op":"check", "viewYear":browsing, "year":occurrence,
        "baseToken":origin["stateToken"],"eventId":row["id"],"taskId":row["checklist"][0]["id"],"done":true})
    };
    phone.reject(check(other));
    let mut missing = check(own);
    missing.as_object_mut().unwrap().remove("baseToken");
    phone.reject(missing);
    let mut wrong_origin = check(own);
    wrong_origin["viewYear"] = json!(browsing + 1);
    phone.reject(wrong_origin);
    let mut outside = check(own);
    outside["year"] = json!(occurrence + 4);
    phone.reject(outside);
    phone.ok(check(own));
    phone.reject(check(own));
    let next = phone.ok(json!({"op":"state","year":browsing,"ifToken":origin["stateToken"],"acceptCheckPatch":true}));
    assert!(next.get("checkPatch").is_none());
    assert_eq!(next["year"], browsing);
    assert!(
        next["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["checklist"][0]["done"] == false)
    );
    let updated = phone.state(occurrence);
    for e in updated["events"].as_array().unwrap() {
        assert_eq!(e["checklist"][0]["done"], e["id"] == own["id"]);
    }
}

#[test]
fn adjacent_journal_and_editor_use_the_same_assignment_and_snapshot_as_the_card() {
    let tmp = TempDir::new().unwrap();
    let phone = Phone::open(&tmp.path().join("profile"));
    phone.ok(json!({"op":"createWorkspace","employee":"Анна","name":"Офис"}));
    let today = Local::now().date_naive();
    let year = today.year();
    let browsing = year + 1;
    let me = phone.state(year)["deviceId"].clone();
    let mut create = event(&me);
    create["date"] = json!(today);
    phone.ok(create.clone());
    let row = phone.state(year)["tasks"][0].clone();
    let target = &row["id"];
    let mut edit = create;
    edit["op"] = json!("editEvent");
    edit["eventId"] = target.clone();
    edit["year"] = json!(year);
    edit["scope"] = json!("year");
    edit["revision"] = row["revision"].clone();
    edit["title"] = json!("Уточнённый срок");
    let mut note =
        json!({"op":"note","eventId":target,"year":year,"text":"Подготовлено","base":""});
    let mut delete = json!({"op":"deleteEvent","eventId":target,"year":year,"scope":"year","revision":row["revision"]});
    let origin = phone.state(browsing);
    for cmd in [&mut edit, &mut note, &mut delete] {
        cmd["viewYear"] = json!(browsing);
        phone.reject(cmd.clone());
        cmd["baseToken"] = origin["stateToken"].clone();
    }
    phone.ok(note.clone());
    phone.reject(edit.clone());
    phone.reject(delete.clone());
    let current = phone.state(browsing);
    edit["baseToken"] = current["stateToken"].clone();
    edit["ownerId"] = Value::Null;
    phone.ok(edit.clone());
    let reassigned = phone.state(year)["events"][0].clone();
    assert_eq!(reassigned["note"], "Подготовлено");
    assert_eq!(reassigned["title"], "Уточнённый срок");
    assert!(reassigned["ownerId"].is_null());
    let origin = phone.state(browsing);
    assert!(
        origin["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["id"] != *target || row["year"] != year)
    );
    edit["revision"] = reassigned["revision"].clone();
    delete["revision"] = reassigned["revision"].clone();
    note["base"] = json!("Подготовлено");
    note["text"] = json!("Снова");
    for cmd in [&mut edit, &mut delete, &mut note] {
        cmd["baseToken"] = origin["stateToken"].clone();
        phone.reject(cmd.clone());
    }
    assert_eq!(
        phone.ok(json!({"op":"state","year":browsing,"ifToken":origin["stateToken"]})),
        json!({"unchanged":true})
    );
    assert_eq!(phone.state(year)["events"][0], reassigned);
}

/// Emits one shared input for the Android and Swift production decoders. The
/// expected result is an unconditional read of the same live Rust profile.
#[test]
#[ignore = "writes a fresh cross-platform contract fixture to GP_CHECKLIST_FIXTURE"]
fn emit_native_checklist_fixture() {
    let output = std::env::var_os("GP_CHECKLIST_FIXTURE").expect("set GP_CHECKLIST_FIXTURE");
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("phone");
    repeated_personal_profile(&path, 600, 1, "");
    let phone = Phone::open(&path);
    let year = Local::now().year();
    let mut cases = Vec::new();
    for tuples in [false, true] {
        let done = !tuples;
        let mut request = json!({"op":"state","year":year});
        if tuples {
            request["acceptChecklistTuples"] = json!(true);
        }
        let before = phone.ok(request.clone());
        let selected = &before["events"][300];
        let task_id = if tuples {
            selected["checklist"][0][0].clone()
        } else {
            selected["checklist"][0]["id"].clone()
        };
        let command = json!({"op":"check","year":year,"eventId":selected["id"],
            "taskId":task_id,"done":done,"baseToken":before["stateToken"]});
        phone.ok(command.clone());
        let mut conditional = json!({"op":"state","year":year,
            "ifToken":before["stateToken"],"acceptCheckPatch":true});
        if tuples {
            conditional["acceptChecklistTuples"] = json!(true);
        }
        let patch = phone.ok(conditional);
        assert_eq!(patch["checkPatch"], true);
        let mut full_request = json!({"op":"state","year":year});
        if tuples {
            full_request["acceptChecklistTuples"] = json!(true);
        }
        let expected = phone.ok(full_request);
        assert_eq!(expected["stateToken"], patch["stateToken"]);

        let mut invalid = Vec::new();
        let mut wrong_token = patch.clone();
        wrong_token["baseToken"] = json!(Id::new_v4());
        invalid.push(json!({"name":"wrong base token","patch":wrong_token}));
        let mut wrong_year = patch.clone();
        wrong_year["year"] = json!(year + 1);
        invalid.push(json!({"name":"wrong year","patch":wrong_year}));
        let mut wrong_event = patch.clone();
        wrong_event["events"][0]["id"] = json!(Id::new_v4());
        invalid.push(json!({"name":"wrong event identity","patch":wrong_event}));
        let mut wrong_task = patch.clone();
        if wrong_task["events"][0]["checklist"][0].is_array() {
            wrong_task["events"][0]["checklist"][0][0] = json!(Id::new_v4());
        } else {
            wrong_task["events"][0]["checklist"][0]["id"] = json!(Id::new_v4());
        }
        invalid.push(json!({"name":"wrong task identity","patch":wrong_task}));
        let mut wrong_position = patch.clone();
        wrong_position["taskPositions"] = json!([-1]);
        invalid.push(json!({"name":"invalid position","patch":wrong_position}));
        let mut duplicate_rows = patch.clone();
        let duplicate = duplicate_rows["events"][0].clone();
        duplicate_rows["events"].as_array_mut().unwrap().push(duplicate);
        invalid.push(json!({"name":"duplicate event rows","patch":duplicate_rows}));
        let mut duplicate_positions = patch.clone();
        let first_position = duplicate_positions["taskPositions"]
            .as_array()
            .and_then(|positions| positions.first())
            .cloned()
            .unwrap_or_else(|| json!(0));
        duplicate_positions["taskPositions"] = json!([first_position.clone(), first_position]);
        invalid.push(json!({"name":"duplicate positions","patch":duplicate_positions}));
        cases.push(json!({"before":before,"command":command,"patch":patch,
            "expected":expected,"invalidPatches":invalid}));
    }
    let prior = cases[0]["before"].clone();
    let sync_stamp = json!({"prior":prior.clone(),"update":{"syncStamp":true,"year":year,
        "baseToken":prior["stateToken"],"stateToken":Id::new_v4(),
        "lastSync":"1970-01-01T00:00:01+00:00"}});

    let indexed_path = temp.path().join("indexed-phone");
    repeated_personal_profile(&indexed_path, 175, 40, "");
    let indexed_phone = Phone::open(&indexed_path);
    let full_before = indexed_phone.ok(json!({"op":"state","year":year}));
    let indexed_before = indexed_phone.ok(json!({"op":"state","year":year,
        "acceptAgendaIndex":true,"acceptChecklistTuples":true}));
    assert_eq!(indexed_before["agendaIndex"], true);
    let indexed_id = indexed_before["events"][87]["id"].clone();
    let indexed_card = indexed_phone.ok(json!({"op":"agendaCard","year":year,"eventYear":year,
        "eventId":indexed_id,"baseToken":indexed_before["stateToken"]}));
    let full_card = full_before["events"].as_array().unwrap().iter()
        .find(|event| event["id"] == indexed_id).unwrap().clone();
    assert_eq!(indexed_card["events"][0], full_card);
    let indexed_page = indexed_phone.ok(json!({"op":"reminderPage","year":year,
        "baseToken":indexed_before["stateToken"],"offset":0,"limit":64}));
    let full_page = full_before["reminders"].as_array().unwrap()
        .iter().take(indexed_page["reminders"].as_array().unwrap().len()).cloned().collect::<Vec<_>>();
    assert_eq!(indexed_page["reminders"], json!(full_page));
    let indexed_task = indexed_card["events"][0]["checklist"][38]["id"].clone();
    let indexed_command = json!({"op":"check","year":year,"viewYear":year,"eventId":indexed_id,
        "taskId":indexed_task,"done":true,"baseToken":indexed_before["stateToken"]});
    indexed_phone.ok(indexed_command.clone());
    let indexed_patch = indexed_phone.ok(json!({"op":"state","year":year,
        "ifToken":indexed_before["stateToken"],"acceptAgendaIndex":true,
        "acceptChecklistTuples":true,"acceptIndexCheckPatch":true}));
    assert_eq!(indexed_patch["checkPatch"], true);
    let indexed_expected = indexed_phone.ok(json!({"op":"state","year":year,
        "acceptAgendaIndex":true,"acceptChecklistTuples":true}));
    assert_eq!(indexed_expected["stateToken"], indexed_patch["stateToken"]);
    let indexed_fixture = json!({
        "before":indexed_before,
        "command":{"eventId":indexed_id,"taskId":indexed_task,"done":true},
        "card":indexed_card,
        "fullCard":full_card,
        "page":indexed_page,
        "fullPage":full_page,
        "patch":indexed_patch,
        "expected":indexed_expected
    });

    let limits = json!({
        "maxTasks":godovoy_plan::model::MAX_TASKS,
        "maxTitleChars":godovoy_plan::model::MAX_TITLE_CHARS,
        "maxDescriptionChars":godovoy_plan::model::MAX_DESCRIPTION_CHARS,
        "maxNoteChars":godovoy_plan::model::MAX_NOTE_CHARS,
        "maxPreparationDays":godovoy_plan::model::MAX_PREPARATION_DAYS,
        "maxExchangeBytes":crate::staging::MAX_EXCHANGE_BYTES
    });
    let output = fs::File::create_new(output).unwrap();
    serde_json::to_writer(output, &json!({"limits":limits,"cases":cases,"syncStamp":sync_stamp,"indexed":indexed_fixture})).unwrap();
}
