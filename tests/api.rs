#[path = "support/secure.rs"]
mod secure;
use godovoy_plan::api::{ApiKeyKind, ApiMethod, ApiSession};
use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, TcpListener},
    path::Path,
    process::Command,
    sync::Arc,
    thread,
    time::Duration,
};

use godovoy_plan::{
    api::{
        ApiServer, BridgeOutcome, BridgeRun, ImportBatch, encode_token, import_secure,
        report_bridge_run,
    },
    model::Id,
    storage::{ApiSettings, ImportResultKind, SharedStore, Store},
};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    server: ApiServer,
    store: SharedStore,
    profile: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self::on_ports(0..=0)
    }

    fn on_ports(ports: std::ops::RangeInclusive<u16>) -> Self {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(&profile.path().join("profile")).unwrap();
        store
            .lock()
            .unwrap()
            .transaction(|state| {
                state.create_workspace("Команда".into(), "Анна".into())?;
                state.api = Some(ApiSettings {
                    token: [11; 32],
                    import_token: Some([22; 32]),
                    port: 0,
                    enabled: true,
                });
                Ok(())
            })
            .unwrap();
        let server = ports
            .into_iter()
            .find_map(|port| match ApiServer::start(store.clone(), port) {
                Ok(server) => Some(server),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::AddrInUse) =>
                {
                    None
                }
                Err(error) => panic!("Starting test API failed: {error}"),
            })
            .expect("No available test API port");
        Self {
            server,
            store,
            profile,
        }
    }
}

async fn start_pending_import(fixture: &Fixture) -> (ApiSession, Vec<u8>) {
    let body = serde_json::to_vec(&json!({"events":[{
        "externalId":"qa-pending", "event":{"title":"Тест импорта","date":"2026-12-20"}
    }]}))
    .unwrap();
    let session = ApiSession::connect(fixture.server.port(), &[22; 32], ApiKeyKind::Import)
        .await
        .unwrap();
    (session, body)
}

#[tokio::test]
async fn pending_import_releases_old_calendar_and_checks_changed_credentials() {
    for (enabled, expected) in [(true, "401"), (false, "503")] {
        let fixture = Fixture::new();
        let old = Arc::downgrade(&fixture.store.lock().unwrap().snapshot());
        let (session, body) = start_pending_import(&fixture).await;
        fixture
            .store
            .lock()
            .unwrap()
            .transaction(|state| {
                let settings = state.api.as_mut().unwrap();
                settings.enabled = enabled;
                if enabled {
                    settings.import_token = Some([33; 32]);
                }
                Ok(())
            })
            .unwrap();
        assert!(
            old.upgrade().is_none(),
            "pending request keeps an old calendar in memory"
        );
        let reply = session
            .request(ApiMethod::Put, "/v1/imports/crm", &body)
            .await
            .unwrap();
        assert_eq!(reply.status.to_string(), expected);
        let current = fixture.store.lock().unwrap().snapshot();
        assert!(
            current
                .workspace
                .as_ref()
                .unwrap()
                .calendar
                .plans
                .is_empty()
        );
        let persisted =
            std::fs::read(fixture.profile.path().join("profile/calendar.json")).unwrap();
        assert_eq!(
            serde_json::from_slice::<godovoy_plan::storage::LocalState>(&persisted).unwrap(),
            *current
        );
        assert!(
            fixture
                .store
                .lock()
                .unwrap()
                .reader()
                .import_status()
                .is_empty()
        );
        assert!(
            !fixture
                .profile
                .path()
                .join("profile/imports-v1.json")
                .exists()
        );
    }
}

#[tokio::test]
async fn shutting_down_closes_an_incomplete_import_and_releases_the_port() {
    // A port picked by bind(0) can be reused by another parallel test client immediately after close.
    let fixture = Fixture::on_ports(30411..=30430);
    let port = fixture.server.port();
    let (session, body) = start_pending_import(&fixture).await;
    fixture
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.api.as_mut().unwrap().enabled = false;
            Ok(())
        })
        .unwrap();
    let Fixture {
        server,
        store,
        profile: _profile,
    } = fixture;
    drop(server);
    assert!(
        session
            .request(ApiMethod::Put, "/v1/imports/crm", &body)
            .await
            .is_err()
    );
    assert!(
        store
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .plans
            .is_empty()
    );
    assert_eq!(ApiServer::start(store, port).unwrap().port(), port);
}

