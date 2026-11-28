#[path = "support/secure.rs"]
mod secure;
use godovoy_plan::api::{ApiKeyKind, ApiMethod};

use godovoy_plan::{
    api::ApiServer,
    storage::{ApiSettings, Store},
};
use serde_json::{Value, json};
use tempfile::TempDir;

struct Client {
    server: ApiServer,
    _profile: TempDir,
}

impl Client {
    fn new() -> Self {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(&profile.path().join("profile")).unwrap();
        store
            .lock()
            .unwrap()
            .transaction(|state| {
                state.create_workspace("Отдел маркетинга".into(), "Анна".into())?;
                state.api = Some(ApiSettings {
                    enabled: true,
                    port: 0,
                    token: [11; 32],
                    import_token: Some([22; 32]),
                });
                Ok(())
            })
            .unwrap();
        let server = ApiServer::start(store, 0).unwrap();
        Self {
            _profile: profile,
            server,
        }
    }

    fn get(&self, path: &str) -> Result<Value, secure::Error> {
        secure::get(self.server.port(), 11, &format!("/v1/{path}"))
    }
    fn post(&self, event: &Value) -> Result<Value, secure::Error> {
        let response = secure::request(
            self.server.port(),
            ApiKeyKind::Main,
            11,
            ApiMethod::Post,
            "/v1/events",
            Some(event),
        )?;
        assert_eq!(response.status, 201);
        Ok(serde_json::from_slice(&response.body).unwrap())
    }
    fn import(&self, source: &str, key: &str, event: &Value) -> Value {
        let batch = json!({"events": [{"externalId": key, "event": event}]});
        let rows = secure::put(
            self.server.port(),
            22,
            &format!("/v1/imports/{source}"),
            &batch,
        )
        .unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        rows[0].clone()
    }

    fn detail(&self, row: &Value, year: u16) -> Result<Value, secure::Error> {
        self.get(&format!(
            "events/{}?year={year}",
            row["id"].as_str().unwrap()
        ))
    }
}

#[test]
fn calendar_workflows_over_secure_api() {
    let c = Client::new();
    let owner = c.get("members").unwrap()[0]["id"].clone();
    let cases = [
        (
            "поздравления",
            "Partners",
            "2028-01-05",
            40,
            "2027-11-26",
            true,
        ),
        (
            "запуск_витрины",
            "Campaign",
            "2028-03-01",
            1,
            "2028-02-29",
            false,
        ),
        ("обучение", "Team", "2028-02-29", 0, "2028-02-29", true),
        (
            "инвентаризация",
            "Operations",
            "2028-12-31",
            366,
            "2027-12-31",
            false,
        ),
        (
            "встреча_партнёров",
            "Partners",
            "2028-07-01",
            30,
            "2028-06-01",
            false,
        ),
        (
            "летняя_акция",
            "Campaign",
            "2028-06-01",
            14,
            "2028-05-18",
            true,
        ),
    ];
    for (name, category, date, lead, prep_date, yearly) in cases {
        let body = json!({"title": name, "category": category, "date": date, "yearly": yearly,
            "ownerId": owner, "preparationDays": lead,
            "checklist": [{"key": "start", "title": "Начать подготовку", "daysBefore": lead}]});
        let created = c.post(&body).unwrap();
        let loaded = c.detail(&created, 2028).unwrap();
        assert_eq!(loaded, created);
        assert_eq!(loaded["category"], category);
        assert_eq!(loaded["ownerId"], owner);
        assert_eq!(loaded["preparationDate"], prep_date);
        assert_eq!(loaded["checklist"][0]["dueDate"], prep_date);
        assert_eq!(loaded["checklist"][0]["done"], false);
        println!("API-CASE create-read/{name}");

        assert_ne!(c.post(&body).unwrap()["id"], created["id"]);
        println!("API-CASE duplicate-manual-create/{name}");

        let next = c.detail(&created, 2029);
        if yearly {
            let occurrence = next.unwrap();
            let expected = if date == "2028-02-29" {
                "2029-02-28".into()
            } else {
                date.replacen("2028", "2029", 1)
            };
            assert_eq!(occurrence["date"], expected);
            assert_eq!(occurrence["checklist"][0]["done"], false);
        } else {
            assert!(matches!(next, Err(secure::Error::Status(404))));
        }
        println!("API-CASE next-year/{name}");

        let first = c.import("office", name, &body);
        let duplicate = c.import("office", name, &body);
        assert_eq!(first["id"], duplicate["id"]);
        assert_eq!(first["changed"], true);
        assert_eq!(duplicate["changed"], false);
        let stored = c.detail(&first, 2028).unwrap();
        assert_eq!(stored["source"], json!({"name":"office","externalId":name}));
        assert_eq!(stored["ownerId"], owner);
        println!("API-CASE import-retry/{name}");
    }
    assert_eq!(c.get("capacity").unwrap()["plans"], 18);
}

