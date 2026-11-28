use std::{
    fmt,
    net::{Ipv4Addr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use snow::{Builder, HandshakeState, TransportState};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use super::{ApiState, BODY_LIMIT, Credential, ImportBatch, ImportResult, valid_source};
use crate::{Fallible, storage::LimitedWriter};

pub(super) const PREFIX: [u8; 8] = *b"\0YRAPI01";
const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const CONTEXT: &[u8] = b"GodovoyPlan/Bridge/1";
const REPORT_PREFIX: [u8; 8] = *b"\0YRAPIR1";
const REPORT_CONTEXT: &[u8] = b"GodovoyPlan/BridgeRun/1";
const REPORT_LIMIT: usize = 1024;
const CHUNK: usize = 60_000;
const TAG: usize = 16;
const MAX_PAYLOAD: usize = BODY_LIMIT + 128;
pub use crate::import_report::{BRIDGE_RUN_LIMIT, BridgeOutcome, BridgeRun};

#[derive(Debug, Eq, PartialEq)]
pub enum ImportTransferError {
    Unavailable,
    Rejected { status: u16 },
    ConfirmationLost,
}

impl fmt::Display for ImportTransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("Локальный API недоступен или не подтвердил подлинность. Проверьте версию и токен импорта."),
            Self::Rejected { status } => write!(f, "Локальный API отклонил импорт ({status})."),
            Self::ConfirmationLost => f.write_str("Нет подтверждения от локального API. Пачка могла сохраниться."),
        }
    }
}

impl std::error::Error for ImportTransferError {}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunRequest<T> {
    source: T,
    run: BridgeRun,
}

#[derive(Serialize)]
struct RequestView<'a> {
    source: &'a str,
    batch: &'a ImportBatch,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    source: String,
    batch: ImportBatch,
}

#[derive(Deserialize, Serialize)]
enum Reply {
    Applied(Vec<ImportResult>),
    Rejected { status: u16 },
}

/// Sends a single import over an encrypted session using the dedicated import token.
pub async fn import_secure(
    port: u16,
    token: &[u8; 32],
    source: &str,
    batch: &ImportBatch,
) -> Fallible<usize> {
    Ok(import_secure_tracked(port, token, source, batch).await?)
}

pub async fn import_secure_tracked(
    port: u16,
    token: &[u8; 32],
    source: &str,
    batch: &ImportBatch,
) -> Result<usize, ImportTransferError> {
    if !valid_source(source) {
        return Err(ImportTransferError::Unavailable);
    }
    let mut submitted = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        let (mut stream, mut transport) = connect(port, token, &PREFIX, CONTEXT)
            .await
            .map_err(|_| ImportTransferError::Unavailable)?;
        let mut json = Vec::new();
        serde_json::to_writer(
            LimitedWriter::new(&mut json, MAX_PAYLOAD),
            &RequestView { source, batch },
        )
        .map_err(|_| ImportTransferError::Unavailable)?;
        submitted = true;
        send_payload(&mut stream, &mut transport, &json)
            .await
            .map_err(|_| ImportTransferError::ConfirmationLost)?;
        match receive(&mut stream, &mut transport)
            .await
            .map_err(|_| ImportTransferError::ConfirmationLost)?
        {
            Reply::Applied(rows) if rows.len() == batch.events.len() => {
                Ok(rows.iter().filter(|row| row.changed).count())
            }
            Reply::Applied(_) => Err(ImportTransferError::ConfirmationLost),
            Reply::Rejected { status } => Err(ImportTransferError::Rejected { status }),
        }
    })
    .await
    .map_err(|_| {
        if submitted {
            ImportTransferError::ConfirmationLost
        } else {
            ImportTransferError::Unavailable
        }
    })?
}