fn import(fixture: &Fixture, name: &str) -> Value {
    secure::put(
        fixture.server.port(),
        22,
        "/v1/imports/crm",
        &json!({"events":[{
            "externalId":"cr-123","event":{"title":name,"date":"2026-12-20","yearly":true,
            "checklist":[{"key":"approve","title":"Согласовать","daysBefore":14}]}
        }]}),
    )
    .unwrap()
}

#[tokio::test]
async fn import_history_is_local_bounded_and_excludes_request_contents() {
    let fixture = Fixture::new();

    let profile = fixture.profile.path().join("profile");
    let file = profile.join("imports-v1.json");
    let reader = fixture.store.lock().unwrap().reader();

    import(&fixture, "private-calendar-title");
    import(&fixture, "private-calendar-title");
    let rejected = secure::put(
        fixture.server.port(),
        22,
        "/v1/imports/crm",
        &json!({"events":[{"externalId":"","event":{
            "title":"private-bad-title","date":"2026-11-10"
        }}]}),
    );
    assert!(matches!(rejected, Err(secure::Error::Status(422))));
    let status = reader.import_status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].source, "crm");
    assert_eq!(status[0].success.unwrap().received, 1);
    assert_eq!(status[0].success.unwrap().changed, 0);
    assert_eq!(
        status[0].failure.unwrap().kind,
        ImportResultKind::InvalidBatch
    );
    let before = std::fs::read(&file).unwrap();
    let denied = secure::put(
        fixture.server.port(),
        33,
        "/v1/imports/denied",
        &json!({"events":[]}),
    );
    assert!(matches!(denied, Err(secure::Error::Transport)));
    assert_eq!(std::fs::read(&file).unwrap(), before);

    let batch = serde_json::from_value(json!({"events":[{"externalId":"private-external-id",
        "event":{"title":"private-secure-title","date":"2026-11-12"}}]}))
    .unwrap();
    assert_eq!(
        import_secure(fixture.server.port(), &[22; 32], "secure", &batch)
            .await
            .unwrap(),
        1
    );
    let status = reader.import_status();
    assert_eq!(status[0].source, "secure");
    assert_eq!(status[0].success.unwrap().changed, 1);
    let contents = std::fs::read_to_string(&file).unwrap();
    for secret in [
        "private-calendar-title",
        "private-bad-title",
        "private-external-id",
        "private-secure-title",
        "cr-123",
        &encode_token(&[22; 32]),
        &encode_token(&[11; 32]),
    ] {
        assert!(!contents.contains(secret));
    }
    let snapshot = fixture.store.lock().unwrap().snapshot();
    let calendar = serde_json::to_string(&snapshot.workspace.as_ref().unwrap().calendar).unwrap();
    assert!(!calendar.contains("InvalidBatch"));
    fixture.store.lock().unwrap().create_backup().unwrap();
    let folder = Store::backup_folder(&profile);
    let backup_text = std::fs::read_dir(folder)
        .unwrap()
        .map(|row| row.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("calendar-")
        })
        .map(|path| std::fs::read_to_string(path).unwrap())
        .unwrap();
    assert!(!backup_text.contains("InvalidBatch"));
    assert!(!backup_text.contains("\"sources\""));

    for index in 0..34 {
        let result = secure::put(
            fixture.server.port(),
            22,
            &format!("/v1/imports/source-{index}"),
            &json!({"events":[]}),
        )
        .unwrap();
        assert_eq!(result, json!([]));
    }
    let expected = reader.import_status();
    assert_eq!(expected.len(), 32);
    assert_eq!(expected[0].source, "source-33");
    assert!(!reader.import_status_warning());
    let Fixture {
        server,
        store,
        profile: _keep_directory,
    } = fixture;
    drop(server);
    drop(store);
    let reopened = Store::open_existing(&profile).unwrap();
    assert_eq!(reopened.lock().unwrap().reader().import_status(), expected);
}

