//! Ownership and thread contract at the Swift/JNI boundary.
use crate::Mobile;
use serde_json::json;
use std::{
    cell::RefCell,
    ffi::{CStr, CString, c_char, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    ptr,
    sync::Mutex,
};

thread_local! {
    static OPEN_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// # Safety
/// `profile` must be a live NUL-terminated UTF-8 path in the app container.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gp_open(profile: *const c_char) -> *mut c_void {
    let result = catch_unwind(|| -> godovoy_plan::Fallible<Mobile> {
        if profile.is_null() {
            return Err("Не указан путь к профилю.".into());
        }
        let path = unsafe { CStr::from_ptr(profile) }.to_str()?;
        if path.len() > 4096 {
            return Err("Слишком длинный путь к профилю.".into());
        }
        Mobile::open(PathBuf::from(path))
    });
    match result {
        Ok(Ok(mobile)) => {
            OPEN_ERROR.with(|s| s.replace(None));
            Box::into_raw(Box::new(Mutex::new(mobile))).cast()
        }
        result => {
            let message = match result {
                Ok(Err(e)) => e.to_string(),
                _ => "Внутренняя ошибка при открытии профиля.".to_owned(),
            };
            OPEN_ERROR.with(|s| s.replace(Some(message)));
            ptr::null_mut()
        }
    }
}

/// Return the diagnostic from the last gp_open on this native thread.
/// Returns null if the last open succeeded. Free the copied string with gp_string_free.
#[unsafe(no_mangle)]
pub extern "C" fn gp_open_error() -> *mut c_char {
    OPEN_ERROR.with(|s| {
        s.borrow()
            .as_deref()
            .and_then(|e| CString::new(e).ok())
            .map_or(ptr::null_mut(), CString::into_raw)
    })
}

/// # Safety
/// `handle` is a live result of gp_open. `input` is a NUL-terminated UTF-8
/// command. Neither may be freed during this call. Free the result with gp_string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gp_request(handle: *mut c_void, input: *const c_char) -> *mut c_char {
    let result = catch_unwind(AssertUnwindSafe(|| -> godovoy_plan::Fallible<String> {
        if input.is_null() || handle.is_null() {
            return Err("Профиль недоступен.".into());
        }
        let bytes = unsafe { CStr::from_ptr(input) }.to_bytes();
        if bytes.len() > 256 * 1024 {
            return Err("Слишком большой запрос.".into());
        }
        let cmd = serde_json::from_slice(bytes).map_err(|_| "Неверный JSON.")?;
        let handle = unsafe { &*handle.cast::<Mutex<Mobile>>() };
        handle
            .lock()
            .map_err(|_| "Откройте профиль заново.")?
            .request(cmd)
    }));
    let response = match result {
        Ok(Ok(data)) => data,
        Ok(Err(error)) => json!({"ok":false,"error":error.to_string()}).to_string(),
        Err(_) => {
            json!({"ok":false,"error":"Внутренняя ошибка. Откройте приложение заново."}).to_string()
        }
    };
    CString::new(response).map_or(ptr::null_mut(), CString::into_raw)
}

/// # Safety
/// Pass a live gp_open result exactly once, after all callers have finished.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gp_close(handle: *mut c_void) {
    if !handle.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            drop(unsafe { Box::from_raw(handle.cast::<Mutex<Mobile>>()) })
        }));
    }
}

/// # Safety
/// Pass a string returned by gp_request exactly once, after the caller has copied it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gp_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Validate a user-entered server origin without opening a calendar profile.
/// # Safety
/// The caller supplies a live buffer of exactly `length` bytes for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gp_valid_hosted_address(input: *const u8, length: usize) -> u8 {
    if input.is_null() || length == 0 || length > 300 {
        return 0;
    }
    let bytes = unsafe { std::slice::from_raw_parts(input, length) };
    u8::from(
        std::str::from_utf8(bytes).is_ok_and(godovoy_plan::hosted_address::valid_hosted_address),
    )
}