pub async fn report_bridge_run(
    port: u16,
    token: &[u8; 32],
    source: &str,
    run: BridgeRun,
) -> Fallible<()> {
    if !valid_source(source) || !run.valid() {
        return Err("Некорректная сводка запуска моста.".into());
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        let (mut stream, mut transport) =
            connect(port, token, &REPORT_PREFIX, REPORT_CONTEXT).await?;
        let json = serde_json::to_vec(&RunRequest { source, run })?;
        send_report_frame(&mut stream, &mut transport, &json).await?;
        let mut frame = [0; REPORT_LIMIT + TAG];
        let mut plain = [0; REPORT_LIMIT];
        if !matches!(
            receive_report_frame(&mut stream, &mut transport, &mut frame, &mut plain).await?,
            b"saved" | b"visible"
        ) {
            return Err("Локальный API не подтвердил сводку запуска.".into());
        }
        Ok(())
    })
    .await
    .map_err(|_| "Превышено время ожидания сводки моста.")?
}

pub(super) async fn connect(
    port: u16,
    token: &[u8; 32],
    prefix: &[u8; 8],
    context: &[u8],
) -> Fallible<(TcpStream, TransportState)> {
    let mut stream = TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await?;
    stream.write_all(prefix).await?;
    let mut handshake = builder_for(token, context)?.build_initiator()?;
    send_handshake(&mut stream, &mut handshake).await?;
    read_handshake(&mut stream, &mut handshake).await?;
    Ok((stream, handshake.into_transport_mode()?))
}

pub(super) async fn serve(
    mut stream: TcpStream,
    api: ApiState,
    routes: axum::Router,
) -> Fallible<()> {
    let mut prefix = [0; PREFIX.len()];
    tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut prefix)).await??;
    if let Some(kind) = super::request::ApiKeyKind::from_prefix(prefix) {
        return super::request::serve(stream, api, kind, routes).await;
    }
    let context = match prefix {
        PREFIX => CONTEXT,
        REPORT_PREFIX => REPORT_CONTEXT,
        _ => return Err("Неподдерживаемая версия транспорта импорта.".into()),
    };
    let token = api
        .reader
        .snapshot()
        .state
        .api
        .as_ref()
        .filter(|settings| settings.enabled)
        .and_then(|settings| settings.import_token)
        .ok_or("Локальный импорт выключен.")?;
    let mut transport = accept(&mut stream, &token, context).await?;
    if prefix == REPORT_PREFIX {
        let mut frame = [0; REPORT_LIMIT + TAG];
        let mut plain = [0; REPORT_LIMIT];
        let json =
            receive_report_frame(&mut stream, &mut transport, &mut frame, &mut plain).await?;
        let request: RunRequest<String> = serde_json::from_slice(json)?;
        if !valid_source(&request.source) || !request.run.valid() {
            return Err("Некорректная сводка запуска моста.".into());
        }
        let permit = Arc::clone(&api.report_writer)
            .try_acquire_owned()
            .map_err(|_| "Локальный API ещё сохраняет предыдущую сводку.")?;
        let stored = tokio::task::spawn_blocking(move || -> super::ApiResult<bool> {
            let _permit = permit;
            let store = api.store.lock().unwrap();
            Credential::Import(token).require_current(store.state())?;
            let workspace = store
                .state()
                .workspace
                .as_ref()
                .ok_or_else(|| super::invalid("Пространство недоступно."))?;
            let identity = (store.state().device, workspace.id);
            let log = store.import_log();
            drop(store);
            Ok(log.record_run(identity.0, identity.1, &request.source, request.run))
        })
        .await?
        .map_err(|_| "Локальный API отклонил сводку запуска.")?;
        return send_report_frame(
            &mut stream,
            &mut transport,
            if stored { b"saved" } else { b"visible" },
        )
        .await;
    }
    let request: Request = receive(&mut stream, &mut transport).await?;
    let reply = match api
        .import(request.source, request.batch, Credential::Import(token))
        .await
    {
        Ok(rows) => Reply::Applied(rows),
        Err((status, _)) => Reply::Rejected {
            status: status.as_u16(),
        },
    };
    send(&mut stream, &mut transport, &reply).await
}