#[test]
fn import_history_write_failure_does_not_change_api_result_and_retries() {
    let fixture = Fixture::new();

    let path = fixture.profile.path().join("profile/imports-v1.json");
    let reader = fixture.store.lock().unwrap().reader();
    assert_eq!(import(&fixture, "First")[0]["changed"], true);
    let old = path.with_extension("previous");
    std::fs::rename(&path, &old).unwrap();
    let previous = std::fs::read(&old).unwrap();
    std::fs::create_dir(&path).unwrap();

    assert_eq!(import(&fixture, "Second")[0]["changed"], true);
    assert!(reader.import_status_warning());
    assert_eq!(reader.import_status()[0].success.unwrap().changed, 1);
    assert_eq!(std::fs::read(&old).unwrap(), previous);
    assert!(path.is_dir());
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(old, &path).unwrap();
    assert_eq!(import(&fixture, "Second")[0]["changed"], false);
    assert!(!reader.import_status_warning());
    assert_eq!(reader.import_status()[0].success.unwrap().changed, 0);
}

#[tokio::test]
async fn bridge_report_survives_reopen_without_changing_calendar_or_batch_history() {
    let fixture = Fixture::new();

    let profile = fixture.profile.path().join("profile");
    import(&fixture, "Existing event");
    let local_batch = std::fs::read(profile.join("imports-v1.json")).unwrap();
    let calendar = std::fs::read(profile.join("calendar.json")).unwrap();
    let reader = fixture.store.lock().unwrap().reader();
    let port = fixture.server.port();
    let empty = BridgeRun {
        fetched: 0,
        confirmed: 0,
        changed: 0,
        outcome: BridgeOutcome::Complete,
    };
    assert!(
        report_bridge_run(port, &[19; 32], "unauthorized", empty)
            .await
            .is_err()
    );
    assert!(
        report_bridge_run(
            port,
            &[22; 32],
            "bad-count",
            BridgeRun {
                changed: 1,
                ..empty
            }
        )
        .await
        .is_err()
    );
    assert!(!profile.join("bridge-runs-v1.json").exists());
    report_bridge_run(port, &[22; 32], "crm", empty)
        .await
        .unwrap();
    let lost = BridgeRun {
        fetched: 130,
        confirmed: 100,
        changed: 92,
        outcome: BridgeOutcome::ConfirmationLost,
    };
    report_bridge_run(port, &[22; 32], "partial", lost)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(profile.join("imports-v1.json")).unwrap(),
        local_batch
    );
    assert_eq!(
        std::fs::read(profile.join("calendar.json")).unwrap(),
        calendar
    );
    let statuses = reader.import_status();
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0].source, "partial");
    assert_eq!(statuses[0].bridge_run.unwrap().run, lost);
    assert!(statuses[1].success.is_some());
    assert_eq!(statuses[1].bridge_run.unwrap().run, empty);

    let Fixture {
        server,
        store,
        profile: _keep_directory,
    } = fixture;
    drop(server);
    drop(store);
    let reopened = Store::open_existing(&profile).unwrap();
    assert_eq!(reopened.lock().unwrap().reader().import_status(), statuses);
}

#[test]
fn legacy_plain_http_cannot_read_the_api() {
    let fixture = Fixture::new();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into();
    let address = format!("http://127.0.0.1:{}/v1/health", fixture.server.port());
    assert!(agent.get(address).call().is_err());
}

