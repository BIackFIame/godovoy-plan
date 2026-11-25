//! Acceptance tests use real processes, CLI-issued credentials and a loopback HTTP connection.
#![cfg(feature = "server")]
use godovoy_plan::{
    model::Id,
    storage::online::{OnlineVault, ProtectionSettings},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tempfile::TempDir;

const YEAR: i32 = 2040;
const PROTOCOL: &str = "2025-11-25";

fn cli(dir: &Path, verb: &str, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_godovoy-online"))
        .arg(verb)
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "CLI {verb}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Employee {
    id: String,
    calendar_key: String,
}

fn employee(dir: &Path, name: &str) -> Employee {
    let output = cli(dir, "grant", &[name]);
    Employee {
        id: output
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .last()
            .unwrap()
            .into(),
        calendar_key: output.lines().last().unwrap().into(),
    }
}

fn mcp_key(dir: &Path, name: &str, id: &str, extra: &[&str]) -> String {
    let mut args = vec![name, "--employee-id", id, "--mcp"];
    args.extend_from_slice(extra);
    cli(dir, "grant", &args).lines().last().unwrap().into()
}

struct Server {
    child: Child,
    addr: SocketAddr,
    host: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(dir: &Path, remote: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_godovoy-online"));
        command.arg("serve").arg(dir).args(["--port", "0"]);
        if remote {
            command.args(["--origin", "https://mcp.test", "--access", "remote"]);
        }
        let mut child = command.stderr(Stdio::piped()).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stderr.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr: SocketAddr = line
            .split_whitespace()
            .find_map(|s| s.trim_end_matches('.').parse().ok())
            .expect("server listen address");
        Self {
            child,
            addr,
            host: if remote {
                "mcp.test".into()
            } else {
                addr.to_string()
            },
        }
    }

    fn request(&self, method: &str, path: &str, key: &str, body: Value) -> (u16, Value) {
        self.with_headers(
            method,
            path,
            key,
            body,
            &[("MCP-Protocol-Version", PROTOCOL)],
        )
    }

    fn with_headers(
        &self,
        method: &str,
        path: &str,
        key: &str,
        body: Value,
        headers: &[(&str, &str)],
    ) -> (u16, Value) {
        let timeout = Duration::from_secs(8);
        let mut wire = TcpStream::connect_timeout(&self.addr, timeout).unwrap();
        wire.set_read_timeout(Some(timeout)).unwrap();
        wire.set_write_timeout(Some(timeout)).unwrap();
        let data = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        };
        write!(wire, "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {key}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n", self.host, data.len()).unwrap();
        for (name, value) in headers {
            write!(wire, "{name}: {value}\r\n").unwrap();
        }
        write!(wire, "\r\n").unwrap();
        wire.write_all(&data).unwrap();
        let mut bytes = String::new();
        wire.read_to_string(&mut bytes).unwrap();
        let (head, payload) = bytes.split_once("\r\n\r\n").unwrap();
        let code = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let json = if payload.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(payload).unwrap()
        };
        (code, json)
    }
    fn rpc(&self, key: &str, method: &str, params: Value) -> (u16, Value) {
        self.request(
            "POST",
            "/mcp",
            key,
            json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}),
        )
    }
    fn call(&self, key: &str, name: &str, arguments: Value) -> (u16, Value) {
        self.rpc(
            key,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        )
    }
    fn change(&self, key: &str, body: Value) {
        let response = self.request("POST", "/v1/command", key, body);
        assert_eq!(response, (204, Value::Null));
    }
}

fn success((status, reply): (u16, Value)) -> Value {
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["jsonrpc"], "2.0");
    assert_eq!(reply["id"], 7);
    assert!(reply.get("error").is_none(), "{reply}");
    reply["result"].clone()
}

fn denied((status, reply): (u16, Value)) {
    if status == 200 {
        assert!(
            reply["result"]["isError"] == true || reply["error"]["code"].is_number(),
            "{reply}"
        );
    } else {
        assert!(matches!(status, 401 | 403), "{status} {reply}");
    }
}