pub(super) async fn accept(
    stream: &mut TcpStream,
    token: &[u8; 32],
    context: &[u8],
) -> Fallible<TransportState> {
    let mut handshake = builder_for(token, context)?.build_responder()?;
    read_handshake(stream, &mut handshake).await?;
    send_handshake(stream, &mut handshake).await?;
    Ok(handshake.into_transport_mode()?)
}

#[cfg(test)]
fn builder(token: &[u8; 32]) -> Fallible<Builder<'_>> {
    builder_for(token, CONTEXT)
}

fn builder_for<'a>(token: &'a [u8; 32], context: &'a [u8]) -> Fallible<Builder<'a>> {
    Ok(Builder::new(PATTERN.parse()?)
        .prologue(context)?
        .psk(0, token)?)
}

async fn send_report_frame(
    stream: &mut TcpStream,
    transport: &mut TransportState,
    payload: &[u8],
) -> Fallible<()> {
    let mut frame = [0; 2 + REPORT_LIMIT + TAG];
    let count = transport.write_message(payload, &mut frame[2..])?;
    stream
        .write_all(frame_with_length(&mut frame, count)?)
        .await?;
    Ok(())
}

async fn receive_report_frame<'a>(
    stream: &mut TcpStream,
    transport: &mut TransportState,
    frame: &mut [u8; REPORT_LIMIT + TAG],
    plain: &'a mut [u8; REPORT_LIMIT],
) -> Fallible<&'a [u8]> {
    let encrypted = read_frame(stream, frame).await?;
    let count = transport.read_message(encrypted, plain)?;
    Ok(&plain[..count])
}

async fn send_handshake(stream: &mut TcpStream, handshake: &mut HandshakeState) -> Fallible<()> {
    let mut frame = [0; 2 + 128];
    let size = handshake.write_message(&[], &mut frame[2..])?;
    stream
        .write_all(frame_with_length(&mut frame, size)?)
        .await?;
    Ok(())
}

async fn read_handshake(stream: &mut TcpStream, handshake: &mut HandshakeState) -> Fallible<()> {
    let mut frame = [0; 128];
    let message = read_frame(stream, &mut frame).await?;
    if handshake.read_message(message, &mut [0; 128])? != 0 {
        return Err("Неверный заголовок сеанса импорта.".into());
    }
    Ok(())
}

fn frame_with_length(buffer: &mut [u8], payload_len: usize) -> Fallible<&[u8]> {
    buffer[..2].copy_from_slice(&u16::try_from(payload_len)?.to_be_bytes());
    Ok(&buffer[..2 + payload_len])
}

async fn read_frame<'a>(stream: &mut TcpStream, buffer: &'a mut [u8]) -> Fallible<&'a [u8]> {
    let size = usize::from(stream.read_u16().await?);
    if size == 0 || size > buffer.len() {
        return Err("Неверный размер сообщения импорта.".into());
    }
    stream.read_exact(&mut buffer[..size]).await?;
    Ok(&buffer[..size])
}

async fn send(
    stream: &mut TcpStream,
    transport: &mut TransportState,
    value: &impl Serialize,
) -> Fallible<()> {
    let mut json = Vec::new();
    serde_json::to_writer(LimitedWriter::new(&mut json, MAX_PAYLOAD), value)?;
    send_payload(stream, transport, &json).await
}

pub(super) async fn send_payload(
    stream: &mut TcpStream,
    transport: &mut TransportState,
    json: &[u8],
) -> Fallible<()> {
    send_payload_checked(stream, transport, json, || Ok(())).await
}