#[test]
fn authorized_read_create_update_and_rotate() {
    let fixture = Fixture::new();

    let port = fixture.server.port();
    let path = "/v1/events?year=2026";
    assert!(matches!(
        secure::get(port, 99, path),
        Err(secure::Error::Transport)
    ));
    assert!(matches!(
        secure::request(port, ApiKeyKind::Import, 22, ApiMethod::Get, path, None),
        Err(secure::Error::Status(401))
    ));
    let created = secure::request(
        port,
        ApiKeyKind::Main,
        11,
        ApiMethod::Post,
        "/v1/events",
        Some(&json!({"title":"Общее собрание","date":"2026-11-05"})),
    )
    .unwrap();
    assert_eq!(created.status, 201);
    assert_eq!(
        serde_json::from_slice::<Value>(&created.body).unwrap()["title"],
        "Общее собрание"
    );
    let a = import(&fixture, "Годовая акция");
    let b = import(&fixture, "Годовая акция");
    let c = import(&fixture, "Новая акция");
    assert_eq!(a[0]["changed"], true);
    assert_eq!(b[0]["changed"], false);
    assert_eq!(c[0]["changed"], true);
    assert_eq!(a[0]["id"], c[0]["id"]);
    let event = secure::get(
        port,
        11,
        &format!("/v1/events/{}?year=2027", c[0]["id"].as_str().unwrap()),
    )
    .unwrap();
    assert_eq!(event["title"], "Новая акция");
    assert_eq!(event["date"], "2027-12-20");
    assert_eq!(event["checklist"][0]["dueDate"], "2027-12-06");
    assert_eq!(
        secure::get(port, 11, "/v1/events?year=2026&limit=1&offset=1")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            state.api.as_mut().unwrap().import_token = Some([33; 32]);
            state.api.as_mut().unwrap().token = [44; 32];
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        secure::get(port, 11, path),
        Err(secure::Error::Transport)
    ));
    assert!(secure::get(port, 44, path).is_ok());
}

#[test]
fn bridge_pulls_json_in_separate_process_without_profile_access() {
    let fixture = Fixture::new();
    let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = upstream.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut request = BufReader::new(&mut stream);
        let mut line = String::new();
        request.read_line(&mut line).unwrap();
        assert!(line.starts_with("GET /events "));
        let mut has_source_key = false;
        loop {
            line.clear();
            request.read_line(&mut line).unwrap();
            if line.is_empty() || line == "\r\n" {
                break;
            }
            if line.to_ascii_lowercase().starts_with("authorization:") {
                assert!(line.trim_end().ends_with("Bearer fixture-source"));
                has_source_key = true;
            }
        }
        assert!(has_source_key);
        let body = json!({"events":[{"externalId":"src-007",
            "event":{"title":"Встреча с партнёрами","date":"2026-12-10"}}]})
        .to_string();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
    });
    let result = Command::new(env!("CARGO_BIN_EXE_godovoy-bridge"))
        .args([
            "--source",
            "fixture",
            "--api-port",
            &fixture.server.port().to_string(),
        ])
        .env("GODOVOY_API_TOKEN", encode_token(&[22; 32]))
        .env("GODOVOY_SOURCE_URL", format!("http://{address}/events"))
        .env("GODOVOY_SOURCE_TOKEN", "fixture-source")
        .current_dir(tempfile::tempdir().unwrap().path())
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        result.status.success(),
        "{:?}",
        String::from_utf8_lossy(&result.stderr)
    );
    let state = fixture.store.lock().unwrap().snapshot();
    let event = state.workspace.as_ref().unwrap().calendar.year(2026)[0];
    assert_eq!(event.event.title, "Встреча с партнёрами");
}

#[tokio::test]
async fn secure_import_spans_frames_and_keeps_the_external_identity() {
    let fixture = Fixture::new();
    let events: Vec<_> = (0..20)
        .map(|index| {
            json!({
                "externalId": format!("test-{index}"),
                "event": {
                    "title": format!("Событие {index}"),
                    "date": "2026-12-10",
                    "description": "d".repeat(4000),
                },
            })
        })
        .collect();
    let batch: ImportBatch = serde_json::from_value(json!({"events": events})).unwrap();
    let port = fixture.server.port();
    assert_eq!(
        import_secure(port, &[22; 32], "bridge", &batch)
            .await
            .unwrap(),
        20
    );
    let snapshot = fixture.store.lock().unwrap().snapshot();
    assert_eq!(
        import_secure(port, &[22; 32], "bridge", &batch)
            .await
            .unwrap(),
        0
    );
    let current = fixture.store.lock().unwrap().snapshot();
    assert!(std::sync::Arc::ptr_eq(&snapshot, &current));
    assert_eq!(current.workspace.as_ref().unwrap().calendar.plans.len(), 20);
}

