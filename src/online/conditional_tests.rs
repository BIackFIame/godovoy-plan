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

    async fn snapshot(response: Response) -> (String, Value) {
        assert_eq!(response.status(), StatusCode::OK);
        let tag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        (tag, serde_json::from_slice(&bytes).unwrap())
    }

    async fn assert_unchanged(response: Response, tag: &str) {
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], tag);
        assert_eq!(response.headers()[header::VARY], "Authorization");
        assert!(
            response.headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("no-store")
        );
        assert!(to_bytes(response.into_body(), 0).await.unwrap().is_empty());
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
        let (acl, data) = {
            let lock = f.vault.lock().unwrap();
            (lock.access().unwrap(), lock.data_bytes().unwrap())
        };
        let views = conditional::Views::default();
        let render = |today, conditions: &[HeaderValue]| {
            let context = actions::ViewContext::new(&acl, 2029, f.employee, true);
            views
                .response(f.credential, context, today, data.clone(), conditions)
                .unwrap()
        };
        let before = NaiveDate::from_ymd_opt(2027, 12, 31).unwrap();
        let (old_tag, old) = snapshot(render(before, &[])).await;
        assert_eq!(old["tasks"], json!([]));
        let condition = [HeaderValue::from_str(&old_tag).unwrap()];
        let (new_tag, new) = snapshot(render(before.succ_opt().unwrap(), &condition)).await;
        assert_ne!(new_tag, old_tag);
        assert_eq!(new["events"], json!([]));
        assert_eq!(new["tasks"].as_array().unwrap().len(), 1);
    }
}