pub(super) async fn send_payload_checked(
    stream: &mut TcpStream,
    transport: &mut TransportState,
    json: &[u8],
    mut authorize: impl FnMut() -> Fallible<()>,
) -> Fallible<()> {
    let size = u32::try_from(json.len())?.to_be_bytes();
    // Keep the encrypted size frame and first data frame in the same socket write.
    // Each frame retains its own Noise nonce, length and authentication tag.
    let mut buffer = vec![0; 2 + size.len() + TAG + 2 + json.len().min(CHUNK) + TAG];
    let size_len = transport.write_message(&size, &mut buffer[2..])?;
    let mut prefix = frame_with_length(&mut buffer, size_len)?.len();
    for chunk in json.chunks(CHUNK) {
        authorize()?;
        let written = transport.write_message(chunk, &mut buffer[prefix + 2..])?;
        let end = prefix + frame_with_length(&mut buffer[prefix..], written)?.len();
        stream.write_all(&buffer[..end]).await?;
        prefix = 0;
    }
    if json.is_empty() {
        authorize()?;
        stream.write_all(&buffer[..prefix]).await?;
    }
    Ok(())
}

async fn receive<T: DeserializeOwned>(
    stream: &mut TcpStream,
    transport: &mut TransportState,
) -> Fallible<T> {
    Ok(serde_json::from_slice(
        &receive_payload(stream, transport, MAX_PAYLOAD).await?,
    )?)
}