#[test]
fn invalid_form_leaves_saved_calendar_intact() {
    let c = Client::new();
    let valid = json!({"title":"Исходная встреча","date":"2028-04-15"});
    let first = c.post(&valid).unwrap();
    let list_before = c.get("events?year=2028").unwrap();
    let capacity_before = c.get("capacity").unwrap();
    let task =
        |key: &str, title: &str, days: u16| json!({"key":key,"title":title,"daysBefore":days});
    let cases = [
        ("missing_title", json!({"title":null})),
        ("blank_title", json!({"title":"  "})),
        ("long_title", json!({"title":"Я".repeat(161)})),
        ("invalid_leap_day", json!({"date":"2027-02-29"})),
        ("before_supported_year", json!({"date":"1899-12-31"})),
        ("long_description", json!({"description":"я".repeat(4001)})),
        ("unknown_category", json!({"category":"Other"})),
        (
            "unknown_owner",
            json!({"ownerId":"00000000-0000-4000-8000-000000000001"}),
        ),
        ("overlong_horizon", json!({"preparationDays":367})),
        ("negative_horizon", json!({"preparationDays":-1})),
        (
            "task_before_window",
            json!({"preparationDays":1,"checklist":[task("k","Встреча",2)]}),
        ),
        (
            "task_without_key",
            json!({"checklist":[task("","Встреча",0)]}),
        ),
        (
            "long_task_key",
            json!({"checklist":[task(&"k".repeat(81),"Встреча",0)]}),
        ),
        ("task_without_title", json!({"checklist":[task("k"," ",0)]})),
        (
            "long_task_title",
            json!({"checklist":[task("k",&"Я".repeat(161),0)]}),
        ),
        (
            "duplicate_task_keys",
            json!({"checklist":[task("k","Заказать",2),task("k","Получить",1)]}),
        ),
        (
            "too_many_tasks",
            json!({"checklist":(0..101).map(|i| task(&format!("t{i}"),"Проверить",0)).collect::<Vec<_>>()}),
        ),
    ];
    for (label, changes) in cases {
        let mut input = valid.clone();
        input
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        let result = c.post(&input);
        assert!(
            matches!(result, Err(secure::Error::Status(422))),
            "{label}: {result:?}"
        );
        assert_eq!(c.get("events?year=2028").unwrap(), list_before, "{label}");
        assert_eq!(c.get("capacity").unwrap(), capacity_before, "{label}");
        assert_eq!(c.detail(&first, 2028).unwrap(), first);
        println!("API-CASE form-rollback/{label}");
    }
}

#[test]
fn list_pagination_returns_right_ids_in_calendar_order() {
    let c = Client::new();
    let mut expected = Vec::new();
    for (day, yearly) in [(27, false), (12, true), (3, false), (20, true)] {
        let event = c.post(&json!({"title":format!("Работы {day}"),"date":format!("2028-06-{day:02}"),"yearly":yearly})).unwrap();
        expected.push((day, event["id"].clone()));
    }
    expected.sort_by_key(|x| x.0);
    let cases: &[(&str, &str, &[usize])] = &[
        ("baseline", "year=2028", &[0, 1, 2, 3]),
        ("first_item", "year=2028&limit=1", &[0]),
        ("first_page", "year=2028&limit=2", &[0, 1]),
        ("second_page", "year=2028&limit=2&offset=2", &[2, 3]),
        ("overlapping_page", "year=2028&limit=2&offset=1", &[1, 2]),
        ("short_last_page", "year=2028&limit=3&offset=3", &[3]),
        ("exact_end", "year=2028&offset=4", &[]),
        ("beyond_end", "year=2028&limit=200&offset=50", &[]),
        ("max_page", "year=2028&limit=200", &[0, 1, 2, 3]),
        ("prior_year", "year=2027", &[]),
        ("next_year_recurring_only", "year=2029", &[1, 2]),
        (
            "offset_after_year_filter",
            "year=2029&limit=1&offset=1",
            &[2],
        ),
    ];
    for (label, query, indices) in cases {
        let rows = c.get(&format!("events?{query}")).unwrap();
        let ids: Vec<_> = rows
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].clone())
            .collect();
        let want: Vec<_> = indices.iter().map(|&i| expected[i].1.clone()).collect();
        assert_eq!(ids, want, "{label}");
        println!("API-CASE page/{label}");
    }
}

