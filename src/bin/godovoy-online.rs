#![forbid(unsafe_code)]
#[path = "../online/actions.rs"]
mod actions;
#[path = "../online/host.rs"]
mod host;

use godovoy_plan::{
    Fallible,
    model::{Calendar, Id},
    storage::{Store, online::OnlineVault},
};
use std::{env, path::Path};

const HELP: &str = "godovoy-online init DIR NAME\ngodovoy-online init DIR --from-profile OLD_DIR\ngodovoy-online grant DIR NAME [--employee-id UUID] [--read-only]\ngodovoy-online revoke DIR CREDENTIAL_ID\ngodovoy-online revoke-all DIR\ngodovoy-online status DIR\ngodovoy-online audit DIR\ngodovoy-online serve DIR [--port 45838] [--origin https://calendar.example]";
fn main() {
    if let Err(e) = run() {
        eprintln!("Ошибка: {e}");
        std::process::exit(1);
    }
}
fn run() -> Fallible<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args == ["--version"] {
        println!("godovoy-online {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.is_empty() || args == ["--help"] || args == ["-h"] {
        println!("{HELP}");
        return Ok(());
    }
    let [op, dir, tail @ ..] = args.as_slice() else {
        return Err(HELP.into());
    };
    let dir = Path::new(dir);
    if op == "init" {
        let (name, calendar, employees) = match tail {
            [name] if !name.starts_with("--") => {
                (name.to_owned(), Calendar::default(), Default::default())
            }
            [flag, source] if flag == "--from-profile" => {
                let source = Path::new(source).canonicalize()?;
                let active = Store::active_directory(&source)?;
                let store = Store::open_existing(&active)?;
                let snapshot = store.lock().map_err(|_| "Профиль недоступен.")?.snapshot();
                let w = snapshot
                    .workspace
                    .as_ref()
                    .ok_or("В профиле ещё нет календаря.")?;
                (w.name.clone(), w.calendar.clone(), w.employees())
            }
            _ => return Err(HELP.into()),
        };
        OnlineVault::init(dir, name, calendar, employees)?;
        println!("Центральный календарь создан. Выдайте персональные ключи командой grant.");
        return Ok(());
    }
    let vault = OnlineVault::open(dir)?;
    match (op.as_str(), tail) {
        ("serve", options) => {
            let mut port = None;
            let mut origin = None;
            for flag in options.chunks(2) {
                match flag {
                    [arg, n] if arg == "--port" && port.is_none() => port = Some(n.parse::<u16>()?),
                    [arg, value] if arg == "--origin" && origin.is_none() => {
                        origin = Some(value.to_owned())
                    }
                    _ => return Err(HELP.into()),
                }
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(host::serve(vault, port.unwrap_or(45838), origin))?;
        }
        ("grant", [name, options @ ..]) => {
            let mut employee = None;
            let mut read_only = false;
            let mut iter = options.iter();
            while let Some(option) = iter.next() {
                match option.as_str() {
                    "--employee-id" if employee.is_none() => {
                        employee = Some(iter.next().ok_or(HELP)?.parse::<Id>()?)
                    }
                    "--read-only" if !read_only => read_only = true,
                    _ => return Err(HELP.into()),
                }
            }
            let (id, employee, token) = vault.lock()?.grant(name, employee, read_only)?;
            println!(
                "Сотрудник: {employee}\nID ключа для отзыва: {id}\nКлюч доступа (показывается один раз):\n{token}"
            );
        }
        ("revoke", [key]) => {
            vault.lock()?.revoke(key.parse()?)?;
            println!("Доступ по ключу отозван.");
        }
        ("revoke-all", []) => {
            let count = vault.lock()?.revoke_all()?;
            println!("Отозвано ключей: {count}.");
        }
        ("status", []) => {
            let lock = vault.lock()?;
            let acl = lock.access()?;
            println!("Календарь: {}", acl.name.escape_debug());
            for (id, name) in acl.employees {
                println!("Сотрудник {id}: {}", name.escape_debug());
            }
            for (id, grant) in acl.grants {
                println!(
                    "Ключ {id}, сотрудник {}, {}",
                    grant.employee,
                    if grant.read_only {
                        "чтение"
                    } else {
                        "редактирование"
                    }
                );
            }
        }
        ("audit", []) => {
            let lock = vault.lock()?;
            let mut audit = lock.access()?.audit;
            audit.extend(lock.data()?.audit);
            audit.make_contiguous().sort_by_key(|a| a.at);
            for item in audit {
                println!("{}", serde_json::to_string(&item)?);
            }
        }
        _ => return Err(HELP.into()),
    }
    Ok(())
}