#[test]
fn repeated_requests_release_workers_and_listener() {
    // A port picked by bind(0) can be reused by another parallel test client immediately after close.
    let fixture = Fixture::on_ports(30431..=30450);
    let port = fixture.server.port();

    for n in 0..400 {
        let imported = import(&fixture, &format!("Событие {n}"));
        assert_eq!(imported[0]["changed"], true);
        let _ = secure::get(fixture.server.port(), 11, "/v1/events?year=2026").unwrap();
    }
    let state = fixture.store.lock().unwrap().snapshot();
    assert_eq!(state.workspace.as_ref().unwrap().calendar.plans.len(), 1);
    drop(fixture);
    assert!(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok());
}

#[test]
fn rolling_back_invalid_data_keeps_previous_profile() {
    let fixture = Fixture::new();

    let batch = json!({"events":[
        {"externalId":"ok", "event":{"title":"Valid","date":"2026-09-01"}},
        {"externalId":"invalid","event":{"title":"","date":"2026-09-02"}}
    ]});
    assert!(matches!(
        secure::put(fixture.server.port(), 22, "/v1/imports/crm", &batch),
        Err(secure::Error::Status(422))
    ));
    assert!(
        fixture
            .store
            .lock()
            .unwrap()
            .state()
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .plans
            .is_empty()
    );
}

#[test]
fn child_profile_probe() {
    if let Some(directory) = std::env::var_os("GODOVOY_TEST_PROFILE") {
        match Store::open(Path::new(&directory)) {
            Ok(store) => println!("profile-device:{}", store.lock().unwrap().state().device),
            Err(_) => std::process::exit(51),
        }
    }
}

#[test]
fn separate_processes_cannot_share_profile_and_have_distinct_ids() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let path_a = a.path().join("profile");
    let path_b = b.path().join("profile");
    let store = Store::open(&path_a).unwrap();
    let device = store.lock().unwrap().state().device;
    let probe = |path: &Path| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_profile_probe", "--nocapture"])
            .env("GODOVOY_TEST_PROFILE", path)
            .output()
            .unwrap()
    };
    assert_eq!(probe(&path_a).status.code(), Some(51));
    let separate = probe(&path_b);
    assert!(separate.status.success());
    let output = String::from_utf8_lossy(&separate.stdout);
    let line = output
        .lines()
        .find_map(|line| line.strip_prefix("profile-device:"))
        .unwrap();
    assert_ne!(Id::parse_str(line).unwrap(), device);
}

#[test]
fn capacity_and_reads_are_available_during_a_profile_write() {
    let fixture = Fixture::new();

    let before = fixture.store.lock().unwrap().reader().snapshot().usage;
    {
        let _writer = fixture.store.lock().unwrap();
        let usage: Value = secure::get(fixture.server.port(), 11, "/v1/capacity").unwrap();
        assert_eq!(usage["plans"], 0);
        assert_eq!(usage["bytes"], before.bytes);
        assert_eq!(usage["planLimit"], before.plan_limit);
        assert_eq!(usage["byteLimit"], before.byte_limit);
        let members: Value = secure::get(fixture.server.port(), 11, "/v1/members").unwrap();
        assert_eq!(members[0]["name"], "Анна");
    }
    assert!(matches!(
        secure::request(
            fixture.server.port(),
            ApiKeyKind::Import,
            22,
            ApiMethod::Get,
            "/v1/capacity",
            None
        ),
        Err(secure::Error::Status(401))
    ));
}

