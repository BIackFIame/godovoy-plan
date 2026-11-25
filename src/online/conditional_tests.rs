mod conditional_tests {
    use super::*;
    use chrono::NaiveDate;
    use godovoy_plan::storage::online::ProtectionSettings;

    async fn get(f: &Fixture, year: i32, key: &str, conditions: &[&str]) -> Response {
        let mut request = Request::builder()
            .uri(format!("/v1/state?year={year}"))
            .header(header::HOST, "calendar.test")
            .header(header::AUTHORIZATION, format!("Bearer {key}"));
        for condition in conditions {
            request = request.header(header::IF_NONE_MATCH, *condition);
        }
        f.app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn poll(f: &Fixture, year: i32, key: &str, tag: &str) -> Response {
        let request = Request::builder()
            .uri(format!("/v1/state?year={year}"))
            .header(header::HOST, "calendar.test")
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .header(header::IF_NONE_MATCH, tag)
            .header("x-godovoy-delta", "event-v1")
            .body(Body::empty())
            .unwrap();
        f.app.clone().oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn head_confirms_the_authorized_year_without_sending_a_calendar_body() {
        let f = Fixture::new();
        let (tag, _) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let check = || {
            let request = Request::builder()
                .method(Method::HEAD)
                .uri("/v1/state?year=2028")
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header(header::IF_NONE_MATCH, &tag)
                .body(Body::empty())
                .unwrap();
            f.app.clone().oneshot(request)
        };

        assert_unchanged(check().await.unwrap(), &tag).await;
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let changed = check().await.unwrap();
        assert_eq!(changed.status(), StatusCode::OK);
        assert_ne!(changed.headers()[header::ETAG], tag.as_str());
        assert!(to_bytes(changed.into_body(), 0).await.unwrap().is_empty());

        f.vault.lock().unwrap().revoke(f.credential).unwrap();
        let denied = check().await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert!(denied.headers().get(header::ETAG).is_none());
        assert!(to_bytes(denied.into_body(), 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn colleague_poll_rebuilds_only_its_own_representation_and_rejects_stale_bases() {
        let f = Fixture::new();
        let (grant, _, reader) = f.vault.lock().unwrap().grant("Обзор", None, true).unwrap();
        let (_, personal, restricted) = f.vault.lock().unwrap().grant("Свои", None, true).unwrap();
        f.vault
            .lock()
            .unwrap()
            .set_employee_scope(personal, true)
            .unwrap();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let (writer, own) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let (base, mut prior) = snapshot(get(&f, 2028, &reader, &[]).await).await;
        let (limited, empty) = snapshot(get(&f, 2028, &restricted, &[]).await).await;
        assert!(empty["events"].as_array().unwrap().is_empty());
        let id = &own["events"][0]["id"];
        let note = |before: &str, after: &str| json!({"op":"note","eventId":id,"year":2028,"base":before,"text":after});
        let ack = save_with_tag(&f, &f.key, &writer, &note("", "у коллеги")).await;
        let writer2 = ack.headers()["x-godovoy-state-tag"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_unchanged(poll(&f, 2028, &restricted, &limited).await, &limited).await;
        let resp = poll(&f, 2028, &reader, &base).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()["x-godovoy-delta-base"], base);
        assert!(!resp.headers().contains_key(header::ETAG));
        let reader2 = resp.headers()["x-godovoy-state-tag"]
            .to_str()
            .unwrap()
            .to_owned();
        let patch: Value =
            serde_json::from_slice(&to_bytes(resp.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(patch["id"], *id);
        for key in ["events", "tasks"] {
            let rows = prior[key].as_array_mut().unwrap();
            rows.retain(|r| r["id"] != *id);
            for change in patch[key].as_array().unwrap() {
                rows.insert(
                    change["index"].as_u64().unwrap() as usize,
                    change["event"].clone(),
                );
            }
        }
        assert_eq!(
            snapshot(get(&f, 2028, &reader, &[]).await).await,
            (reader2.clone(), prior)
        );
        for resp in [
            poll(&f, 2028, &reader, &writer).await,
            poll(&f, 2029, &reader, &base).await,
            get(&f, 2028, &reader, &[&base]).await,
        ] {
            assert_eq!(resp.status(), StatusCode::OK);
            assert!(!resp.headers().contains_key("x-godovoy-delta-base"));
        }
        let second = save_with_tag(&f, &f.key, &writer2, &note("у коллеги", "следующая")).await;
        assert_eq!(second.status(), StatusCode::OK);
        let (fresh, year) = snapshot(poll(&f, 2028, &reader, &base).await).await;
        assert_eq!(year["events"][0]["note"], "следующая");
        let delta = poll(&f, 2028, &reader, &reader2).await;
        assert_eq!(delta.headers()["x-godovoy-state-tag"], fresh);
        f.vault
            .lock()
            .unwrap()
            .set_protection(
                0,
                ProtectionSettings {
                    russia: true,
                    allow_free_text: false,
                    ..Default::default()
                },
            )
            .unwrap();
        let (_, filtered) = snapshot(poll(&f, 2028, &reader, &reader2).await).await;
        assert_eq!(filtered["events"][0]["note"], "");
        f.vault.lock().unwrap().revoke(grant).unwrap();
        let denied = poll(&f, 2028, &reader, &reader2).await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert!(!denied.headers().contains_key("x-godovoy-delta-base"));
    }

    #[tokio::test]
    async fn external_replacement_with_identical_stat_invalidates_colleague_delta() {
        let f = Fixture::new();
        let (_, _, reader) = f.vault.lock().unwrap().grant("Обзор", None, true).unwrap();
        let mut event = f.create();
        event["title"] = json!("original title");
        assert_eq!(f.cmd(event).await, StatusCode::NO_CONTENT);
        let (writer, own) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let (base, _) = snapshot(get(&f, 2028, &reader, &[]).await).await;
        let ack=save_with_tag(&f,&f.key,&writer,&json!({"op":"note","eventId":own["events"][0]["id"],"year":2028,"base":"","text":"новая"})).await;
        assert_eq!(ack.status(), StatusCode::OK);
        let path = f._tmp.path().join("host/online-calendar-v1.json");
        let stamp = path.metadata().unwrap().modified().unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        let changed = original.replace("original title", "external title");
        assert_ne!(changed, original);
        assert_eq!(changed.len(), original.len());
        std::fs::write(&path, changed).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert_eq!(path.metadata().unwrap().modified().unwrap(), stamp);
        let (_, full) = snapshot(poll(&f, 2028, &reader, &base).await).await;
        assert_eq!(full["events"][0]["title"], "external title");
    }

    async fn snapshot(response: Response) -> (String, Value) {
        assert_eq!(response.status(), StatusCode::OK);
        let tag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (tag, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn overview_detail_is_bound_to_the_visible_year_snapshot() {
        let f = Fixture::new();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let event = {
            let lock = f.vault.lock().unwrap();
            let mut data = lock.data().unwrap();
            let (&id, plan) = data.calendar.plans.first_key_value().unwrap();
            let plan = plan.clone();
            for _ in 0..79 {
                data.calendar.plans.insert(Id::new_v4(), plan.clone());
            }
            lock.save_data(&data).unwrap();
            id
        };
        // Exercise the shared digest path on bytes inside its retention limit.
        shared_source(&f);
        let (_, _, other) = f
            .vault
            .lock()
            .unwrap()
            .grant("Читатель", None, true)
            .unwrap();
        let index = || async {
            let built = Request::builder()
                .uri("/v1/state?year=2028")
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header("x-godovoy-view", "overview-v1")
                .body(Body::empty())
                .unwrap();
            snapshot(f.app.clone().oneshot(built).await.unwrap()).await
        };
        async fn detail(f: &Fixture, event: Id, key: &str, tag: &str) -> Response {
            let built = Request::builder()
                .uri(format!("/v1/event?year=2028&eventYear=2028&id={event}"))
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::IF_MATCH, tag)
                .body(Body::empty())
                .unwrap();
            f.app.clone().oneshot(built).await.unwrap()
        }
        let (legacy, old) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let (tag, overview) = index().await;
        assert_ne!(legacy, tag);
        assert_eq!(overview["overview"], true);
        assert_eq!(overview["events"].as_array().unwrap().len(), 80);
        assert!(overview["events"][0].get("checklist").is_none());
        assert_eq!(
            detail(&f, event, &other, &tag).await.status(),
            StatusCode::PRECONDITION_FAILED
        );
        assert_eq!(
            detail(&f, event, &f.key, &legacy).await.status(),
            StatusCode::PRECONDITION_FAILED
        );
        let card = detail(&f, event, &f.key, &tag).await;
        assert_eq!(card.status(), StatusCode::OK);
        assert_eq!(card.headers()["x-godovoy-state-tag"], tag);
        assert_eq!(
            serde_json::from_slice::<Value>(&to_bytes(card.into_body(), 65_536).await.unwrap())
                .unwrap(),
            *old["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == event.to_string())
                .unwrap()
        );
        assert_eq!(
            f.cmd(
                json!({"op":"note","eventId":event,"year":2028,"base":"","text":"Отдельная запись"})
            )
            .await,
            StatusCode::NO_CONTENT
        );
        let (fresh, summaries) = index().await;
        assert_ne!(fresh, tag);
        assert_eq!(overview, summaries);
        assert_eq!(
            detail(&f, event, &f.key, &tag).await.status(),
            StatusCode::PRECONDITION_FAILED
        );
        f.vault
            .lock()
            .unwrap()
            .set_protection(
                0,
                ProtectionSettings {
                    russia: true,
                    allow_free_text: false,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            detail(&f, event, &f.key, &fresh).await.status(),
            StatusCode::PRECONDITION_FAILED
        );
        let (restricted, _) = index().await;
        let card = detail(&f, event, &f.key, &restricted).await;
        assert_eq!(card.status(), StatusCode::OK);
        let value: Value =
            serde_json::from_slice(&to_bytes(card.into_body(), 65_536).await.unwrap()).unwrap();
        assert_eq!(value["note"], "");
        assert_eq!(value["description"], "");
        f.vault
            .lock()
            .unwrap()
            .set_employee_scope(f.employee, true)
            .unwrap();
        let (_, personal) = index().await;
        assert_eq!(personal["events"].as_array().unwrap().len(), 80);
        assert!(personal.get("overview").is_none());
        assert_eq!(
            detail(&f, event, &f.key, &restricted).await.status(),
            StatusCode::PRECONDITION_FAILED
        );
    }

    async fn assert_unchanged(response: Response, tag: &str) {
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], tag);
        assert_eq!(
            response.headers()[header::VARY],
            "Authorization, X-Godovoy-Delta, X-Godovoy-View, If-None-Match"
        );
        assert!(
            response.headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("no-store")
        );
        assert!(to_bytes(response.into_body(), 0).await.unwrap().is_empty());
    }

    async fn save_with_tag(f: &Fixture, key: &str, tag: &str, command: &Value) -> Response {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/command")
            .header(header::HOST, "calendar.test")
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-godovoy-year", "2028")
            .header("x-godovoy-base", tag)
            .body(Body::from(command.to_string()))
            .unwrap();
        f.app.clone().oneshot(req).await.unwrap()
    }

    fn shared_source(f: &Fixture) -> (std::path::PathBuf, Vec<u8>) {
        let path = f._tmp.path().join("host/online-calendar-v1.json");
        let mut bytes = std::fs::read(&path).unwrap();
        // Valid trailing whitespace places this ordinary event inside the cache size range.
        bytes.resize(160 * 1024, b' ');
        std::fs::write(&path, &bytes).unwrap();
        (path, bytes)
    }

    #[tokio::test]
    async fn failed_cached_note_preserves_the_file_and_other_readers_year() {
        let f = Fixture::new();
        let (_, _, other) = f.vault.lock().unwrap().grant("Борис", None, false).unwrap();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let (path, bytes) = shared_source(&f);
        let (tag, own) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let (other_tag, prior) = snapshot(get(&f, 2028, &other, &[]).await).await;
        let command = json!({"op":"note", "eventId":own["events"][0]["id"],
            "year":2028, "base":"", "text":"Не сохранять"});

        // Rejection by the mandatory audit happens after mutation of the working Data.
        let journal = f._tmp.path().join("host").join(format!(
            "security-{}.jsonl",
            chrono::Utc::now().format("%Y-%m")
        ));
        let intact = std::fs::read(&journal).unwrap();
        std::fs::write(&journal, b"unfinished synthetic record").unwrap();
        assert_eq!(
            save_with_tag(&f, &f.key, &tag, &command).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::write(journal, intact).unwrap();

        assert_unchanged(get(&f, 2028, &f.key, &[&tag]).await, &tag).await;
        assert_eq!(
            snapshot(get(&f, 2028, &other, &[]).await).await,
            (other_tag, prior)
        );
        assert_eq!(
            save_with_tag(&f, &f.key, &tag, &command).await.status(),
            StatusCode::OK
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_atomic_save_cannot_publish_a_working_note_from_cache() {
        use std::os::unix::fs::PermissionsExt;

        let f = Fixture::new();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let (path, bytes) = shared_source(&f);
        let (tag, prior) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let root = path.parent().unwrap();
        let permissions = std::fs::metadata(root).unwrap().permissions();
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Privileged test processes can still write to a read-only directory.
        let can_stage = tempfile::NamedTempFile::new_in(root).is_ok();
        let command = json!({"op":"note", "eventId":prior["events"][0]["id"],
            "year":2028,"base":"","text":"Новая запись"});
        let result = save_with_tag(&f, &f.key, &tag, &command).await.status();
        if !can_stage {
            assert_eq!(result, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(f.cmd(command).await, StatusCode::SERVICE_UNAVAILABLE);
        }
        std::fs::set_permissions(root, permissions).unwrap();
        let (_, current) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        if can_stage {
            assert_eq!(result, StatusCode::OK);
            assert_eq!(current["events"][0]["note"], "Новая запись");
        } else {
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(current, prior);
            let journal = root.join(format!(
                "security-{}.jsonl",
                chrono::Utc::now().format("%Y-%m")
            ));
            assert!(
                std::fs::read_to_string(journal)
                    .unwrap()
                    .contains("note_requested")
            );
        }
    }

    #[tokio::test]
    async fn revocation_denies_cached_note_with_valid_previous_tag() {
        let f = Fixture::new();
        let (_, _, other) = f.vault.lock().unwrap().grant("Борис", None, false).unwrap();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let (path, bytes) = shared_source(&f);
        let (tag, own) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let (_, prior) = snapshot(get(&f, 2028, &other, &[]).await).await;
        let command = json!({"op":"note", "eventId":own["events"][0]["id"],
            "year":2028, "base":"", "text":"После отзыва"});

        f.vault.lock().unwrap().revoke(f.credential).unwrap();
        let denied = save_with_tag(&f, &f.key, &tag, &command).await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert!(!denied.headers().contains_key("x-godovoy-state-tag"));
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        let (_, current) = snapshot(get(&f, 2028, &other, &[]).await).await;
        assert_eq!(current["events"], prior["events"]);
    }

    #[tokio::test]
    async fn matching_size_and_timestamp_still_require_current_calendar_bytes() {
        let f = Fixture::new();
        let mut event = f.create();
        event["title"] = json!("First title");
        assert_eq!(f.cmd(event).await, StatusCode::NO_CONTENT);
        let (tag, _) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let path = f._tmp.path().join("host/online-calendar-v1.json");
        let modified = path.metadata().unwrap().modified().unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        let changed = original.replace("First title", "Other title");
        assert_ne!(original, changed);
        assert_eq!(original.len(), changed.len());
        std::fs::write(&path, changed).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(path.metadata().unwrap().modified().unwrap(), modified);
        let (fresh_tag, state) = snapshot(get(&f, 2028, &f.key, &[&tag]).await).await;
        assert_ne!(tag, fresh_tag);
        assert_eq!(state["events"][0]["title"], "Other title");
    }

    #[tokio::test]
    async fn receipt_matches_entire_authorized_snapshot_and_detects_other_changes() {
        let f = Fixture::new();
        let (_, colleague, other_key) =
            f.vault.lock().unwrap().grant("Борис", None, false).unwrap();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let (tag, mut original) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let id = original["events"][0]["id"].clone();
        let command = json!({"op":"note","eventId":id,"year":2028,"base":"","text":"Запись"});
        let reply = save_with_tag(&f, &f.key, &tag, &command).await;
        assert_eq!(reply.status(), StatusCode::OK);
        let new_tag = reply.headers()["x-godovoy-state-tag"]
            .to_str()
            .unwrap()
            .to_owned();
        let patch: Value =
            serde_json::from_slice(&to_bytes(reply.into_body(), 65536).await.unwrap()).unwrap();
        for field in ["events", "tasks"] {
            let rows = original[field].as_array_mut().unwrap();
            rows.retain(|row| row["id"] != id);
            for row in patch[field].as_array().unwrap() {
                assert_eq!(row["event"]["id"], id);
                rows.insert(
                    row["index"].as_u64().unwrap() as usize,
                    row["event"].clone(),
                );
            }
        }
        let (expected_tag, expected) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        assert_eq!(new_tag, expected_tag);
        assert_eq!(original, expected);
        let mut concurrent = f.create();
        concurrent["ownerId"] = json!(colleague);
        assert_eq!(
            f.request("/v1/command", &other_key, Some(concurrent))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let check = json!({"op":"check","eventId":id,"year":2028,"taskId":original["events"][0]["checklist"][0]["id"],"done":true});
        let reply = save_with_tag(&f, &f.key, &new_tag, &check).await;
        assert_eq!(reply.status(), StatusCode::NO_CONTENT);
        assert!(!reply.headers().contains_key("x-godovoy-state-tag"));
        let (_, refreshed) = snapshot(get(&f, 2028, &f.key, &[&new_tag]).await).await;
        assert_eq!(refreshed["events"].as_array().unwrap().len(), 2);
        let (other_tag, _) = snapshot(get(&f, 2028, &other_key, &[]).await).await;
        assert_eq!(
            save_with_tag(&f, &f.key, &other_tag, &check).await.status(),
            StatusCode::NO_CONTENT
        );
        f.vault.lock().unwrap().revoke(f.credential).unwrap();
        assert_eq!(
            save_with_tag(&f, &f.key, &new_tag, &check).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn private_colleague_edits_are_absent_from_receipts_and_keep_the_visible_base() {
        let f = Fixture::new();
        let (_, colleague, colleague_key) =
            f.vault.lock().unwrap().grant("Борис", None, false).unwrap();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        f.vault
            .lock()
            .unwrap()
            .set_employee_scope(f.employee, true)
            .unwrap();
        let (base, state) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let mut private = f.create();
        private["ownerId"] = json!(colleague);
        private["title"] = json!("Приватный заголовок");
        assert_eq!(
            f.request("/v1/command", &colleague_key, Some(private))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let mut command = json!({"op":"note", "eventId":state["events"][0]["id"], "year":2028, "base":"", "text":"Готово"});
        let reply = save_with_tag(&f, &f.key, &base, &command).await;
        assert_eq!(reply.status(), StatusCode::OK);
        let tag = reply.headers()["x-godovoy-state-tag"]
            .to_str()
            .unwrap()
            .to_owned();
        let bytes = to_bytes(reply.into_body(), 65536).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(!body.contains("Приватный заголовок"));
        assert!(!body.contains(&colleague.to_string()));
        assert_unchanged(get(&f, 2028, &f.key, &[&tag]).await, &tag).await;
        let (_, all) = snapshot(get(&f, 2028, &colleague_key, &[]).await).await;
        command["eventId"] = all["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["title"] == "Приватный заголовок")
            .unwrap()["id"]
            .clone();
        let denied = save_with_tag(&f, &f.key, &tag, &command).await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert!(!denied.headers().contains_key("x-godovoy-state-tag"));
    }

    #[tokio::test]
    async fn large_full_read_requires_a_durable_audit_entry() {
        let f = Fixture::new();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        {
            let lock = f.vault.lock().unwrap();
            let mut data = lock.data().unwrap();
            let template = data.calendar.plans.values().next().unwrap().clone();
            for _ in 0..600 {
                data.calendar.plans.insert(Id::new_v4(), template.clone());
            }
            lock.save_data(&data).unwrap();
            assert!(lock.data_bytes().unwrap().len() >= 128 * 1024);
        }
        let (_, body) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        assert_eq!(body["events"].as_array().unwrap().len(), 601);
        let log = f._tmp.path().join("host").join(format!(
            "security-{}.jsonl",
            chrono::Utc::now().format("%Y-%m")
        ));
        assert!(
            std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .last()
                .unwrap()
                .contains("state_requested")
        );
        std::fs::write(log, b"unfinished synthetic record").unwrap();
        let denied = get(&f, 2028, &f.key, &[]).await;
        assert_eq!(denied.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!denied.headers().contains_key(header::ETAG));
    }

    #[tokio::test]
    async fn conditional_reads_match_http_tags_and_scope_changes() {
        let f = Fixture::new();
        let (_, colleague, colleague_key) =
            f.vault.lock().unwrap().grant("Борис", None, false).unwrap();
        let (tag, _) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let weak = format!("W/{tag}");
        let list = format!("\"other,tag\", {weak}");
        for values in [
            vec![tag.as_str()],
            vec![weak.as_str()],
            vec![list.as_str()],
            vec!["\"another\"", weak.as_str()],
            vec!["*"],
        ] {
            let response = get(&f, 2028, &f.key, &values).await;
            assert_eq!(response.headers()["x-godovoy-assigned-only"], "false");
            assert_eq!(response.headers()["x-godovoy-allow-free-text"], "true");
            assert_unchanged(response, &tag).await;
        }
        for value in ["\"other\"", "invalid", "*, \"other\""] {
            assert_eq!(
                get(&f, 2028, &f.key, &[value]).await.status(),
                StatusCode::OK
            );
        }

        let mut event = f.create();
        event["description"] = json!("Текст для участников");
        assert_eq!(f.cmd(event).await, StatusCode::NO_CONTENT);
        let (changed, state) = snapshot(get(&f, 2028, &f.key, &[&tag]).await).await;
        assert_ne!(changed, tag);
        assert_eq!(state["events"][0]["description"], "Текст для участников");
        let (next_year, state) = snapshot(get(&f, 2029, &f.key, &[&changed]).await).await;
        assert_ne!(next_year, changed);
        assert_eq!(state["year"], 2029);

        f.vault
            .lock()
            .unwrap()
            .set_employee_scope(f.employee, true)
            .unwrap();
        let (personal_tag, state) = snapshot(get(&f, 2028, &f.key, &[&changed]).await).await;
        assert!(state["assignedOnly"].as_bool().unwrap());
        assert_eq!(state["employees"].as_array().unwrap().len(), 1);
        let mut private = f.create();
        private["ownerId"] = json!(colleague);
        assert_eq!(
            f.request("/v1/command", &colleague_key, Some(private))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let response = get(&f, 2028, &f.key, &[&personal_tag]).await;
        assert_eq!(response.headers()["x-godovoy-assigned-only"], "true");
        assert_unchanged(response, &personal_tag).await;
        let (_, other_state) =
            snapshot(get(&f, 2028, &colleague_key, &[&personal_tag]).await).await;
        assert_eq!(other_state["employeeId"], json!(colleague));

        f.vault
            .lock()
            .unwrap()
            .set_protection(
                0,
                ProtectionSettings {
                    belarus: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let (minimized, state) = snapshot(get(&f, 2028, &f.key, &[&personal_tag]).await).await;
        assert_ne!(minimized, personal_tag);
        assert_eq!(state["events"][0]["description"], "");
        let response = get(&f, 2028, &f.key, &[&minimized]).await;
        assert_eq!(response.headers()["x-godovoy-allow-free-text"], "false");
        assert_unchanged(response, &minimized).await;
        f.vault.lock().unwrap().revoke(f.credential).unwrap();
        let denied = get(&f, 2028, &f.key, &[&minimized]).await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert!(!denied.headers().contains_key(header::ETAG));
    }

    #[tokio::test]
    async fn conditional_reads_obey_the_recovery_window_and_require_a_working_journal() {
        let mut f = Fixture::new();
        let (primary_tag, _) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        f.vault.lock().unwrap().begin_standby(5).unwrap();
        assert_eq!(
            get(&f, 2028, &f.key, &[&primary_tag]).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let mut standby = Host::new(
            f.vault.clone(),
            "https://calendar.test",
            Some(EntryPoint::Office),
        )
        .unwrap();
        standby.enable_standby().unwrap();
        let primary = std::mem::replace(&mut f.app, router(standby));
        let first = get(&f, 2028, &f.key, &[]).await;
        let deadline = first.headers()["x-godovoy-standby-until"].clone();
        let (standby_tag, _) = snapshot(first).await;
        let response = get(&f, 2028, &f.key, &[&standby_tag]).await;
        assert_eq!(response.headers()["x-godovoy-standby-until"], deadline);
        assert_unchanged(response, &standby_tag).await;
        f.vault.lock().unwrap().end_standby().unwrap();
        assert_eq!(
            get(&f, 2028, &f.key, &[&standby_tag]).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        f.app = primary;
        let journal = f._tmp.path().join("host").join(format!(
            "security-{}.jsonl",
            chrono::Utc::now().format("%Y-%m")
        ));
        std::fs::write(&journal, b"unfinished synthetic record").unwrap();
        let unavailable = get(&f, 2028, &f.key, &[&primary_tag]).await;
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!unavailable.headers().contains_key(header::ETAG));
    }

    #[tokio::test]
    async fn personal_task_appears_at_midnight_even_when_viewing_a_different_year() {
        let f = Fixture::new();
        let mut event = f.create();
        event["date"] = json!("2028-01-01");
        event["yearly"] = json!(false);
        event["preparationDays"] = json!(0);
        event["checklist"] = json!([]);
        assert_eq!(f.cmd(event).await, StatusCode::NO_CONTENT);
        let lock = f.vault.lock().unwrap();
        let acl = lock.access().unwrap();
        let views = conditional::Views::default();
        let render = |today, conditions: &[HeaderValue]| {
            let context = actions::ViewContext::new(&acl, 2029, f.employee, true);
            views
                .response(
                    f.credential,
                    context,
                    today,
                    lock.open_data().unwrap(),
                    conditions,
                    true,
                )
                .unwrap()
        };
        let before = NaiveDate::from_ymd_opt(2027, 12, 31).unwrap();
        let (old_tag, old) = snapshot(render(before, &[])).await;
        assert_eq!(old["tasks"], json!([]));
        // A genuine one-file transition is known, but the client's base did
        // not cover the new local date. The task entering its window needs a full response.
        let path = f._tmp.path().join("host/online-calendar-v1.json");
        let mut raw = std::fs::read(&path).unwrap();
        let before_raw = conditional::InputHasher::digest(&raw).into();
        raw.push(b' ');
        std::fs::write(&path, &raw).unwrap();
        let event = *lock.data().unwrap().calendar.plans.keys().next().unwrap();
        views.transition(Some(conditional::EventTransition {
            before: before_raw,
            after: conditional::InputHasher::digest(&raw).into(),
            event,
        }));
        let condition = [HeaderValue::from_str(&old_tag).unwrap()];
        let (new_tag, new) = snapshot(render(before.succ_opt().unwrap(), &condition)).await;
        assert_ne!(new_tag, old_tag);
        assert_eq!(new["events"], json!([]));
        assert_eq!(new["tasks"].as_array().unwrap().len(), 1);
    }
}
