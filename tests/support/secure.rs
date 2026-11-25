use godovoy_plan::api::{ApiKeyKind, ApiMethod, ApiReply, request_secure};
use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Transport,
    Status(u16),
}

pub fn request(
    port: u16,
    kind: ApiKeyKind,
    seed: u8,
    method: ApiMethod,
    path: &str,
    json: Option<&Value>,
) -> Result<ApiReply, Error> {
    // Integration tests also call this helper while running an unrelated async bridge client.
    let body = json
        .map(|value| serde_json::to_vec(value).unwrap())
        .unwrap_or_default();
    let reply = std::thread::scope(|threads| {
        threads
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(request_secure(port, &[seed; 32], kind, method, path, &body))
            })
            .join()
            .unwrap()
    })
    .map_err(|_| Error::Transport)?;
    if !(200..300).contains(&reply.status) {
        return Err(Error::Status(reply.status));
    }
    Ok(reply)
}

pub fn get(port: u16, seed: u8, path: &str) -> Result<Value, Error> {
    let response = request(port, ApiKeyKind::Main, seed, ApiMethod::Get, path, None)?;
    Ok(serde_json::from_slice(&response.body).unwrap())
}
pub fn put(port: u16, seed: u8, path: &str, value: &Value) -> Result<Value, Error> {
    let response = request(
        port,
        ApiKeyKind::Import,
        seed,
        ApiMethod::Put,
        path,
        Some(value),
    )?;
    Ok(serde_json::from_slice(&response.body).unwrap())
}