#[test]
fn full_calendar_keeps_existing_updates_and_rejects_mixed_batch_atomically() {
    use godovoy_plan::model::{MAX_PLANS, Record};

    let fixture = Fixture::new();

    let first = import(&fixture, "Годовая акция");
    let id: Id = first[0]["id"].as_str().unwrap().parse().unwrap();
    fixture
        .store
        .lock()
        .unwrap()
        .transaction(|state| {
            let revision = state.revision()?;
            let calendar = &mut state.workspace_mut()?.calendar;
            for value in 1..MAX_PLANS {
                // A removed plan still occupies one entry so offline peers cannot resurrect it.
                calendar
                    .plans
                    .entry(Id::from_u128(value as u128))
                    .or_default()
                    .templates
                    .insert(2026, Record::new(revision, None));
            }
            Ok(())
        })
        .unwrap();
    let snapshot = fixture.store.lock().unwrap().reader().snapshot();
    assert_eq!(snapshot.usage.plans, MAX_PLANS);
    assert!(snapshot.usage.near_limit());

    let mixed = secure::put(
        fixture.server.port(),
        22,
        "/v1/imports/crm",
        &json!({"events":[
            {"externalId":"cr-123","event":{"title":"Переименовано","date":"2026-12-20"}},
            {"externalId":"new","event":{"title":"Новое событие","date":"2026-12-21"}}
        ]}),
    );
    assert!(matches!(mixed, Err(secure::Error::Status(422))));
    let after = fixture.store.lock().unwrap().reader().snapshot();
    assert!(std::sync::Arc::ptr_eq(&snapshot.state, &after.state));
    assert_eq!(
        after
            .state
            .workspace
            .as_ref()
            .unwrap()
            .calendar
            .event(id, 2026)
            .unwrap()
            .event
            .title,
        "Годовая акция"
    );

    let updated = import(&fixture, "Переименовано");
    assert_eq!(updated[0]["id"], first[0]["id"]);
    assert_eq!(updated[0]["changed"], true);
    let committed = fixture.store.lock().unwrap().reader().snapshot();
    assert_eq!(committed.usage.plans, MAX_PLANS);
    assert_eq!(
        committed.usage.bytes as u64,
        std::fs::metadata(fixture.profile.path().join("profile/calendar.json"))
            .unwrap()
            .len()
    );
}

#[test]
fn cli_validates_the_profile_before_submitting_event_json() {
    use std::process::Stdio;
    let fixture = Fixture::new();
    let submit = |token: u8| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_godovoy-bridge"))
            .args([
                "--request",
                "POST",
                "/v1/events",
                "--api-port",
                &fixture.server.port().to_string(),
            ])
            .env("GODOVOY_API_TOKEN", encode_token(&[token; 32]))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"title":"Annual review","date":"2027-02-01"}"#)
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let other_profile = submit(99);
    assert!(!other_profile.status.success());
    assert!(other_profile.stdout.is_empty());
    assert!(
        secure::get(fixture.server.port(), 11, "/v1/events?year=2027")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    let expected_profile = submit(11);
    assert!(expected_profile.status.success());
    let event: Value = serde_json::from_slice(&expected_profile.stdout).unwrap();
    assert_eq!(event["title"], "Annual review");
}

