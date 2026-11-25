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
        phone.ok(json!({"op":"state","year":2027,"ifToken":guarded["stateToken"]}))["workspaceName"],
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
    let changed = phone.ok(json!({"op":"state","year":2027,"ifToken":before_check["stateToken"]}));
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
    let address = state["address"].as_str().unwrap().parse().unwrap();
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
    phone.ok(json!({"op":"linkEmployeeDevice","deviceId":own,"employeeId":own}));
    assert_eq!(phone.state(year)["tasks"], json!([]));
    phone.ok(link);

    let stale = phone.ok(json!({"op":"export"}));
    phone.ok(json!({"op":"revoke","deviceIds":[id]}));
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