fn event_command(title: &str, employee: &str) -> Value {
    json!({"op":"createEvent","title":title,"date":"2040-04-19","description":"Приватное описание Анны",
        "category":"Team","yearly":true,"ownerId":employee,"preparationDays":5,
        "checklist":[{"title":"Согласовать план","daysBefore":2}]})
}

#[test]
fn employee_keys_are_isolated_even_when_the_regular_calendar_has_team_access() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("calendar");
    cli(&dir, "init", &["Годовой план"]);
    let anna = employee(&dir, "Анна");
    let boris = employee(&dir, "Борис");
    let reader = mcp_key(&dir, "Анна", &anna.id, &[]);
    let writer = mcp_key(&dir, "Анна", &anna.id, &["--mcp-write"]);
    let other = mcp_key(&dir, "Борис", &boris.id, &[]);
    let server = Server::start(&dir, false);
    server.change(&anna.calendar_key, event_command("Задача Анны", &anna.id));
    server.change(
        &anna.calendar_key,
        event_command("Закрытая задача Бориса", &boris.id),
    );
    let state = server
        .request(
            "GET",
            "/v1/state?year=2040",
            &anna.calendar_key,
            Value::Null,
        )
        .1;
    let events = state["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    let own = events.iter().find(|e| e["ownerId"] == anna.id).unwrap();
    let foreign = events.iter().find(|e| e["ownerId"] == boris.id).unwrap();

    let init = success(server.rpc(&reader,"initialize",json!({"protocolVersion":PROTOCOL,"clientInfo":{"name":"qa","version":"1"},"capabilities":{}})));
    assert_eq!(init["protocolVersion"], PROTOCOL);
    assert!(init["capabilities"]["tools"].is_object());
    assert_eq!(
        server.request(
            "POST",
            "/mcp",
            &reader,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        ),
        (202, Value::Null)
    );
    let tools = success(server.rpc(&reader, "tools/list", json!({})));
    let names: Vec<_> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"list_my_events") && names.contains(&"get_my_event"));
    assert!(!names.contains(&"set_my_task_done") && !names.contains(&"set_my_note"));
    let page = success(server.call(&reader, "list_my_events", json!({"year":YEAR})));
    let visible = page["structuredContent"]["events"].as_array().unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0]["id"], own["id"]);
    assert!(!page.to_string().contains("Закрытая задача Бориса"));
    assert!(!page.to_string().contains(boris.id.as_str()));
    assert_eq!(
        success(server.call(&other, "list_my_events", json!({"year":YEAR})))["structuredContent"]["events"]
            [0]["id"],
        foreign["id"]
    );

    let target = |event: &Value| json!({"eventId":event["id"],"year":YEAR});
    let check = |event: &Value, done: bool| json!({"eventId":event["id"],"year":YEAR,"taskId":event["checklist"][0]["id"],"done":done});
    let absent = json!({"eventId":Id::new_v4(),"year":YEAR});
    let foreign_response = server.call(&writer, "get_my_event", target(foreign));
    assert_eq!(
        foreign_response,
        server.call(&writer, "get_my_event", absent)
    );
    denied(foreign_response);
    denied(server.call(&writer, "set_my_task_done", check(foreign, true)));
    denied(server.call(
        &writer,
        "set_my_note",
        json!({"eventId":foreign["id"],"year":YEAR,"base":"","text":"Изменение"}),
    ));
    denied(server.call(&reader, "set_my_task_done", check(own, true)));
    denied(server.call(
        &writer,
        "get_my_event",
        json!({"eventId":foreign["id"],"year":YEAR,"employeeId":boris.id}),
    ));
    denied(server.rpc(&anna.calendar_key, "tools/list", json!({})));
    denied(server.request("GET", "/v1/state?year=2040", &writer, Value::Null));
    denied(server.request(
        "POST",
        "/v1/command",
        &writer,
        event_command("Новая задача", &anna.id),
    ));

    assert_eq!(
        success(server.call(&writer, "set_my_task_done", check(own, true)))["structuredContent"]["saved"],
        true
    );
    let note =
        |base: &str, text: &str| json!({"eventId":own["id"],"year":YEAR,"base":base,"text":text});
    assert_eq!(
        success(server.call(&writer, "set_my_note", note("", "Есть ответ партнёров")))["structuredContent"]
            ["saved"],
        true
    );
    denied(server.call(&writer, "set_my_note", note("", "Устаревшее изменение")));
    let detail = success(server.call(&reader, "get_my_event", target(own)));
    assert_eq!(detail["structuredContent"]["note"], "Есть ответ партнёров");
    assert_eq!(detail["structuredContent"]["checklist"][0]["done"], true);
    let unchanged = success(server.call(&other, "get_my_event", target(foreign)));
    assert_eq!(
        unchanged["structuredContent"]["checklist"][0]["done"],
        false
    );
    assert_eq!(unchanged["structuredContent"]["note"], "");

    let mut reassign = event_command("Задача Анны", &boris.id);
    reassign["op"] = json!("editEvent");
    reassign["eventId"] = own["id"].clone();
    reassign["revision"] = own["revision"].clone();
    reassign["year"] = json!(YEAR);
    reassign["scope"] = json!("year");
    reassign["checklist"][0]["id"] = own["checklist"][0]["id"].clone();
    server.change(&anna.calendar_key, reassign);
    denied(server.call(&writer, "get_my_event", target(own)));
    denied(server.call(&writer, "set_my_task_done", check(own, false)));
    let next_year = json!({"eventId":own["id"],"year":YEAR+1});
    assert_eq!(
        success(server.call(&reader, "get_my_event", next_year))["structuredContent"]["id"],
        own["id"]
    );
    assert_eq!(
        success(server.call(&other, "get_my_event", target(own)))["structuredContent"]["note"],
        "Есть ответ партнёров"
    );

    cli(&dir, "revoke", &[writer.split_once('.').unwrap().0]);
    assert_eq!(server.rpc(&writer, "ping", json!({})).0, 401);
    assert_eq!(server.rpc(&reader, "ping", json!({})).0, 200);
    cli(
        &dir,
        "revoke",
        &[anna.calendar_key.split_once('.').unwrap().0],
    );
    assert_eq!(server.rpc(&reader, "ping", json!({})).0, 401);
    cli(&dir, "grant", &["Анна", "--employee-id", &anna.id]);
    assert_eq!(server.rpc(&reader, "ping", json!({})).0, 401);
    assert_eq!(server.rpc(&other, "ping", json!({})).0, 200);
}

