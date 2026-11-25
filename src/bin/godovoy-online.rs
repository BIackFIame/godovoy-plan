#![forbid(unsafe_code)]
#[path = "../online/actions.rs"]
mod actions;
#[path = "../online/admin.rs"]
mod admin;
#[path = "../online/host.rs"]
mod host;

use chrono::{DateTime, SecondsFormat, Utc};
use godovoy_plan::{
    Fallible,
    model::{Calendar, Id},
    storage::{
        Store,
        online::{GrantPermissions, GrantPurpose, OnlineVault},
    },
};
use std::{env, path::Path};

const HELP: &str = "godovoy-online configure DIR [--backup-root PRIVATE_DIR]\ngodovoy-online protection-report DIR\ngodovoy-online backup DIR NEW_DEST\ngodovoy-online backup-verify BACKUP\ngodovoy-online backup-restore BACKUP NEW_DIR\ngodovoy-online init DIR NAME\ngodovoy-online init DIR --from-profile OLD_DIR\ngodovoy-online grant DIR NAME [--employee-id UUID] [--read-only] [--allow-remote]\ngodovoy-online grant DIR NAME --employee-id UUID --mcp [--mcp-write] [--allow-remote]\ngodovoy-online revoke DIR CREDENTIAL_ID\ngodovoy-online revoke-all DIR\ngodovoy-online status DIR\ngodovoy-online audit DIR\ngodovoy-online standby-begin DIR --minutes N\ngodovoy-online standby-end DIR\ngodovoy-online serve DIR [--standby] [--port 45838] [--origin https://calendar.example] [--access office|remote]";
fn describe_window(window: godovoy_plan::storage::online::StandbyWindow) {
    let deadline =
        DateTime::<Utc>::from_timestamp(window.deadline, 0).expect("validated standby deadline");
    let state = if window.active_at(Utc::now().timestamp()) {
        "открыто"
    } else {
        "заблокировано"
    };
    println!(
        "Резервное окно {}: {}, до {} (UTC).",
        window.id,
        state,
        deadline.to_rfc3339_opts(SecondsFormat::Secs, true)
    );
}
fn main() {
    if let Err(e) = run() {
        let message = e.to_string();
        if message == HELP {
            eprintln!("Ошибка: {HELP}");
        } else {
            eprintln!("Ошибка: {}", godovoy_plan::terminal_message(&message));
        }
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
    match (op.as_str(), tail) {
        ("backup-verify", []) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&OnlineVault::verify_backup(dir)?)?
            );
            return Ok(());
        }
        ("backup-restore", [dest]) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&OnlineVault::restore_backup(dir, Path::new(dest))?)?
            );
            return Ok(());
        }
        _ => {}
    }
    let vault = OnlineVault::open(dir)?;
    match (op.as_str(), tail) {
        ("configure", options) => {
            let backup_root = match options {
                [] => None,
                [flag, path] if flag == "--backup-root" => {
                    let path = Path::new(path);
                    if !std::fs::symlink_metadata(path)?.is_dir() {
                        return Err("Нужен существующий каталог для резервных копий.".into());
                    }
                    let path = path.canonicalize()?;
                    if path.starts_with(dir.canonicalize()?) {
                        return Err("Выберите отдельный каталог вне рабочего календаря.".into());
                    }
                    Some(path)
                }
                _ => return Err(HELP.into()),
            };
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(admin::serve(vault, backup_root))?;
        }
        ("protection-report", []) => {
            let lock = vault.lock()?;
            lock.audit_security("operator_report_requested", None, None, None)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&lock.protection_report()?)?
            );
        }
        ("backup", [dest]) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&vault.create_verified_backup(Path::new(dest))?)?
            );
        }
        ("serve", options) => {
            let mut port = None;
            let mut origin = None;
            let mut entry = None;
            let mut standby = false;
            let mut iter = options.iter();
            while let Some(flag) = iter.next() {
                match flag.as_str() {
                    "--standby" if !standby => standby = true,
                    "--port" if port.is_none() => {
                        port = Some(iter.next().ok_or(HELP)?.parse::<u16>()?)
                    }
                    "--origin" if origin.is_none() => {
                        origin = Some(iter.next().ok_or(HELP)?.to_owned())
                    }
                    "--access" if entry.is_none() => {
                        entry = Some(match iter.next().ok_or(HELP)?.as_str() {
                            "office" => host::EntryPoint::Office,
                            "remote" => host::EntryPoint::Remote,
                            _ => return Err(HELP.into()),
                        })
                    }
                    _ => return Err(HELP.into()),
                }
            }
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(host::serve(
                    vault,
                    port.unwrap_or(45838),
                    origin,
                    entry,
                    standby,
                ))?;
        }
        ("grant", [name, options @ ..]) => {
            let mut employee = None;
            let mut permissions = GrantPermissions::default();
            let mut mcp = false;
            let mut mcp_write = false;
            let mut iter = options.iter();
            while let Some(option) = iter.next() {
                match option.as_str() {
                    "--employee-id" if employee.is_none() => {
                        employee = Some(iter.next().ok_or(HELP)?.parse::<Id>()?)
                    }
                    "--mcp" if !mcp => mcp = true,
                    "--mcp-write" if !mcp_write => mcp_write = true,
                    "--read-only" if !permissions.read_only => permissions.read_only = true,
                    "--allow-remote" if !permissions.allow_remote => {
                        permissions.allow_remote = true
                    }
                    _ => return Err(HELP.into()),
                }
            }
            let lock = vault.lock()?;
            let (id, employee, token) = if mcp {
                if permissions.read_only {
                    return Err(
                        "Ключ --mcp уже ограничен чтением. Для записи используется --mcp-write."
                            .into(),
                    );
                }
                let employee =
                    employee.ok_or("Для --mcp укажите --employee-id существующего сотрудника.")?;
                permissions.read_only = !mcp_write;
                lock.grant_mcp(name, employee, permissions)?
            } else {
                if mcp_write {
                    return Err("--mcp-write используется вместе с --mcp.".into());
                }
                lock.grant_with_permissions(name, employee, permissions)?
            };
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
        ("standby-begin", [flag, minutes]) if flag == "--minutes" => {
            describe_window(vault.lock()?.begin_standby(minutes.parse()?)?);
        }
        ("standby-end", []) => {
            vault.lock()?.end_standby()?;
            println!("Резервное окно закрыто. Разрешён основной режим.");
        }
        ("status", []) => {
            let lock = vault.lock()?;
            let acl = lock.access()?;
            println!("Календарь: {}", acl.name.escape_debug());
            if let Some(window) = lock.standby_status()? {
                describe_window(window);
            } else {
                println!("Основной режим.");
            }
            println!(
                "Действующих ключей MCP: {}.",
                acl.grants
                    .values()
                    .filter(|grant| grant.purpose == GrantPurpose::Mcp)
                    .count()
            );
            for (id, name) in acl.employees {
                println!("Сотрудник {id}: {}", name.escape_debug());
            }
            for (id, grant) in acl.grants {
                println!(
                    "Ключ {id}, сотрудник {}, {}, {}, {}",
                    grant.employee,
                    match grant.purpose {
                        GrantPurpose::Employee => "календарь",
                        GrantPurpose::Mcp => "MCP (только свои назначения)",
                    },
                    if grant.read_only {
                        "чтение"
                    } else {
                        "редактирование"
                    },
                    if grant.allow_remote {
                        "офис и внешний вход"
                    } else {
                        "только офис"
                    }
                );
                if let Some(parent) = grant.parent {
                    println!("  Основание MCP: {parent}");
                }
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
