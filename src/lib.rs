#![forbid(unsafe_code)]
#[cfg(feature = "local-api")]
pub mod api;
pub mod exchange;
pub mod hosted_address;
#[cfg(feature = "local-api")]
mod import_report;

pub mod model;
pub mod network;
pub mod planning;
pub mod storage;

pub const APP_NAME: &str = "Годовой план";
pub const APP_ID: &str = "local.godovoyplan.calendar";
pub type Fallible<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Render an untrusted diagnostic as one terminal line, preserving ordinary paths and text.
pub fn terminal_message(message: &str) -> String {
    let mut output = String::with_capacity(message.len());
    for c in message.chars() {
        if c.is_control()
            || matches!(c, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            output.extend(c.escape_default());
        } else {
            output.push(c);
        }
    }
    output
}