#[test]
fn revoked_owner_is_historical_and_cannot_receive_new_assignments() {
    use godovoy_plan::{
        model::Record,
        network::{Config, Network},
    };
    use std::net::SocketAddrV4;
    let fixture = Fixture::new();
    let former = Id::new_v4();
    let active = fixture.store.lock().unwrap().state().device;
    fixture
        .store
        .lock()
        .unwrap()
        .transaction(|s| {
            let rev = s.revision()?;
            s.workspace_mut()?
                .calendar
                .members
                .insert(former, Record::new(rev, "Борис".to_owned()));
            Ok(())
        })
        .unwrap();
    let batch = |key: &str, event: Value| json!({"events":[{"externalId":key,"event":event}]});
    let owned = json!({"date":"2027-11-26","title":"Поздравления","ownerId":former});
    let original = secure::put(
        fixture.server.port(),
        22,
        "/v1/imports/crm",
        &batch("partners", owned.clone()),
    )
    .unwrap();
    let network = Network::start(
        fixture.store.clone(),
        Config {
            listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            discovery_port: None,
        },
    )
    .unwrap();
    network
        .revoke_devices_async([former].into_iter().collect())
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let directory = fixture.store.lock().unwrap().successor_directory().unwrap();
    let next = Store::open_existing(&directory).unwrap();
    next.lock()
        .unwrap()
        .transaction(|s| {
            s.api = Some(ApiSettings {
                enabled: true,
                port: 0,
                token: [33; 32],
                import_token: Some([44; 32]),
            });
            Ok(())
        })
        .unwrap();
    let api = ApiServer::start(next, 0).unwrap();
    let port = api.port();
    let members = secure::get(port, 33, "/v1/members").unwrap();
    let rows = members.as_array().unwrap();
    assert!(rows.contains(&json!({"id":former,"name":"Борис","revoked":true,"employeeId":former})));
    assert!(rows.contains(&json!({"id":active,"name":"Анна","revoked":false,"employeeId":active})));
    let bad = secure::request(
        port,
        ApiKeyKind::Main,
        33,
        ApiMethod::Post,
        "/v1/events",
        Some(&owned),
    );
    assert!(matches!(bad, Err(secure::Error::Status(422))));
    assert!(matches!(
        secure::put(
            port,
            44,
            "/v1/imports/crm",
            &batch("another", owned.clone())
        ),
        Err(secure::Error::Status(422))
    ));
    let path = format!(
        "/v1/events/{}?year=2027",
        original[0]["id"].as_str().unwrap()
    );
    for event in [json!({"date":"2027-11-25","title":"Письма"}), owned.clone()] {
        secure::put(port, 44, "/v1/imports/crm", &batch("partners", event)).unwrap();
        assert_eq!(
            secure::get(port, 33, &path).unwrap()["ownerId"],
            json!(former)
        );
    }
    let mut reassigned = owned.clone();
    reassigned["ownerId"] = json!(active);
    secure::put(port, 44, "/v1/imports/crm", &batch("partners", reassigned)).unwrap();
    assert!(matches!(
        secure::put(port, 44, "/v1/imports/crm", &batch("partners", owned)),
        Err(secure::Error::Status(422))
    ));
    assert_eq!(
        secure::get(port, 33, &path).unwrap()["ownerId"],
        json!(active)
    );
}

#[test]
fn api_preserves_legacy_owner_and_reports_linked_employee() {
    use godovoy_plan::model::Record;
    let fixture = Fixture::new();
    let phone = Id::new_v4();
    let computer = fixture.store.lock().unwrap().state().device;
    fixture
        .store
        .lock()
        .unwrap()
        .transaction(|s| {
            let revision = s.revision()?;
            s.workspace_mut()?
                .calendar
                .members
                .insert(phone, Record::new(revision, "Телефон Анны".into()));
            Ok(())
        })
        .unwrap();
    let port = fixture.server.port();
    let batch = json!({"events":[{"externalId":"spring","event":{"date":"2027-03-15","title":"Весенняя акция","ownerId":phone}}]});
    secure::put(port, 22, "/v1/imports/crm", &batch).unwrap();
    fixture
        .store
        .lock()
        .unwrap()
        .link_employee_device(phone, computer)
        .unwrap();
    let rows = secure::get(port, 11, "/v1/members").unwrap();
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == json!(phone) && row["employeeId"] == json!(computer))
    );
    let events = secure::get(port, 11, "/v1/events?year=2027").unwrap();
    assert_eq!(events[0]["ownerId"], json!(phone));
    assert_eq!(events[0]["employeeId"], json!(computer));
    let second = json!({"events":[{"externalId":"autumn","event":{"date":"2027-10-01","title":"Осенняя акция","ownerId":phone}}]});
    secure::put(port, 22, "/v1/imports/crm", &second).unwrap();
    let events = secure::get(port, 11, "/v1/events?year=2027").unwrap();
    assert_eq!(events[1]["ownerId"], json!(computer));
}
