mod conditional_tests {
    use super::*;
    use blake2::Digest;
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
    async fn calendar_watch_only_signals_authorized_committed_changes_in_the_open_year() {
        let f = Fixture::new();
        let (other_id, other_employee, other_key) =
            f.vault.lock().unwrap().grant("Иван", None, false).unwrap();
        let (_, reader_id, observer) = f.vault.lock().unwrap().grant("Ольга", None, true).unwrap();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let event = &f.state().await["events"][0];
        for employee in [f.employee, other_employee] {
            f.vault
                .lock()
                .unwrap()
                .set_employee_scope(employee, true)
                .unwrap();
        }
        let stamp = |key: &str, year: i32, session: bool| {
            let path = if session {
                "/v1/session".to_string()
            } else {
                format!("/v1/state?year={year}")
            };
            let request = Request::builder()
                .uri(path)
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header("x-godovoy-watch-year", year.to_string())
                .body(Body::empty())
                .unwrap();
            let app = f.app.clone();
            async move {
                let response = app.oneshot(request).await.unwrap();
                (
                    response.status(),
                    response.headers().get("x-godovoy-watch").cloned(),
                )
            }
        };
        let zero = Some(HeaderValue::from_static("0"));
        assert_eq!(stamp(&f.key, 2028, true).await.1, None);
        for (key, year) in [
            (&f.key, 2028),
            (&other_key, 2028),
            (&observer, 2028),
            (&f.key, 2029),
        ] {
            assert_eq!(
                stamp(key, year, false).await,
                (StatusCode::OK, zero.clone())
            );
        }
        let (_, _, another_full_key) = f
            .vault
            .lock()
            .unwrap()
            .grant("Второй читатель", None, true)
            .unwrap();
        assert_eq!(stamp(&another_full_key, 2028, true).await.1, None);
        let change = json!({"op":"check","eventId":event["id"],"taskId":event["checklist"][0]["id"],"year":2028,"done":true});
        assert_eq!(
            f.request("/v1/command", &other_key, Some(change.clone()))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(stamp(&f.key, 2028, true).await.1, zero);
        assert_eq!(f.cmd(change.clone()).await, StatusCode::NO_CONTENT);
        let updated = stamp(&f.key, 2028, true).await.1.unwrap();
        assert_eq!(updated.as_bytes().len(), 32);
        assert_eq!(stamp(&f.key, 2028, false).await.1, Some(updated.clone()));
        assert_ne!(stamp(&observer, 2028, true).await.1, zero);
        assert_eq!(stamp(&other_key, 2028, true).await.1, zero);
        assert_eq!(stamp(&f.key, 2029, true).await.1, zero);
        assert_eq!(f.cmd(change).await, StatusCode::NO_CONTENT);
        assert_eq!(stamp(&f.key, 2028, true).await.1, Some(updated));
        let (_, _, mcp_key) = f
            .vault
            .lock()
            .unwrap()
            .grant_mcp(
                "Анна",
                f.employee,
                godovoy_plan::storage::online::GrantPermissions {
                    read_only: false,
                    allow_remote: false,
                },
            )
            .unwrap();
        let mcp_change = json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{
            "name":"set_my_task_done","arguments":{"eventId":event["id"],"taskId":event["checklist"][0]["id"],"year":2028,"done":false}
        }});
        // The owner views the full office calendar while their assistant remains personal.
        f.vault.lock().unwrap().set_employee_scope(f.employee, false).unwrap();
        assert_eq!(stamp(&f.key, 2028, false).await.1, zero);
        assert_eq!(stamp(&f.key, 2029, false).await.1, zero);
        let before = stamp(&f.key, 2028, true).await.1;
        let mut previous = None;
        for _ in 0..2 {
            let request = Request::builder()
                .method(Method::POST)
                .uri("/mcp")
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {mcp_key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                .body(Body::from(mcp_change.to_string()))
                .unwrap();
            let reply = f.app.clone().oneshot(request).await.unwrap();
            assert_eq!(reply.status(), StatusCode::OK);
            assert_eq!(reply.headers()["x-godovoy-assigned-only"], "true");
            let body = to_bytes(reply.into_body(), 65536).await.unwrap();
            let reply: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                reply["result"]["structuredContent"]["saved"], true,
                "{reply}"
            );
            let current = stamp(&f.key, 2028, true).await.1.unwrap();
            assert_ne!(Some(&current), before.as_ref());
            if let Some(old) = previous.replace(current.clone()) {
                assert_eq!(current, old);
            }
            assert_eq!(stamp(&other_key, 2028, true).await.1, zero);
            assert_eq!(stamp(&f.key, 2029, true).await.1, zero);
        }
        let full_stamp = stamp(&observer, 2028, true).await.1;
        f.vault
            .lock()
            .unwrap()
            .set_employee_scope(reader_id, true)
            .unwrap();
        assert_eq!(stamp(&observer, 2028, false).await.1, zero);
        assert_eq!(f.cmd(json!({"op":"note","eventId":event["id"],"year":2028,"text":"Перезвонить","base":""})).await, StatusCode::NO_CONTENT);
        assert_eq!(stamp(&observer, 2028, true).await.1, zero);
        f.vault
            .lock()
            .unwrap()
            .set_employee_scope(reader_id, false)
            .unwrap();
        assert_eq!(stamp(&observer, 2028, true).await.1, full_stamp);
        f.vault.lock().unwrap().revoke(other_id).unwrap();
        assert_eq!(
            stamp(&other_key, 2028, true).await,
            (StatusCode::UNAUTHORIZED, None)
        );
    }

    #[tokio::test]
    async fn packed_small_cards_keep_exact_rows_and_separate_conditional_tags() {
        let f = Fixture::new();
        let today = chrono::Local::now().date_naive();
        let year = chrono::Datelike::year(&today);
        let mut event = f.create();
        event["date"] = json!(today.to_string());
        assert_eq!(f.cmd(event).await, StatusCode::NO_CONTENT);
        let (_, _, key) = f.vault.lock().unwrap().grant_with_permissions("Анна", Some(f.employee),
            godovoy_plan::storage::online::GrantPermissions {read_only: true, allow_remote: true}).unwrap();
        let remote = router(Host::new(f.vault.clone(), "https://calendar.test", Some(EntryPoint::Remote)).unwrap());
        let get = |app: Router, offer: Option<&str>, tag: Option<&str>| {
            let mut q = Request::builder().uri(format!("/v1/state?year={year}"))
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header("x-godovoy-view", "overview-v1").header("x-godovoy-rows", "tuple-v2")
                .header("x-godovoy-assigned-view", "overview-v1");
            if let Some(v) = offer { q = q.header("x-godovoy-cards", v); }
            if let Some(v) = tag { q = q.header(header::IF_NONE_MATCH, v); }
            app.oneshot(q.body(Body::empty()).unwrap())
        };
        let (named, legacy) = snapshot(get(f.app.clone(), None, None).await.unwrap()).await;
        let (tag, packed) = snapshot(get(f.app.clone(), Some("tuple-v1"), Some(&named)).await.unwrap()).await;
        assert_ne!(tag, named);
        assert_eq!(packed["cardFormat"], "tuple-v1");
        assert!(packed.get("assignedOwnerId").is_none());
        let e = &legacy["events"][0];
        let task = &e["checklist"][0];
        let checklist = json!([[task["id"],task["title"],task["daysBefore"],task["dueDate"],task["done"]]]);
        assert_eq!(packed["events"][0], json!([e["id"],e["year"],e["title"],e["description"],
            e["category"],e["date"],e["yearly"],e["ownerId"],e["preparationDays"],e["preparationDate"],
            checklist,e["note"],e["revision"]]));
        assert_eq!(packed["tasks"], json!([0]));
        assert_eq!(legacy["tasks"][0], *e);
        assert_unchanged(get(f.app.clone(), Some("tuple-v1"), Some(&tag)).await.unwrap(), &tag).await;
        assert_eq!(snapshot(get(f.app.clone(), None, Some(&tag)).await.unwrap()).await, (named.clone(), legacy.clone()));
        assert_eq!(snapshot(get(f.app.clone(), Some("unknown"), None).await.unwrap()).await, (named, legacy.clone()));
        let (remote_named, remote_legacy) = snapshot(get(remote.clone(), None, None).await.unwrap()).await;
        let (remote_tag, remote_packed) =
            snapshot(get(remote.clone(), Some("tuple-v1"), Some(&remote_named)).await.unwrap()).await;
        assert_ne!(remote_tag, remote_named);
        assert_eq!(remote_packed, packed);
        assert_eq!(remote_legacy["events"][0], *e);
        assert_unchanged(get(remote.clone(), Some("tuple-v1"), Some(&remote_tag)).await.unwrap(), &remote_tag).await;
        f.vault.lock().unwrap().set_employee_scope(f.employee, true).unwrap();
        let (_, scoped) = snapshot(get(f.app.clone(), Some("tuple-v1"), Some(&tag)).await.unwrap()).await;
        assert_eq!(scoped["assignedOwnerId"], f.employee.to_string());
        assert_eq!(scoped["events"], packed["events"]);
        assert_eq!(scoped["tasks"], packed["tasks"]);
        let (_, remote_scoped) =
            snapshot(get(remote, Some("tuple-v1"), Some(&remote_tag)).await.unwrap()).await;
        assert_eq!(remote_scoped, scoped);
    }

    #[tokio::test]
    async fn checklist_volume_selects_an_overview_only_when_requested() {
        let f = Fixture::new();
        let remote = router(
            Host::new(
                f.vault.clone(),
                "https://calendar.test",
                Some(EntryPoint::Remote),
            )
            .unwrap(),
        );
        let (_, _, key) = f
            .vault
            .lock()
            .unwrap()
            .grant_with_permissions(
                "Анна",
                Some(f.employee),
                godovoy_plan::storage::online::GrantPermissions {
                    read_only: true,
                    allow_remote: true,
                },
            )
            .unwrap();
        let state = |method: Method, first: bool, condition: Option<&str>| {
            let mut request = Request::builder()
                .method(method)
                .uri("/v1/state?year=2028")
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header("x-godovoy-view", "overview-v1")
                .header("x-godovoy-rows", "tuple-v2");
            if first {
                request = request.header("x-godovoy-page", "first-v1");
            }
            if let Some(tag) = condition {
                request = request.header(header::IF_NONE_MATCH, tag);
            }
            request.body(Body::empty()).unwrap()
        };
        let mut command = f.create();
        for count in [64, 63, 1] {
            command["checklist"] = Value::Array(
                (0..count)
                    .map(|i| json!({"title":format!("Пункт {i}"), "daysBefore":0}))
                    .collect(),
            );
            assert_eq!(f.cmd(command.clone()).await, StatusCode::NO_CONTENT);
            let office = f
                .app
                .clone()
                .oneshot(state(Method::GET, true, None))
                .await
                .unwrap();
            let (_, office) = snapshot(office).await;
            assert!(office.get("overview").is_none());
            let response = remote
                .clone()
                .oneshot(state(Method::GET, true, None))
                .await
                .unwrap();
            let snapshot_header = response.headers().get("x-godovoy-state-tag").cloned();
            let (tag, view) = snapshot(response).await;
            assert_eq!(view.get("overview").is_some(), count == 1);
            assert!(view.get("eventCount").is_none());
            if count == 1 {
                assert_eq!(view["rowFormat"], "tuple-v2");
                assert_eq!(view["events"].as_array().unwrap().len(), 3);
                assert_eq!(snapshot_header.unwrap(), tag.as_str());
                let all = remote
                    .clone()
                    .oneshot(state(Method::GET, false, None))
                    .await
                    .unwrap();
                assert_eq!(snapshot(all).await, (tag, view));
            }
        }
        let (_, full) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let events = full["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            events
                .iter()
                .map(|row| row["checklist"].as_array().unwrap().len())
                .sum::<usize>(),
            128
        );

        let fill = |len| {
            let lock = f.vault.lock().unwrap();
            let mut data = lock.data().unwrap();
            let single = data
                .calendar
                .occurrences(2028)
                .find(|row| row.event.tasks.len() == 1)
                .unwrap();
            let plan = data.calendar.plans[&single.id].clone();
            for _ in data.calendar.plans.len()..len {
                data.calendar.plans.insert(Id::new_v4(), plan.clone());
            }
            lock.save_data(&data).unwrap();
        };
        fill(80);
        let response = remote
            .clone()
            .oneshot(state(Method::GET, true, None))
            .await
            .unwrap();
        assert_eq!(
            response.headers()["x-godovoy-state-tag"],
            response.headers()[header::ETAG]
        );
        let (_, exactly_eighty) = snapshot(response).await;
        assert_eq!(exactly_eighty["events"].as_array().unwrap().len(), 80);
        assert!(exactly_eighty.get("eventCount").is_none());

        fill(81);
        let response = remote
            .clone()
            .oneshot(state(Method::GET, true, None))
            .await
            .unwrap();
        let full_tag = response.headers()["x-godovoy-state-tag"]
            .to_str()
            .unwrap()
            .to_owned();
        let (page_tag, page) = snapshot(response).await;
        assert_ne!(page_tag, full_tag);
        assert_eq!(page["eventCount"], 81);
        assert_eq!(page["rowFormat"], "tuple-v2");
        let all = remote
            .clone()
            .oneshot(state(Method::GET, false, Some(&page_tag)))
            .await
            .unwrap();
        let (tag, all) = snapshot(all).await;
        assert_eq!(tag, full_tag);
        assert_eq!(
            page["events"].as_array().unwrap(),
            &all["events"].as_array().unwrap()[..80]
        );
        assert_eq!(page["tasks"], all["tasks"]);
        let first_unchanged = remote
            .clone()
            .oneshot(state(Method::GET, true, Some(&page_tag)))
            .await
            .unwrap();
        assert_unchanged(first_unchanged, &page_tag).await;
        let confirmed = remote
            .oneshot(state(Method::HEAD, false, Some(&full_tag)))
            .await
            .unwrap();
        assert_unchanged(confirmed, &full_tag).await;
    }

    #[tokio::test]
    async fn opted_in_assigned_rows_and_cards_keep_the_canonical_employee_scope() {
        let f = Fixture::new();
        let (_, other, other_key) = f.vault.lock().unwrap().grant("Коллега", None, false).unwrap();
        let today = chrono::Local::now().date_naive();
        let year = chrono::Datelike::year(&today);
        let next = year + 1;
        for owner in [f.employee, other] {
            let mut same = f.create();
            same["ownerId"] = json!(owner);
            same["date"] = json!(today.to_string());
            same["yearly"] = json!(false);
            same["checklist"] = Value::Array(
                (0..43).map(|n| json!({"title":format!("Пункт {n}"),"daysBefore":0})).collect()
            );
            for _ in 0..3 {
                assert_eq!(f.cmd(same.clone()).await, StatusCode::NO_CONTENT);
            }
            same["date"] = json!(format!("{next}-01-01"));
            same["preparationDays"] = json!(366);
            same["title"] = json!(format!("Подготовка {owner}"));
            assert_eq!(f.cmd(same).await, StatusCode::NO_CONTENT);
        }
        // A grant may still name an older registered device ID.
        let canonical = Id::new_v4();
        {
            let lock = f.vault.lock().unwrap();
            let mut stored = serde_json::to_value(lock.data().unwrap()).unwrap();
            let rev = json!(godovoy_plan::model::Revision { sequence: 50, operation: Id::new_v4() });
            stored["calendar"]["members"][f.employee.to_string()] = json!({"revision":rev,"value":"Анна"});
            stored["calendar"]["members"][canonical.to_string()] = json!({"revision":rev,"value":"Анна"});
            stored["calendar"]["device_people"] =
                json!({(f.employee.to_string()):{"revision":rev,"value":canonical}});
            let data = serde_json::from_value(stored).unwrap();
            lock.save_data(&data).unwrap();
            lock.set_employee_scope(f.employee, true).unwrap();
            lock.set_employee_scope(other, true).unwrap();
        }
        let compact = |uri: &str, key: &str| {
            Request::builder().uri(uri)
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header("x-godovoy-view", "overview-v1")
                .header("x-godovoy-rows", "tuple-v2")
                .header("x-godovoy-assigned-view", "overview-v1")
        };
        let state = |key: &str| {
            f.app.clone().oneshot(compact(&format!("/v1/state?year={year}"), key)
                .header("x-godovoy-page", "first-v1")
                .body(Body::empty()).unwrap())
        };
        let (tag_a, a) = snapshot(state(&f.key).await.unwrap()).await;
        let (tag_b, b) = snapshot(state(&other_key).await.unwrap()).await;
        for (view, owner) in [(&a,canonical),(&b,other)] {
            assert_eq!(view["assignedOnly"], true);
            assert_eq!(view["assignedOwnerId"], json!(owner));
            assert_eq!(view["rowFormat"],"tuple-v2");
            assert_eq!(view["events"].as_array().unwrap().len(),3);
            assert!(view.get("eventCount").is_none());
            for row in view["events"].as_array().unwrap().iter().chain(view["tasks"].as_array().unwrap()) {
                let related = row[5].as_u64().map_or_else(|| row[5].clone(), |i| view["employees"][i as usize]["id"].clone());
                assert_eq!(related,json!(owner));
            }
        }
        let (legacy_tag, legacy) = snapshot(get(&f, year, &f.key, &[]).await).await;
        assert!(legacy.get("overview").is_none());
        assert_ne!(legacy_tag,tag_a);
        assert_ne!(tag_a, tag_b);
        let adjacent = |view: &Value| {
            view["tasks"].as_array().unwrap().iter().find(|row| row[1]==next).unwrap()[0]
                .as_str().unwrap().to_owned()
        };
        let mine = adjacent(&a);
        let theirs = adjacent(&b);
        let detail = |id: &str, tag: &str| {
            f.app.clone().oneshot(compact(
                &format!("/v1/event?year={year}&eventYear={next}&id={id}"), &f.key,
            ).header(header::IF_MATCH, tag).body(Body::empty()).unwrap())
        };
        assert_eq!(detail(&mine, &tag_b).await.unwrap().status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(detail(&theirs, &tag_a).await.unwrap().status(), StatusCode::NOT_FOUND);
        let card = detail(&mine, &tag_a).await.unwrap();
        assert_eq!(card.status(), StatusCode::OK);
        let data: Value = serde_json::from_slice(&to_bytes(card.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(data["ownerId"], json!(canonical));
        assert_eq!(data["checklist"].as_array().unwrap().len(), 43);
        let note = |id: &str| {
            f.app.clone().oneshot(compact("/v1/command", &f.key).method(Method::POST)
                .header(header::CONTENT_TYPE,"application/json")
                .header("x-godovoy-base", &tag_a).header("x-godovoy-year", year.to_string())
                .body(Body::from(json!({"op":"note","eventId":id,"year":next,"base":"","text":"Готово"}).to_string())).unwrap())
        };
        assert_eq!(note(&theirs).await.unwrap().status(), StatusCode::FORBIDDEN);
        let saved = note(&mine).await.unwrap();
        assert_eq!(saved.status(), StatusCode::OK);
        let receipt: Value = serde_json::from_slice(&to_bytes(saved.into_body(),65536).await.unwrap()).unwrap();
        assert_eq!(receipt["events"],json!([]));
        assert_eq!(receipt["detail"]["note"],"Готово");
        f.vault.lock().unwrap().revoke(f.credential).unwrap();
        assert_eq!(detail(&mine, &tag_a).await.unwrap().status(), StatusCode::UNAUTHORIZED);
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

    fn fill_calendar_audit(f: &Fixture) {
        let lock = f.vault.lock().unwrap();
        let mut data = lock.data().unwrap();
        let last = data.audit.back().unwrap().clone();
        data.audit.resize(1024, last);
        lock.save_data(&data).unwrap();
        assert!(lock.data_bytes().unwrap().len() >= 128 * 1024);
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
        fill_calendar_audit(&f);
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
        assert_eq!(patch["events"][0]["event"]["note"], "у коллеги");
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
        fill_calendar_audit(&f);
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
        let (file, original) = shared_source(&f);
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
        let compact_request = |condition: &str| {
            Request::builder().uri("/v1/state?year=2028")
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header("x-godovoy-view", "overview-v1")
                .header("x-godovoy-rows", "tuple-v2")
                .header(header::IF_NONE_MATCH, condition)
                .body(Body::empty()).unwrap()
        };
        let compact = f.app.clone().oneshot(compact_request(&tag)).await.unwrap();
        assert_eq!(compact.status(), StatusCode::OK);
        let (compact_tag, compact) = snapshot(compact).await;
        assert_ne!(tag, compact_tag);
        assert_eq!(compact["rowFormat"], "tuple-v2");
        for group in ["events", "tasks"] {
            let named = overview[group].as_array().unwrap();
            let tuples = compact[group].as_array().unwrap();
            assert_eq!(named.len(), tuples.len());
            for (row, tuple) in named.iter().zip(tuples) {
                let roster = overview["employees"].as_array().unwrap();
                let owner_ref = roster.iter().position(|employee| employee["id"] == row["ownerId"])
                    .map_or_else(|| row["ownerId"].clone(), |i| json!(i));
                assert_eq!(*tuple, json!([row["id"],row["year"],row["title"],row["date"],row["category"],
                    owner_ref,row["completed"],row["total"],row["next"]]));
            }
        }
        assert_unchanged(f.app.clone().oneshot(compact_request(&compact_tag)).await.unwrap(), &compact_tag).await;
        assert_eq!(detail(&f, event, &f.key, &compact_tag).await.status(), StatusCode::PRECONDITION_FAILED);

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
        let modified = file.metadata().unwrap().modified().unwrap();
        let mut damaged = original.clone();
        *damaged.last_mut().unwrap() = b'!';
        std::fs::write(&file, damaged).unwrap();
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(modified).unwrap();
        assert_eq!(
            detail(&f, event, &f.key, &tag).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        std::fs::write(&file, &original).unwrap();
        assert_eq!(detail(&f, event, &f.key, &tag).await.status(), StatusCode::OK);
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
        let reduced = f.app.clone().oneshot(compact_request(&compact_tag)).await.unwrap();
        let (_, reduced) = snapshot(reduced).await;
        assert_eq!(reduced, personal);
        assert!(reduced.get("rowFormat").is_none());
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
            "Authorization, X-Godovoy-Delta, X-Godovoy-View, X-Godovoy-Rows, X-Godovoy-Assigned-View, X-Godovoy-Cards, X-Godovoy-Page, X-Godovoy-Continue, X-Godovoy-Base, If-None-Match"
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
    async fn cached_source_checks_the_last_chunk_and_current_credentials() {
        let f = Fixture::new();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let (path, bytes) = shared_source(&f);
        let (tag, _) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        assert_unchanged(get(&f, 2028, &f.key, &[&tag]).await, &tag).await;
        let modified = path.metadata().unwrap().modified().unwrap();
        let mut changed = bytes.clone();
        *changed.last_mut().unwrap() = b'!';
        std::fs::write(&path, &changed).unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap()
            .set_modified(modified).unwrap();
        let invalid = get(&f, 2028, &f.key, &[&tag]).await;
        assert_eq!(invalid.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!invalid.headers().contains_key("etag"));
        std::fs::write(&path, &bytes).unwrap();
        assert_unchanged(get(&f, 2028, &f.key, &[&tag]).await, &tag).await;
        f.vault.lock().unwrap().revoke(f.credential).unwrap();
        assert_eq!(get(&f, 2028, &f.key, &[&tag]).await.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unconditional_reopen_checks_current_file_and_grant() {
        let f = Fixture::new();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let (path, bytes) = shared_source(&f);
        let request = || {
            let req = Request::builder()
                .uri("/v1/state?year=2028")
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header("x-godovoy-view", "overview-v1")
                .header("x-godovoy-rows", "tuple-v2")
                .body(Body::empty()).unwrap();
            f.app.clone().oneshot(req)
        };
        let expected = snapshot(request().await.unwrap()).await;
        assert_eq!(snapshot(request().await.unwrap()).await, expected);

        let modified = path.metadata().unwrap().modified().unwrap();
        let mut corrupt = bytes.clone();
        *corrupt.last_mut().unwrap() = b'!';
        std::fs::write(&path, corrupt).unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap()
            .set_modified(modified).unwrap();
        let invalid = request().await.unwrap();
        assert_eq!(invalid.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!invalid.headers().contains_key(header::ETAG));

        std::fs::write(path, bytes).unwrap();
        assert_eq!(snapshot(request().await.unwrap()).await, expected);
        f.vault.lock().unwrap().revoke(f.credential).unwrap();
        assert_eq!(request().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn full_reopen_refreshes_shared_rows_after_a_same_size_calendar_edit() {
        let f = Fixture::new();
        let (_, _, reader) = f
            .vault
            .lock()
            .unwrap()
            .grant("Читатель", None, true)
            .unwrap();
        let mut event = f.create();
        event["title"] = json!("First title");
        assert_eq!(f.cmd(event).await, StatusCode::NO_CONTENT);

        let path = f._tmp.path().join("host/online-calendar-v1.json");
        // Put the changed text in a later block of an otherwise valid calendar.
        let mut original = " ".repeat(160 * 1024);
        original.push_str(&std::fs::read_to_string(&path).unwrap());
        std::fs::write(&path, &original).unwrap();
        let modified = path.metadata().unwrap().modified().unwrap();
        let (tag, _) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let (reader_tag, before) = snapshot(get(&f, 2028, &reader, &[]).await).await;
        assert_eq!(before["events"][0]["title"], "First title");
        let changed = original.replace("First title", "Other title");
        assert_eq!(changed.len(), original.len());
        std::fs::write(&path, changed).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(path.metadata().unwrap().modified().unwrap(), modified);

        for (key, previous) in [(&reader, reader_tag), (&f.key, tag)] {
            let (fresh, current) = snapshot(get(&f, 2028, key, &[]).await).await;
            assert_ne!(fresh, previous);
            assert_eq!(current["events"][0]["title"], "Other title");
            assert_unchanged(get(&f, 2028, key, &[&fresh]).await, &fresh).await;
        }
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
    async fn page_delta_requires_the_exact_readers_page_and_a_stable_visible_row() {
        let f = Fixture::new();
        let (grant, person, reader) = f.vault.lock().unwrap().grant("Обзор", None, true).unwrap();
        let mut event = f.create();
        event["yearly"] = json!(false);
        assert_eq!(f.cmd(event).await, StatusCode::NO_CONTENT);
        {
            let lock = f.vault.lock().unwrap();
            let mut data = lock.data().unwrap();
            let plan = data.calendar.plans.values().next().unwrap().clone();
            data.calendar.plans.clear();
            for id in 1..=165 { data.calendar.plans.insert(Id::from_u128(id), plan.clone()); }
            lock.save_data(&data).unwrap();
        }
        let page = |key: &str, year: i32, size: &str, delta: &str, base: Option<&str>| {
            let mut req = Request::builder().uri(format!("/v1/state?year={year}"))
                .header(header::HOST, "calendar.test").header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header("x-godovoy-view", "overview-v1").header("x-godovoy-rows", "tuple-v2")
                .header("x-godovoy-page", size).header("x-godovoy-delta", delta);
            if let Some(base) = base { req = req.header(header::IF_NONE_MATCH, base); }
            f.app.clone().oneshot(req.body(Body::empty()).unwrap())
        };
        let first = "first-v1";
        let opt = "page-event-v1";
        let (writer, full) = snapshot(get(&f, 2028, &f.key, &[]).await).await;
        let (base, before) = snapshot(page(&reader, 2028, first, opt, None).await.unwrap()).await;
        let change = |index: usize, done| {
            let row = &full["events"][index];
            json!({"op":"check", "eventId":row["id"], "year":2028, "taskId":row["checklist"][0]["id"], "done":done})
        };
        let ack = save_with_tag(&f, &f.key, &writer, &change(79, true)).await;
        let writer = ack.headers()["x-godovoy-state-tag"].to_str().unwrap().to_owned();
        let response = page(&reader, 2028, first, opt, Some(&base)).await.unwrap();
        assert_eq!(response.headers()["x-godovoy-delta-base"], base);
        assert!(response.headers()[header::CACHE_CONTROL].to_str().unwrap().contains("no-store"));
        let next = response.headers()["x-godovoy-page-tag"].to_str().unwrap().to_owned();
        let snapshot_token = response.headers()["x-godovoy-state-tag"].clone();
        let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        let patch: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(patch["events"][0]["index"], 79);
        assert_eq!(patch["events"][0]["event"]["id"], before["events"][79][0]);
        assert_eq!(patch["events"][0]["event"]["completed"], 1);
        assert!(patch["detail"].is_null());
        let verified = page(&reader, 2028, first, opt, None).await.unwrap();
        assert_eq!(verified.headers()[header::ETAG], next);
        assert_eq!(verified.headers()["x-godovoy-state-tag"], snapshot_token);
        let (_, current) = snapshot(verified).await;
        assert_eq!(current["events"][79][6], 1);
        assert_eq!(current["events"][79][0], before["events"][79][0]);
        for case in [
            page(&reader, 2028, first, "event-v1", Some(&base)),
            page(&reader, 2028, "prefix-160-v1", opt, Some(&base)),
            page(&f.key, 2028, first, opt, Some(&base)),
            page(&reader, 2029, first, opt, Some(&base)),
        ] {
            let response = case.await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key("x-godovoy-delta-base"));
        }
        assert_eq!(save_with_tag(&f, &f.key, &writer, &change(80, true)).await.status(), StatusCode::OK);
        for stale in [&base, &next] {
            let r = page(&reader, 2028, first, opt, Some(stale)).await.unwrap();
            assert!(!r.headers().contains_key("x-godovoy-delta-base"));
            let (_, value) = snapshot(r).await;
            assert_eq!(value["events"], current["events"]);
            assert!(!value.to_string().contains(full["events"][80]["id"].as_str().unwrap()));
        }
        f.vault.lock().unwrap().set_employee_scope(person, true).unwrap();
        let scoped = page(&reader, 2028, first, opt, Some(&base)).await.unwrap();
        assert!(!scoped.headers().contains_key("x-godovoy-delta-base"));
        let (_, value) = snapshot(scoped).await;
        assert_eq!(value["assignedOnly"], true);
        assert_eq!(value["events"], json!([]));
        f.vault.lock().unwrap().revoke(grant).unwrap();
        assert_eq!(page(&reader, 2028, first, opt, Some(&base)).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn first_page_has_its_own_http_validator_and_a_complete_snapshot_token() {
        let f = Fixture::new();
        let today = chrono::Local::now().date_naive();
        let year = chrono::Datelike::year(&today);
        let mut create = f.create();
        create["date"] = json!(today.to_string());
        assert_eq!(f.cmd(create).await, StatusCode::NO_CONTENT);
        {
            let lock = f.vault.lock().unwrap();
            let mut data = lock.data().unwrap();
            let plan = data.calendar.plans.values().next().unwrap().clone();
            for _ in 1..197 {
                data.calendar.plans.insert(Id::new_v4(), plan.clone());
            }
            lock.save_data(&data).unwrap();
        }
        fill_calendar_audit(&f);
        let state = |page: Option<&str>, condition: Option<&str>| {
            let mut request = Request::builder()
                .uri(format!("/v1/state?year={year}"))
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header("x-godovoy-view", "overview-v1")
                .header("x-godovoy-rows", "tuple-v2");
            if let Some(page) = page {
                request = request.header("x-godovoy-page", page);
            }
            if let Some(tag) = condition {
                request = request.header(header::IF_NONE_MATCH, tag);
            }
            f.app.clone().oneshot(request.body(Body::empty()).unwrap())
        };
        let response = state(Some("first-v1"), None).await.unwrap();
        assert!(response.headers()[header::CACHE_CONTROL].to_str().unwrap().contains("no-store"));
        let snapshot_tag = response.headers()["x-godovoy-state-tag"].to_str().unwrap().to_owned();
        let (first, partial) = snapshot(response).await;
        let (all, complete) = snapshot(state(None, Some(&first)).await.unwrap()).await;
        assert_eq!(all, snapshot_tag);
        assert_ne!(first, all);
        assert_eq!(partial["eventCount"], 197);
        assert_eq!(partial["events"].as_array().unwrap(), &complete["events"].as_array().unwrap()[..80]);
        assert_eq!(partial["tasks"], complete["tasks"]);
        assert_eq!(partial["tasks"].as_array().unwrap().len(), 197);
        assert_unchanged(state(Some("first-v1"), Some(&first)).await.unwrap(), &first).await;
        assert_eq!(state(Some("first-v1"), Some(&all)).await.unwrap().status(), StatusCode::OK);
        let (second, extended) = snapshot(state(Some("prefix-160-v1"), Some(&first)).await.unwrap()).await;
        assert_ne!(second, first);
        assert_ne!(second, all);
        assert_eq!(extended["events"].as_array().unwrap(), &complete["events"].as_array().unwrap()[..160]);
        assert_eq!(extended["eventCount"], 197);
        assert_eq!(extended["tasks"], complete["tasks"]);
        assert_unchanged(state(Some("prefix-160-v1"), Some(&second)).await.unwrap(), &second).await;
        assert_eq!(state(None, Some(&second)).await.unwrap().status(), StatusCode::OK);
        assert_eq!(state(Some("first-v1"), Some(&second)).await.unwrap().status(), StatusCode::OK);
        let continuation = |previous: &str, prior_offer: &str, next_offer: &str| {
            let request = Request::builder()
                .uri(format!("/v1/state?year={year}"))
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header("x-godovoy-view", "overview-v1")
                .header("x-godovoy-rows", "tuple-v2")
                .header("x-godovoy-page", next_offer)
                .header("x-godovoy-continue", prior_offer)
                .header("x-godovoy-base", previous);
            f.app.clone().oneshot(request.body(Body::empty()).unwrap())
        };
        let part = continuation(&first, "first-v1", "prefix-160-v1").await.unwrap();
        assert_eq!(part.headers()["x-godovoy-state-tag"], all);
        assert_eq!(part.headers()["x-godovoy-continued"], first);
        assert_eq!(part.headers()["x-godovoy-page-tag"], second);
        let (part_tag, chunk) = snapshot(part).await;
        assert_ne!(part_tag, second);
        assert_ne!(part_tag, first);
        assert_ne!(part_tag, all);
        assert_eq!(chunk.as_object().unwrap().len(), 4);
        assert!(chunk.get("tasks").is_none());
        assert_eq!(chunk["offset"], 80);
        assert_eq!(chunk["eventCount"], 197);
        assert_eq!(chunk["events"].as_array().unwrap(), &complete["events"].as_array().unwrap()[80..160]);
        let last = continuation(&second, "prefix-160-v1", "all").await.unwrap();
        assert_eq!(last.headers()["x-godovoy-state-tag"], all);
        assert!(last.headers().get("x-godovoy-page-tag").is_none());
        let (_, last) = snapshot(last).await;
        assert_eq!(last["offset"], 160);
        assert_eq!(last["events"].as_array().unwrap(), &complete["events"].as_array().unwrap()[160..]);
        let wrong = continuation(&second, "first-v1", "prefix-160-v1").await.unwrap();
        assert!(wrong.headers().get("x-godovoy-continued").is_none());
        let (_, replacement) = snapshot(wrong).await;
        assert_eq!(replacement, extended);
        let beyond = &complete["events"][80][0];
        let id = beyond.as_str().unwrap();
        let detail = |token: &str| {
            let request = Request::builder()
                .uri(format!("/v1/event?year={year}&eventYear={year}&id={id}"))
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header("x-godovoy-rows", "tuple-v2")
                .header(header::IF_MATCH, token)
                .body(Body::empty())
                .unwrap();
            f.app.clone().oneshot(request)
        };
        assert_eq!(detail(&first).await.unwrap().status(), StatusCode::PRECONDITION_FAILED);
        assert_eq!(detail(&all).await.unwrap().status(), StatusCode::OK);
        let command = Request::builder()
            .method(Method::POST)
            .uri("/v1/command")
            .header(header::HOST, "calendar.test")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-godovoy-view", "overview-v1")
            .header("x-godovoy-rows", "tuple-v2")
            .header("x-godovoy-page", "first-v1")
            .header("x-godovoy-base", &all)
            .header("x-godovoy-year", year.to_string())
            .body(Body::from(json!({"op":"note","eventId":id,"year":year,"base":"","text":"Готово"}).to_string()))
            .unwrap();
        let response = f.app.clone().oneshot(command).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let new_first = response.headers()["x-godovoy-page-tag"].to_str().unwrap().to_owned();
        let new_snapshot = response.headers()["x-godovoy-state-tag"].to_str().unwrap().to_owned();
        assert_ne!(new_first, first);
        assert_ne!(new_snapshot, all);
        let receipt: Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(receipt["events"], json!([]));
        assert_eq!(receipt["tasks"][0]["event"]["id"], id);
        assert_eq!(receipt["detail"]["note"], "Готово");
        let changed = continuation(&first, "first-v1", "prefix-160-v1").await.unwrap();
        assert!(changed.headers().get("x-godovoy-continued").is_none());
        assert_eq!(changed.headers()["x-godovoy-state-tag"], new_snapshot);
        let (_, changed) = snapshot(changed).await;
        assert_eq!(changed["events"].as_array().unwrap().len(), 160);
        assert_eq!(changed["tasks"].as_array().unwrap().len(), 197);
        assert_unchanged(state(Some("first-v1"), Some(&new_first)).await.unwrap(), &new_first).await;
        assert_unchanged(state(None, Some(&new_snapshot)).await.unwrap(), &new_snapshot).await;

        let quick = json!({"op":"check","eventId":id,"year":year,"done":true,"onlyIfAssigned":true,
            "taskId":receipt["detail"]["checklist"][0]["id"]});
        let command = Request::builder().method(Method::POST).uri("/v1/command")
            .header(header::HOST, "calendar.test")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-godovoy-view", "overview-v1")
            .header("x-godovoy-rows", "tuple-v2")
            .header("x-godovoy-page", "prefix-160-v1")
            .header("x-godovoy-receipt", "rows-only-v1")
            .header("x-godovoy-base", &new_snapshot)
            .header("x-godovoy-year", year.to_string())
            .body(Body::from(quick.to_string())).unwrap();
        let response = f.app.clone().oneshot(command).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let page_tag = response.headers()["x-godovoy-page-tag"].to_str().unwrap().to_owned();
        let answer: Value = serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert!(answer.get("detail").is_none());
        assert_eq!(answer["events"][0]["index"], 80);
        assert_eq!(answer["events"][0]["event"]["completed"], 1);
        assert_unchanged(state(Some("prefix-160-v1"), Some(&page_tag)).await.unwrap(), &page_tag).await;
    }

    #[tokio::test]
    async fn receipt_audit_failure_preserves_the_saved_note_without_disclosing_it() {
        let f = Fixture::new();
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let host = Host::new(f.vault.clone(), "https://calendar.test", Some(EntryPoint::Office)).unwrap();
        let initial = Request::builder()
            .uri("/v1/state?year=2028")
            .header(header::HOST, "calendar.test")
            .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
            .header("x-godovoy-view", "overview-v1")
            .body(Body::empty())
            .unwrap();
        let (tag, overview) = snapshot(router(host.clone()).oneshot(initial).await.unwrap()).await;
        let event: Id = serde_json::from_value(overview["events"][0]["id"].clone()).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, format!("Bearer {}", f.key).parse().unwrap());
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert("x-godovoy-view", HeaderValue::from_static("overview-v1"));
        headers.insert("x-godovoy-base", tag.parse().unwrap());
        headers.insert("x-godovoy-year", HeaderValue::from_static("2028"));
        let note = "Подтверждено перед сбоем квитанции";
        let body = Bytes::from(json!({
            "op":"note", "eventId":event, "year":2028, "base":"", "text":note
        }).to_string());
        let (audit_called, observed_audit) = tokio::sync::oneshot::channel();
        let reply = command_with_receipt_audit(host, headers, body, move |lock, _, _| {
            let saved = lock.data().unwrap();
            assert_eq!(saved.calendar.follow_event(event, 2028).unwrap().note(), note);
            audit_called.send(()).unwrap();
            Err(std::io::Error::other("receipt audit failed").into())
        }).await;

        assert_eq!(reply.status(), StatusCode::NO_CONTENT);
        observed_audit.await.expect("the receipt audit must have run");
        assert!(!reply.headers().contains_key("x-godovoy-state-tag"));
        assert!(to_bytes(reply.into_body(), 65536).await.unwrap().is_empty());
        assert_eq!(f.state().await["events"][0]["note"], note);
    }

    #[tokio::test]
    async fn overview_write_receipt_matches_cold_projection() {
        let mut f = Fixture::new();
        let host = Host::new(
            f.vault.clone(),
            "https://calendar.test",
            Some(EntryPoint::Office),
        )
        .unwrap();
        let views = host.views.clone();
        f.app = router(host);
        assert_eq!(f.cmd(f.create()).await, StatusCode::NO_CONTENT);
        let (event, task) = {
            let lock = f.vault.lock().unwrap();
            let mut data = lock.data().unwrap();
            let event = data.calendar.occurrences(2028).next().unwrap();
            let (id, task) = (event.id, event.event.tasks[0].id);
            let plan = data.calendar.plans[&id].clone();
            for _ in 0..96 {
                data.calendar.plans.insert(Id::new_v4(), plan.clone());
            }
            lock.save_data(&data).unwrap();
            (id, task)
        };
        fill_calendar_audit(&f);
        let authorized = |uri: &str| {
            Request::builder()
                .uri(uri)
                .header(header::HOST, "calendar.test")
                .header(header::AUTHORIZATION, format!("Bearer {}", f.key))
                .header("x-godovoy-view", "overview-v1")
                .header("x-godovoy-rows", "tuple-v2")
        };
        let uri = "/v1/state?year=2028";
        let initial = authorized(uri).body(Body::empty()).unwrap();
        let (mut tag, overview) = snapshot(f.app.clone().oneshot(initial).await.unwrap()).await;
        assert_eq!(overview["rowFormat"], "tuple-v2");
        for command in [
            json!({"op":"note","eventId":event,"year":2028,"base":"","text":"Уточнено"}),
            json!({"op":"check","eventId":event,"year":2028,"taskId":task,"done":true}),
        ] {
            let request = authorized("/v1/command")
                .method(Method::POST)
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-godovoy-year", "2028")
                .header("x-godovoy-base", &tag)
                .body(Body::from(command.to_string()))
                .unwrap();
            let reply = f.app.clone().oneshot(request).await.unwrap();
            assert_eq!(reply.status(), StatusCode::OK);
            let updated = reply.headers()["x-godovoy-state-tag"].to_str().unwrap();
            assert_ne!(updated, tag);
            let lock = f.vault.lock().unwrap();
            let access = lock.access().unwrap();
            let data = lock.data().unwrap();
            let today = chrono::Local::now().date_naive();
            let view = actions::ViewContext::new(&access, 2028, f.employee, true)
                .offer_overview(actions::OverviewFormat::TupleRows)
                .render(&data, today);
            assert_eq!(views.view_tag(f.credential, &view, today).unwrap(), updated);
            tag = updated.to_owned();
        }
        let confirm = authorized(uri)
            .header(header::IF_NONE_MATCH, &tag)
            .body(Body::empty())
            .unwrap();
        assert_unchanged(f.app.clone().oneshot(confirm).await.unwrap(), &tag).await;
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