#[test]
fn policy_and_remote_entry_are_checked_on_each_tool_request() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("calendar");
    cli(&dir, "init", &["Годовой план"]);
    let anna = employee(&dir, "Анна");
    cli(
        &dir,
        "grant",
        &["Анна", "--employee-id", &anna.id, "--allow-remote"],
    );
    let office = mcp_key(&dir, "Анна", &anna.id, &["--mcp-write"]);
    let external = mcp_key(&dir, "Анна", &anna.id, &["--allow-remote", "--mcp-write"]);
    let local = Server::start(&dir, false);
    let remote = Server::start(&dir, true);
    assert_eq!(remote.rpc(&office, "tools/list", json!({})).0, 403);
    assert_eq!(remote.rpc(&external, "tools/list", json!({})).0, 200);
    local.change(&anna.calendar_key, event_command("Событие", &anna.id));
    let id = success(local.call(&office,"list_my_events",json!({"year":YEAR})))["structuredContent"]["events"][0]["id"].clone();
    let note = json!({"year":YEAR,"eventId":id,"base":"","text":"Не передавать наружу"});
    assert_eq!(
        success(local.call(&office, "set_my_note", note.clone()))["structuredContent"]["saved"],
        true
    );
    let vault = OnlineVault::open(&dir).unwrap();
    vault
        .lock()
        .unwrap()
        .set_protection(
            0,
            ProtectionSettings {
                russia: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(remote.rpc(&external, "tools/list", json!({})).0, 403);
    let detail = success(local.call(&office, "get_my_event", json!({"year":YEAR,"eventId":id})));
    assert!(!detail.to_string().contains("Приватное описание Анны"));
    assert!(!detail.to_string().contains("Не передавать наружу"));
    let tools = success(local.rpc(&office, "tools/list", json!({})));
    assert!(
        !tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "set_my_note")
    );
    denied(local.call(&office, "set_my_note", note));
    vault
        .lock()
        .unwrap()
        .revoke_employee_keys(anna.id.parse().unwrap())
        .unwrap();
    assert_eq!(local.rpc(&office, "ping", json!({})).0, 401);
    assert_eq!(local.rpc(&external, "ping", json!({})).0, 401);
}

