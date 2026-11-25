use std::path::Path;

use godovoy_plan::Fallible;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Off,
    Enabled,
    NeedsUpdate,
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{
        fs,
        io::{self, Write},
        path::{Path, PathBuf},
    };

    use super::{Fallible, Status};

    const LABEL: &str = godovoy_plan::APP_ID;

    fn file() -> Fallible<PathBuf> {
        Ok(dirs::home_dir()
            .ok_or("Домашний каталог недоступен.")?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    fn plist(executable: &Path) -> Fallible<String> {
        let path = executable
            .to_str()
            .ok_or("Путь к приложению должен быть в UTF-8.")?;
        let mut escaped = String::with_capacity(path.len());
        for c in path.chars() {
            match c {
                '&' => escaped.push_str("&amp;"),
                '<' => escaped.push_str("&lt;"),
                '>' => escaped.push_str("&gt;"),
                c if c.is_control() => return Err("Путь содержит управляющий символ.".into()),
                c => escaped.push(c),
            }
        }
        Ok(format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\n\
             <key>Label</key><string>{LABEL}</string>\n\
             <key>ProgramArguments</key><array><string>{escaped}</string><string>--at-login</string></array>\n\
             <key>RunAtLoad</key><true/>\n</dict></plist>\n"
        ))
    }

    pub fn status(executable: &Path) -> Fallible<Status> {
        read_status(&file()?, executable)
    }

    fn read_status(file: &Path, executable: &Path) -> Fallible<Status> {
        match fs::File::open(file) {
            Ok(input) => {
                use std::io::Read;
                let mut contents = Vec::new();
                input.take(16_384).read_to_end(&mut contents)?;
                Ok(if contents == plist(executable)?.as_bytes() {
                    Status::Enabled
                } else {
                    Status::NeedsUpdate
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Status::Off),
            Err(error) => Err(error.into()),
        }
    }

    pub fn set(executable: &Path, enabled: bool) -> Fallible<()> {
        write_status(file()?, executable, enabled)
    }

    fn write_status(file: PathBuf, executable: &Path, enabled: bool) -> Fallible<()> {
        if enabled {
            let dir = file.parent().ok_or("Папка LaunchAgents недоступна.")?;
            fs::create_dir_all(dir)?;
            let mut staged = tempfile::NamedTempFile::new_in(dir)?;
            staged.write_all(plist(executable)?.as_bytes())?;
            staged.as_file().sync_all()?;
            staged.persist(file)?;
        } else {
            match fs::remove_file(file) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    #[test]
    fn autostart_entry_can_be_updated_or_removed_after_moving_the_app() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("LaunchAgents/calendar.plist");
        let old_path = Path::new("/Applications/Old Calendar.app/Contents/MacOS/godovoy-plan");
        let new_path = Path::new("/Applications/New Calendar.app/Contents/MacOS/godovoy-plan");
        assert_eq!(read_status(&file, new_path).unwrap(), Status::Off);
        write_status(file.clone(), old_path, true).unwrap();
        assert_eq!(read_status(&file, new_path).unwrap(), Status::NeedsUpdate);
        write_status(file.clone(), new_path, true).unwrap();
        assert_eq!(read_status(&file, new_path).unwrap(), Status::Enabled);
        write_status(file.clone(), new_path, false).unwrap();
        assert_eq!(read_status(&file, new_path).unwrap(), Status::Off);
    }

    #[test]
    fn plist_passes_path_as_one_argument() {
        let config = plist(Path::new("/Applications/A & B/<Calendar>")).unwrap();
        assert!(config.contains("<string>/Applications/A &amp; B/&lt;Calendar&gt;</string>"));
        assert!(config.contains("<string>--at-login</string>"));
        assert!(plist(Path::new("/Applications/a\nb")).is_err());
    }
}

#[cfg(windows)]
mod platform {
    use std::{ffi::OsString, io, path::Path};

    use winreg::{
        RegKey,
        enums::{HKEY_CURRENT_USER, KEY_SET_VALUE},
    };

    use super::{Fallible, Status};

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "GodovoyPlan";

    fn command(executable: &Path) -> OsString {
        let mut value = OsString::from("\"");
        value.push(executable);
        value.push("\" --at-login");
        value
    }

    pub fn status(executable: &Path) -> Fallible<Status> {
        let result = RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(RUN)
            .and_then(|key| key.get_value::<OsString, _>(NAME));
        match result {
            Ok(value) if value == command(executable) => Ok(Status::Enabled),
            Ok(_) => Ok(Status::NeedsUpdate),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Status::Off),
            Err(error) => Err(error.into()),
        }
    }

    pub fn set(executable: &Path, enabled: bool) -> Fallible<()> {
        let hive = RegKey::predef(HKEY_CURRENT_USER);
        if enabled {
            let (key, _) = hive.create_subkey(RUN)?;
            key.set_value(NAME, &command(executable))?;
        } else {
            match hive
                .open_subkey_with_flags(RUN, KEY_SET_VALUE)
                .and_then(|key| key.delete_value(NAME))
            {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

#[cfg(any(target_os = "macos", windows))]
pub use platform::{set, status};

#[cfg(not(any(target_os = "macos", windows)))]
pub fn status(_: &Path) -> Fallible<Status> {
    Err("Автозапуск доступен на Windows и macOS.".into())
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn set(_: &Path, _: bool) -> Fallible<()> {
    Err("Автозапуск доступен на Windows и macOS.".into())
}

pub fn executable(standard_profile: bool) -> Fallible<std::path::PathBuf> {
    if !standard_profile {
        return Err("Автозапуск доступен для основного профиля.".into());
    }
    let executable = std::env::current_exe()?;
    if !Path::new(&executable).is_file() {
        return Err("Исполняемый файл приложения недоступен.".into());
    }
    Ok(executable)
}