#[test]
fn tied_dates_have_stable_ids_across_partial_page_boundaries() {
    let client = Client::new();
    let mut expected = Vec::new();
    for (index, date) in [3, 1, 3, 2, 2, 1, 3, 2, 1].into_iter().enumerate() {
        let row = client
            .post(&json!({
                "title": format!("Подготовка №{index}"),
                "date": format!("2028-02-{date:02}"),
                "yearly": index % 2 == 0
            }))
            .unwrap();
        expected.push((date, row["id"].as_str().unwrap().to_owned(), index % 2 == 0));
    }
    expected.sort_unstable();
    for year in [2028, 2029] {
        let ids: Vec<_> = expected
            .iter()
            .filter(|row| year == 2028 || row.2)
            .map(|row| row.1.as_str())
            .collect();
        for offset in [0, 1, 3, 6, ids.len(), usize::MAX] {
            for limit in [1, 3, 200] {
                let rows = client
                    .get(&format!("events?year={year}&offset={offset}&limit={limit}"))
                    .unwrap();
                let actual: Vec<_> = rows
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| row["id"].as_str().unwrap())
                    .collect();
                let start = offset.min(ids.len());
                assert_eq!(actual, ids[start..(start + limit).min(ids.len())]);
            }
        }
    }
}

#[test]
fn source_updates_retain_identity_and_apply_only_supplied_fields() {
    let c = Client::new();
    let owner = c.get("members").unwrap()[0]["id"].clone();
    let base = json!({"title":"Рассылка", "date":"2028-05-30", "description":"Согласовано",
        "yearly":true, "ownerId":owner, "category":"Partners", "preparationDays":20,
        "checklist":[{"key":"letters","title":"Письма","daysBefore":5}]});
    let cases = [
        ("rename", json!({"title":"Партнёрская рассылка"}), true),
        ("clear_description", json!({"description":""}), true),
        (
            "rewrite_description",
            json!({"description":"Согласовать заново"}),
            true,
        ),
        ("clear_tasks", json!({"checklist":[]}), true),
        (
            "rename_task",
            json!({"checklist":[{"key":"letters","title":"Новые письма","daysBefore":5}]}),
            true,
        ),
        (
            "move_task_deadline",
            json!({"checklist":[{"key":"letters","title":"Письма","daysBefore":3}]}),
            true,
        ),
        ("switch_category", json!({"category":"Team"}), true),
        ("reschedule_day", json!({"date":"2028-05-29"}), true),
        ("change_preparation", json!({"preparationDays":21}), true),
        ("end_annual_recurrence", json!({"yearly":false}), true),
        ("preserve_omitted_fields", json!({}), false),
    ];
    for (label, changes, changed) in cases {
        let initial = c.import("crm", label, &base);
        let initial_view = c.detail(&initial, 2028).unwrap();
        let mut patch = json!({"title":"Рассылка", "date":"2028-05-30"});
        patch
            .as_object_mut()
            .unwrap()
            .extend(changes.as_object().unwrap().clone());
        let update = c.import("crm", label, &patch);
        assert_eq!(update["id"], initial["id"], "{label}");
        assert_eq!(update["changed"], changed, "{label}");
        let after = c.detail(&update, 2028).unwrap();
        for field in [
            "title",
            "description",
            "date",
            "ownerId",
            "category",
            "yearly",
            "preparationDays",
        ] {
            let want = changes.get(field).unwrap_or(&base[field]);
            assert_eq!(&after[field], want, "{label}: {field}");
        }
        if let Some(tasks) = changes.get("checklist") {
            assert_eq!(
                after["checklist"].as_array().unwrap().len(),
                tasks.as_array().unwrap().len()
            );
            if !tasks.as_array().unwrap().is_empty() {
                assert_eq!(
                    after["checklist"][0]["id"],
                    initial_view["checklist"][0]["id"]
                );
                assert_eq!(after["checklist"][0]["title"], tasks[0]["title"]);
                assert_eq!(after["checklist"][0]["daysBefore"], tasks[0]["daysBefore"]);
            }
        } else if changes.get("date").is_none() {
            assert_eq!(after["checklist"], initial_view["checklist"], "{label}");
        } else {
            assert_eq!(after["checklist"][0]["dueDate"], "2028-05-24");
        }
        assert_eq!(c.import("crm", label, &patch)["changed"], false, "{label}");
        println!("API-CASE upstream-update/{label}");
    }
}
