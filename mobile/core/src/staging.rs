use godovoy_plan::{Fallible, model::Id};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(super) fn directory(initial: &Path) -> Fallible<PathBuf> {
    let p = initial.join("exchange");
    let meta = fs::symlink_metadata(&p)?;
    if !meta.is_dir() {
        return Err("Недоступен приватный каталог обмена.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.mode() & 0o077 != 0 || meta.uid() != fs::symlink_metadata(initial)?.uid() {
            return Err("Недостаточная защита каталога обмена.".into());
        }
    }
    Ok(p)
}

pub(super) fn prepare(initial: &Path) -> Fallible<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(initial.join("exchange")) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    // The native share picker can keep a URI until the next process launch.
    for entry in fs::read_dir(directory(initial)?)? {
        let entry = entry?;
        let path = entry.path();
        if is_exchange(&path) && entry.file_type()?.is_file() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn is_exchange(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "yrplan")
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|s| Id::parse_str(s).is_ok())
}

pub(super) fn input_path(initial: &Path, path: &Path) -> Fallible<PathBuf> {
    let dir = directory(initial)?;
    if !path.is_absolute()
        || !is_exchange(path)
        || path.parent().and_then(|p| p.canonicalize().ok()) != Some(dir.canonicalize()?)
    {
        return Err("Выберите файл .yrplan через приложение.".into());
    }
    let path = dir.join(path.file_name().ok_or("Выберите файл .yrplan.")?);
    let m = fs::symlink_metadata(&path)?;
    if !m.is_file() || m.len() > 9 * 1024 * 1024 {
        return Err("Нужен обычный файл .yrplan размером до 9 МиБ.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.nlink() != 1 || m.uid() != fs::symlink_metadata(dir)?.uid() {
            return Err("Нужна приватная копия файла.".into());
        }
    }
    Ok(path)
}