#[test]
fn modern_mcp_validates_the_envelope_and_keeps_the_same_employee_scope() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("calendar");
    cli(&dir, "init", &["Совместный план"]);
    let anna = employee(&dir, "Анна");
    let boris = employee(&dir, "Борис");
    let key = mcp_key(&dir, "Анна", &anna.id, &["--mcp-write"]);
    let server = Server::start(&dir, false);
    server.change(
        &anna.calendar_key,
        event_command("Только для Бориса", &boris.id),
    );
    let foreign = server
        .request(
            "GET",
            "/v1/state?year=2040",
            &boris.calendar_key,
            Value::Null,
        )
        .1["events"][0]
        .clone();
    let version = "2026-07-28";
    let request =
        |rpc_method: &str, meta_method: &str, name: &str, meta_name: &str, arguments: Value| {
            let mut headers = vec![
                ("MCP-Protocol-Version", version),
                ("Mcp-Method", meta_method),
            ];
            let mut params = json!({"_meta":{
                "io.modelcontextprotocol/protocolVersion":version,
                "io.modelcontextprotocol/clientInfo":{"name":"calendar-qa","version":"1"},
                "io.modelcontextprotocol/clientCapabilities":{}
            }});
            if !name.is_empty() {
                params["name"] = json!(name);
                params["arguments"] = arguments;
            }
            if !meta_name.is_empty() {
                headers.push(("Mcp-Name", meta_name));
            }
            server.with_headers(
                "POST",
                "/mcp",
                &key,
                json!({"jsonrpc":"2.0","id":7,"method":rpc_method,"params":params}),
                &headers,
            )
        };
    let discovery = success(request(
        "server/discover",
        "server/discover",
        "",
        "",
        Value::Null,
    ));
    assert!(discovery["capabilities"]["tools"].is_object());
    let list = success(request(
        "tools/call",
        "tools/call",
        "list_my_events",
        "list_my_events",
        json!({"year":YEAR}),
    ));
    assert_eq!(list["structuredContent"]["events"], json!([]));
    for (method, name) in [
        ("tools/list", "get_my_event"),
        ("tools/call", "set_my_task_done"),
        ("tools/call", ""),
    ] {
        let (status, response) = request(
            "tools/call",
            method,
            "get_my_event",
            name,
            json!({"year":YEAR,"eventId":foreign["id"]}),
        );
        assert_eq!(status, 400);
        assert_eq!(response["error"]["code"], -32020);
    }
    let get = |event_id: Value| {
        request(
            "tools/call",
            "tools/call",
            "get_my_event",
            "get_my_event",
            json!({"year":YEAR,"eventId":event_id}),
        )
    };
    let foreign_response = get(foreign["id"].clone());
    assert_eq!(foreign_response, get(json!(Id::new_v4())));
    denied(foreign_response);
    denied(request(
        "tools/call",
        "tools/call",
        "set_my_task_done",
        "set_my_task_done",
        json!({"year":YEAR,"eventId":foreign["id"],"taskId":foreign["checklist"][0]["id"],"done":true}),
    ));
    let state = server
        .request(
            "GET",
            "/v1/state?year=2040",
            &boris.calendar_key,
            Value::Null,
        )
        .1;
    assert_eq!(state["events"][0]["checklist"][0]["done"], false);
}
