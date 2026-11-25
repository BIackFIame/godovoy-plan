#![forbid(unsafe_code)]
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app_icon;
mod autostart;
mod chart;
mod file_picker;
mod notifications;
mod startup;
mod theme;
mod ui;

use std::path::PathBuf;

use godovoy_plan::{APP_NAME, Fallible, network::Config};

fn main() -> Fallible<()> {
    if std::env::args_os().skip(1).eq([std::ffi::OsStr::new("--version")]) {
        println!("godovoy-plan {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut directory = None;
    let mut config = Config::default();
    let mut at_login = false;
    let mut standard_profile = true;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str().ok_or("Неизвестная кодировка аргумента.")? {
            "--data-dir" => {
                standard_profile = false;
                directory = Some(PathBuf::from(
                    args.next().ok_or("Укажите папку после --data-dir.")?,
                ))
            }
            "--listen" => {
                standard_profile = false;
                config.listen = args
                    .next()
                    .ok_or("Укажите IPv4:порт после --listen.")?
                    .to_str()
                    .ok_or("Укажите корректный адрес IPv4.")?
                    .parse()?
            }
            "--no-discovery" => {
                standard_profile = false;
                config.discovery_port = None;
            }
            "--at-login" => at_login = true,
            "--help" | "-h" => {
                println!("{APP_NAME}\n\n--data-dir <папка>\n--listen <IPv4:порт>\n--no-discovery");
                return Ok(());
            }
            flag if flag.starts_with("-psn_") => {} // Старые версии macOS могут добавить этот аргумент.
            _ => return Err(format!("Неизвестный аргумент {}", arg.to_string_lossy()).into()),
        }
    }
    let directory = directory
        .or_else(|| dirs::data_local_dir().map(|path| path.join("GodovoyPlan")))
        .ok_or("Не удалось определить папку профиля.")?;
    startup::run(directory, config, at_login, standard_profile)
}
