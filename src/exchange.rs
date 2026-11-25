use std::{
    fs::{self, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::Path,
};

use blake2::{
    Blake2sMac256,
    digest::{KeyInit, Mac},
};
use chacha20poly1305::{XChaCha20Poly1305, XNonce, aead::AeadInPlace};
use flate2::{Compression, bufread::ZlibDecoder, write::ZlibEncoder};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{
    Fallible,
    model::{Calendar, Id},
    network::merge_calendar,
    storage::{CALENDAR_MESSAGE_LIMIT, LimitedWriter, SharedStore, Workspace, random_bytes},
};

pub const EXCHANGE_EXTENSION: &str = "yrplan";
const MAGIC: &[u8; 8] = b"YRFILE01";
const NONCE_SIZE: usize = 24;
const TAG_SIZE: usize = 16;
const PAYLOAD_LIMIT: usize = CALENDAR_MESSAGE_LIMIT + 64 * 1024;
const FILE_LIMIT: usize = MAGIC.len() + NONCE_SIZE + PAYLOAD_LIMIT + TAG_SIZE;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Packet<C = Calendar> {
    workspace: Id,
    source: Id,
    calendar: C,
}

pub fn export_file(store: &SharedStore, path: &Path) -> Fallible<usize> {
    check_exchange_path(path)?;
    let current = store.lock().unwrap().snapshot();
    let workspace = current
        .workspace
        .as_ref()
        .ok_or("Сначала подключитесь к календарю команды.")?;
    if !workspace.calendar.members.contains_key(&current.device) {
        return Err("Устройство отсутствует в списке участников календаря.".into());
    }
    let packet = Packet {
        workspace: workspace.id,
        source: current.device,
        calendar: &workspace.calendar,
    };
    let mut bytes = Zeroizing::new(Vec::new());
    let mut encoder = ZlibEncoder::new(
        LimitedWriter::new(&mut *bytes, PAYLOAD_LIMIT),
        Compression::fast(),
    );
    {
        let output = BufWriter::new(&mut encoder);
        let mut limited = LimitedWriter::new(output, CALENDAR_MESSAGE_LIMIT);
        serde_json::to_writer(&mut limited, &packet)?;
        limited.flush()?;
    }
    encoder.finish()?;
    let nonce = random_bytes::<NONCE_SIZE>()?;
    let tag = cipher(workspace)
        .encrypt_in_place_detached((&nonce).into(), MAGIC, &mut bytes)
        .map_err(|_| "Не удалось зашифровать файл обмена.")?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let write_packet = |writer: &mut fs::File| -> io::Result<()> {
        writer.write_all(MAGIC)?;
        writer.write_all(&nonce)?;
        writer.write_all(&bytes)?;
        writer.write_all(&tag)?;
        writer.sync_all()
    };
    let mut output = tempfile::Builder::new()
        .prefix(".godovoy-exchange-")
        .tempfile_in(parent)?;
    write_packet(output.as_file_mut())?;
    finish_export(output.persist_noclobber(path), path, write_packet)?;
    Ok(MAGIC.len() + NONCE_SIZE + bytes.len() + TAG_SIZE)
}

fn finish_export(
    persisted: Result<fs::File, tempfile::PersistError>,
    path: &Path,
    write_packet: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> Fallible<()> {
    let Err(error) = persisted else {
        return Ok(());
    };
    if !matches!(
        error.error.kind(),
        io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied
    ) {
        return Err(export_error(error.error));
    }
    // Some removable drives do not support exclusive rename or hard links.
    // Release the staged copy before allocating space for the final file.
    error.file.close()?;
    let mut output = OpenOptions::new();
    output.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        output.mode(0o600);
    }
    let mut file = output.open(path).map_err(export_error)?;
    write_packet(&mut file).map_err(|error| {
        format!("Запись не завершена. Удалите неполный файл или выберите другое имя. {error}")
            .into()
    })
}

fn export_error(error: io::Error) -> Box<dyn std::error::Error + Send + Sync> {
    if error.kind() == io::ErrorKind::AlreadyExists {
        "Файл с таким именем уже существует. Выберите другое имя.".into()
    } else {
        error.into()
    }
}

pub fn import_file(store: &SharedStore, path: &Path) -> Fallible<bool> {
    check_exchange_path(path)?;
    let current = store.lock().unwrap().snapshot();
    let workspace = current
        .workspace
        .as_ref()
        .ok_or("Сначала подключитесь к календарю команды.")?;
    let mut bytes = read_packet(path)?;
    let (prefix, rest) = bytes.split_at_mut(MAGIC.len());
    let (nonce, rest) = rest.split_at_mut(NONCE_SIZE);
    let (compressed, tag) = rest.split_at_mut(rest.len() - TAG_SIZE);
    let tag: &[u8; TAG_SIZE] = (&*tag).try_into()?;
    let nonce = XNonce::from_slice(nonce);
    cipher(workspace)
        .decrypt_in_place_detached(nonce, prefix, compressed, tag.into())
        .map_err(|_| "Файл повреждён или принадлежит другому календарю.")?;
    let packet = decode(compressed)?;
    if packet.workspace != workspace.id {
        return Err("Файл принадлежит другому календарю.".into());
    }
    let key = Zeroizing::new(workspace.key);
    drop(current);
    let mut store = store.lock().unwrap();
    if store.state().workspace.as_ref().is_none_or(|current| {
        current.id != packet.workspace || !bool::from(current.key.ct_eq(&*key))
    }) {
        return Err("Календарь команды изменился. Откройте файл заново.".into());
    }
    merge_calendar(
        &mut store,
        packet.workspace,
        packet.source,
        packet.calendar,
        None,
    )
}

fn check_exchange_path(path: &Path) -> Fallible<()> {
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if !matches!(path.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
        {
            return Err("Сохраните файл на локальный диск или флешку.".into());
        }
    }
    if path.file_name().is_none() {
        return Err("Выберите файл обмена.".into());
    }
    Ok(())
}

fn read_packet(path: &Path) -> Fallible<Zeroizing<Vec<u8>>> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err("Выберите обычный файл обмена.".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err("Выберите обычный файл обмена.".into());
        }
    }
    if !metadata.is_file() || metadata.len() > FILE_LIMIT as u64 {
        return Err("Файл обмена превышает допустимый размер.".into());
    }
    let mut reader = file.take(FILE_LIMIT as u64 + 1);
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    reader.read_to_end(&mut bytes)?;
    if reader.limit() == 0
        || bytes.len() <= MAGIC.len() + NONCE_SIZE + TAG_SIZE
        || !bytes.starts_with(MAGIC)
    {
        return Err("Файл обмена имеет неверный размер или формат.".into());
    }
    Ok(bytes)
}

