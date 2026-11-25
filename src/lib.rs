#![forbid(unsafe_code)]
#[cfg(feature = "desktop")]
pub mod api;
#[cfg(feature = "desktop")]
mod import_report;
pub mod exchange;

pub mod model;
pub mod network;
pub mod planning;
pub mod storage;

pub const APP_NAME: &str = "Годовой план";
pub const APP_ID: &str = "local.godovoyplan.calendar";
pub type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