pub(super) async fn receive_payload(
    stream: &mut TcpStream,
    transport: &mut TransportState,
    limit: usize,
) -> Fallible<Vec<u8>> {
    let mut size = [0; 4];
    let mut header = [0; 4 + TAG];
    let frame = read_frame(stream, &mut header).await?;
    if transport.read_message(frame, &mut size)? != size.len() {
        return Err("Неверный размер заголовка импорта.".into());
    }
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > limit {
        return Err("Импорт превышает разрешённый размер.".into());
    }
    let mut json = vec![0; size];
    let mut buffer = vec![0; size.min(CHUNK) + TAG];
    for chunk in json.chunks_mut(CHUNK) {
        let frame = read_frame(stream, &mut buffer[..chunk.len() + TAG]).await?;
        if transport.read_message(frame, chunk)? != chunk.len() {
            return Err("Получено неполное сообщение импорта.".into());
        }
    }
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::ApiServer,
        storage::{ApiSettings, Store},
    };

    #[tokio::test(flavor = "current_thread")]
    async fn a_partial_payload_is_discarded_when_access_ends_between_chunks() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = [17; 32];
        let (checks, reply) = tokio::join!(
            async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut prefix = [0; 8];
                stream.read_exact(&mut prefix).await.unwrap();
                assert_eq!(prefix, PREFIX);
                let mut transport = accept(&mut stream, &token, CONTEXT).await.unwrap();
                let mut checks = 0;
                let result =
                    send_payload_checked(&mut stream, &mut transport, &vec![7; CHUNK + 1], || {
                        checks += 1;
                        if checks == 1 {
                            Ok(())
                        } else {
                            Err("Отозвано".into())
                        }
                    })
                    .await;
                assert!(result.is_err());
                checks
            },
            async {
                let (mut stream, mut transport) =
                    connect(port, &token, &PREFIX, CONTEXT).await.unwrap();
                receive_payload(&mut stream, &mut transport, CHUNK + 1).await
            }
        );
        assert_eq!(checks, 2);
        assert!(reply.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_sent_batch_with_no_reply_has_unknown_confirmation() {
        let token = [21; 32];
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut prefix = [0; 8];
            stream.read_exact(&mut prefix).await.unwrap();
            assert_eq!(prefix, PREFIX);
            let mut handshake = builder(&token).unwrap().build_responder().unwrap();
            read_handshake(&mut stream, &mut handshake).await.unwrap();
            send_handshake(&mut stream, &mut handshake).await.unwrap();
            let mut transport = handshake.into_transport_mode().unwrap();
            let request: Request = receive(&mut stream, &mut transport).await.unwrap();
            assert_eq!(request.source, "qa");
            assert_eq!(request.batch.events.len(), 1);
        });
        let batch = serde_json::from_value(serde_json::json!({"events":[{
            "externalId":"qa-test", "event":{"title":"Встреча","date":"2026-12-20"}
        }]}))
        .unwrap();
        assert_eq!(
            import_secure_tracked(port, &token, "qa", &batch).await,
            Err(ImportTransferError::ConfirmationLost)
        );
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_open_report_session_checks_rotated_token_before_publication() {
        let profile = tempfile::tempdir().unwrap();
        let store = Store::open(&profile.path().join("profile")).unwrap();
        let token = [11; 32];
        store
            .lock()
            .unwrap()
            .transaction(|state| {
                state.create_workspace("Команда".into(), "Анна".into())?;
                state.api = Some(ApiSettings {
                    token: [22; 32],
                    import_token: Some(token),
                    port: 0,
                    enabled: true,
                });
                Ok(())
            })
            .unwrap();
        let server = ApiServer::start(store.clone(), 0).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            let (mut stream, mut transport) =
                connect(server.port(), &token, &REPORT_PREFIX, REPORT_CONTEXT)
                    .await
                    .unwrap();
            store
                .lock()
                .unwrap()
                .transaction(|state| {
                    state.api.as_mut().unwrap().import_token = Some([33; 32]);
                    Ok(())
                })
                .unwrap();
            let json = serde_json::to_vec(&RunRequest {
                source: "qa",
                run: BridgeRun {
                    fetched: 0,
                    confirmed: 0,
                    changed: 0,
                    outcome: BridgeOutcome::Complete,
                },
            })
            .unwrap();
            send_report_frame(&mut stream, &mut transport, &json)
                .await
                .unwrap();
            let mut frame = [0; REPORT_LIMIT + TAG];
            let mut plain = [0; REPORT_LIMIT];
            assert!(
                receive_report_frame(&mut stream, &mut transport, &mut frame, &mut plain)
                    .await
                    .is_err()
            );
        })
        .await
        .unwrap();
        assert!(store.lock().unwrap().reader().import_status().is_empty());
        assert!(!profile.path().join("profile/bridge-runs-v1.json").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_open_session_checks_credentials_before_importing() {
        for (enabled, expected) in [(true, 401), (false, 503)] {
            let profile = tempfile::tempdir().unwrap();
            let store = Store::open(&profile.path().join("profile")).unwrap();
            let token = [11; 32];
            store
                .lock()
                .unwrap()
                .transaction(|state| {
                    state.create_workspace("Команда".into(), "Анна".into())?;
                    state.api = Some(ApiSettings {
                        token: [22; 32],
                        import_token: Some(token),
                        port: 0,
                        enabled: true,
                    });
                    Ok(())
                })
                .unwrap();
            let server = ApiServer::start(store.clone(), 0).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, server.port()))
                    .await
                    .unwrap();
                stream.write_all(&PREFIX).await.unwrap();
                let mut handshake = builder(&token).unwrap().build_initiator().unwrap();
                send_handshake(&mut stream, &mut handshake).await.unwrap();
                read_handshake(&mut stream, &mut handshake).await.unwrap();
                let mut transport = handshake.into_transport_mode().unwrap();

                store
                    .lock()
                    .unwrap()
                    .transaction(|state| {
                        let settings = state.api.as_mut().unwrap();
                        settings.enabled = enabled;
                        if enabled {
                            settings.import_token = Some([33; 32]);
                        }
                        Ok(())
                    })
                    .unwrap();
                let batch = serde_json::from_value(serde_json::json!({"events":[{
                    "externalId":"qa-pending", "event":{"title":"Тест импорта","date":"2026-12-20"}
                }]}))
                .unwrap();
                send(
                    &mut stream,
                    &mut transport,
                    &RequestView {
                        source: "crm",
                        batch: &batch,
                    },
                )
                .await
                .unwrap();
                let reply = receive(&mut stream, &mut transport).await.unwrap();
                assert!(matches!(reply, Reply::Rejected { status } if status == expected));
            })
            .await
            .unwrap();
            assert!(
                store
                    .lock()
                    .unwrap()
                    .state()
                    .workspace
                    .as_ref()
                    .unwrap()
                    .calendar
                    .plans
                    .is_empty()
            );
        }
    }
}