fn decode(compressed: &[u8]) -> Fallible<Packet> {
    let mut reader = ZlibDecoder::new(compressed).take(CALENDAR_MESSAGE_LIMIT as u64 + 1);
    let packet = serde_json::from_reader(BufReader::new(&mut reader))
        .map_err(|_| "Не удалось прочитать календарь из файла обмена.")?;
    if reader.limit() == 0 || reader.into_inner().total_in() != compressed.len() as u64 {
        return Err("Файл обмена содержит неполные данные или превышает допустимый размер.".into());
    }
    Ok(packet)
}

fn cipher(workspace: &Workspace) -> XChaCha20Poly1305 {
    // Keyed BLAKE2s is a PRF under an already random team secret. The
    // distinct domain and workspace ID separate file encryption from Noise.
    let mut kdf = <Blake2sMac256 as KeyInit>::new((&workspace.key).into());
    Mac::update(
        &mut kdf,
        b"GodovoyPlan/OfflineExchange/XChaCha20Poly1305/1\0",
    );
    Mac::update(&mut kdf, workspace.id.as_bytes());
    let key = Zeroizing::new(<[u8; 32]>::from(kdf.finalize().into_bytes()));
    XChaCha20Poly1305::new((&*key).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_fallback_releases_staging_and_preserves_existing_files() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("calendar.yrplan");
        let staged = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let staged_path = staged.path().to_owned();
        finish_export(
            Err(tempfile::PersistError {
                error: io::ErrorKind::Unsupported.into(),
                file: staged,
            }),
            &target,
            |file| {
                assert!(!staged_path.exists());
                file.write_all(b"calendar")
            },
        )
        .unwrap();
        let staged = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let collision = finish_export(
            Err(tempfile::PersistError {
                error: io::ErrorKind::PermissionDenied.into(),
                file: staged,
            }),
            &target,
            |_| panic!("an existing file must never be opened for writing"),
        );
        assert!(collision.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"calendar");

        let failed = directory.path().join("incomplete.yrplan");
        let staged = tempfile::NamedTempFile::new_in(directory.path()).unwrap();
        let error = finish_export(
            Err(tempfile::PersistError {
                error: io::ErrorKind::Unsupported.into(),
                file: staged,
            }),
            &failed,
            |file| {
                file.write_all(b"incomplete")?;
                Err(io::ErrorKind::StorageFull.into())
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("Удалите неполный файл"));
        assert!(read_packet(&failed).is_err());
    }

    #[test]
    fn compressed_envelope_must_be_complete_and_consumed_once() {
        let packet = Packet {
            workspace: Id::from_u128(1),
            source: Id::from_u128(2),
            calendar: Calendar::default(),
        };
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        serde_json::to_writer(&mut encoder, &packet).unwrap();
        let mut bytes = encoder.finish().unwrap();
        assert_eq!(decode(&bytes).unwrap().workspace, packet.workspace);
        let tail = bytes.pop().unwrap();
        assert!(decode(&bytes).is_err());
        bytes.extend([tail, 0]);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn decompression_stops_at_its_own_limit() {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        serde_json::to_writer(
            &mut encoder,
            &Packet {
                workspace: Id::from_u128(1),
                source: Id::from_u128(2),
                calendar: Calendar::default(),
            },
        )
        .unwrap();
        for _ in 0..(CALENDAR_MESSAGE_LIMIT / 4096 + 1) {
            encoder.write_all(&[b' '; 4096]).unwrap();
        }
        assert!(decode(&encoder.finish().unwrap()).is_err());
    }
}
