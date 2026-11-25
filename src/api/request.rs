use std::time::Duration;

use axum::{
    body::{Body, to_bytes},
    http::{Request, Uri},
};
use serde::{Deserialize, Serialize};
use snow::TransportState;
use tokio::net::TcpStream;
use tower::ServiceExt;

use super::{ApiState, BODY_LIMIT, Credential, bridge};
use crate::{Fallible, storage::PROFILE_LIMIT};

const HEADER_LIMIT: usize = 4096;
// Expanded occurrences include calculated dates and per-task progress.
const REPLY_LIMIT: usize = PROFILE_LIMIT * 4;
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiKeyKind {
    Main,
    Import,
}

impl ApiKeyKind {
    fn prefix(self) -> &'static [u8; 8] {
        match self {
            Self::Main => b"\0YRREST1",
            Self::Import => b"\0YRIMPT1",
        }
    }

    pub(super) fn from_prefix(prefix: [u8; 8]) -> Option<Self> {
        [Self::Main, Self::Import]
            .into_iter()
            .find(|kind| kind.prefix() == &prefix)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum ApiMethod {
    Get,
    Post,
    Put,
}

impl ApiMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RequestHead<'a> {
    method: ApiMethod,
    path: &'a str,
}

pub struct ApiReply {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Authenticates the local application before sending any route or calendar data.
pub struct ApiSession {
    stream: TcpStream,
    transport: TransportState,
}

impl ApiSession {
    pub async fn connect(port: u16, key: &[u8; 32], kind: ApiKeyKind) -> Fallible<Self> {
        let (stream, transport) = tokio::time::timeout(TIMEOUT,
            bridge::connect(port, key, kind.prefix(), kind.prefix())).await?
            .map_err(|_| "Не удалось подтвердить подлинность локального API. Проверьте порт, тип ключа и ключ.")?;
        Ok(Self { stream, transport })
    }

    pub async fn request(
        mut self,
        method: ApiMethod,
        path: &str,
        body: &[u8],
    ) -> Fallible<ApiReply> {
        if body.len() > BODY_LIMIT || (method == ApiMethod::Get) != body.is_empty() {
            return Err("Проверьте размер JSON и метод запроса.".into());
        }
        let head = serde_json::to_vec(&RequestHead { method, path })?;
        if head.len() > HEADER_LIMIT {
            return Err("Слишком длинный адрес запроса.".into());
        }
        tokio::time::timeout(TIMEOUT, async {
            bridge::send_payload(&mut self.stream, &mut self.transport, &head).await?;
            if !body.is_empty() {
                bridge::send_payload(&mut self.stream, &mut self.transport, body).await?;
            }
            let status = bridge::receive_payload(&mut self.stream, &mut self.transport, 2).await?;
            let status = u16::from_be_bytes(status.as_slice().try_into()?);
            let body =
                bridge::receive_payload(&mut self.stream, &mut self.transport, REPLY_LIMIT).await?;
            Ok(ApiReply { status, body })
        })
        .await?
    }
}

pub async fn request_secure(
    port: u16,
    key: &[u8; 32],
    kind: ApiKeyKind,
    method: ApiMethod,
    path: &str,
    body: &[u8],
) -> Fallible<ApiReply> {
    ApiSession::connect(port, key, kind)
        .await?
        .request(method, path, body)
        .await
}

pub(super) async fn serve(
    mut stream: TcpStream,
    api: ApiState,
    kind: ApiKeyKind,
    routes: axum::Router,
) -> Fallible<()> {
    let credential = {
        let view = api.reader.snapshot();
        let settings = view
            .state
            .api
            .as_ref()
            .filter(|s| s.enabled)
            .ok_or("API выключен.")?;
        match kind {
            ApiKeyKind::Main => Credential::Admin(settings.token),
            ApiKeyKind::Import => {
                Credential::Import(settings.import_token.ok_or("Импорт выключен.")?)
            }
        }
    };
    let key = match &credential {
        Credential::Admin(key) | Credential::Import(key) => key,
    };
    let mut channel = bridge::accept(&mut stream, key, kind.prefix()).await?;
    let head = bridge::receive_payload(&mut stream, &mut channel, HEADER_LIMIT).await?;
    let head: RequestHead = serde_json::from_slice(&head)?;
    let uri: Uri = head.path.parse()?;
    if uri.scheme().is_some() || uri.authority().is_some() || !uri.path().starts_with("/v1/") {
        return Err("Некорректный путь API.".into());
    }
    let body = if head.method == ApiMethod::Get {
        Body::empty()
    } else {
        Body::from(bridge::receive_payload(&mut stream, &mut channel, BODY_LIMIT).await?)
    };
    let request = Request::builder()
        .method(head.method.as_str())
        .uri(uri)
        .header("content-type", "application/json")
        .extension(credential)
        .body(body)?;
    let reply = routes.oneshot(request).await?;
    let status = reply.status().as_u16().to_be_bytes();
    let bytes = to_bytes(reply.into_body(), REPLY_LIMIT).await?;
    bridge::send_payload(&mut stream, &mut channel, &status).await?;
    bridge::send_payload(
        &mut stream,
        &mut channel,
        if bytes.is_empty() { b"null" } else { &bytes },
    )
    .await
}
