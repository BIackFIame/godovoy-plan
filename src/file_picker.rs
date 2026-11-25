use std::path::{Path, PathBuf};

use godovoy_plan::Fallible;

#[cfg(any(windows, target_os = "macos"))]
use godovoy_plan::exchange::EXCHANGE_EXTENSION;

pub fn pick_exchange_to_open() -> Option<PathBuf> {
    #[cfg(any(windows, target_os = "macos"))]
    return rfd::FileDialog::new()
        .set_title("Открыть файл обмена")
        .add_filter("Файл обмена «Годовой план»", &[EXCHANGE_EXTENSION])
        .pick_file();
    #[cfg(not(any(windows, target_os = "macos")))]
    None
}

pub fn pick_exchange_to_save() -> Option<PathBuf> {
    #[cfg(any(windows, target_os = "macos"))]
    return rfd::FileDialog::new()
        .set_title("Сохранить файл обмена")
        .set_file_name(format!(
            "Годовой план {}.{EXCHANGE_EXTENSION}",
            chrono::Local::now().format("%Y-%m-%d %H%M%S")
        ))
        .add_filter("Файл обмена «Годовой план»", &[EXCHANGE_EXTENSION])
        .save_file();
    #[cfg(not(any(windows, target_os = "macos")))]
    None
}

pub fn reveal_exchange_file(path: &Path) -> Fallible<()> {
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};

        let status = Command::new("/usr/bin/open")
            .arg("-R")
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err("Finder не смог показать файл.".into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let parent = path.parent().ok_or("Не удалось определить папку файла.")?;
        let windows = std::env::var_os("SystemRoot").ok_or("Не удалось открыть Проводник.")?;
        std::process::Command::new(PathBuf::from(windows).join("explorer.exe"))
            .arg(parent)
            .spawn()?;
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = path;
        Err("Откройте папку файла в системном проводнике.".into())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::reveal_exchange_file;

    #[test]
    fn reveal_reports_failure_after_the_system_helper_exits() {
        let directory = tempfile::tempdir().unwrap();
        let missing_file = directory.path().join("Файл уже перемещён.yrplan");
        assert_eq!(
            reveal_exchange_file(&missing_file).unwrap_err().to_string(),
            "Finder не смог показать файл."
        );
    }
}
