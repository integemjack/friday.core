//! friday-core 的 C 接口（头文件 include/friday_core.h，两边要一起改）。编成静态库链进 Swift 版和 Qt 版。
//!
//! 约定：字符串都是 UTF-8、以 NUL 结尾；`friday_core_call` 返回的字符串归调用方，用 `friday_core_free` 放掉。
//! 任意线程都能调（有状态的部分在核心里加锁）。核心里的 panic 在这里兜住，变成 `{"error": {"code": "internal", …}}`，不会跨过 C 边界。

use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

use friday_core::api;

/// 执行一个方法：method 是方法名（如 "quota.classify"），params 是 JSON 对象的文本（NULL 或空串 = {}）。
/// 返回 `{"result": …}` 或 `{"error": {"code", "message"}}` 的 JSON 文本，不会返回 NULL。
///
/// # Safety
/// method、params 要么是 NULL，要么指向以 NUL 结尾的字符串，并且在这次调用里有效。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn friday_core_call(method: *const c_char, params: *const c_char) -> *mut c_char {
    let method = unsafe { text(method) };
    let params = unsafe { text(params) };
    let response = catch_unwind(AssertUnwindSafe(|| api::call(&method, &params))).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<&str>()
            .map(|text| text.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "核心程序出错".to_string());
        api::envelope(Err(api::ApiError { code: "internal", message })).to_string()
    });
    // JSON 文本里不会有 NUL（字符串里的 \0 会被转义成 \u0000）
    CString::new(response).map_or(std::ptr::null_mut(), CString::into_raw)
}

/// 放掉 friday_core_call 返回的字符串（NULL 什么都不做）
///
/// # Safety
/// text 是 friday_core_call 返回的、还没放掉的指针，或 NULL。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn friday_core_free(text: *mut c_char) {
    if !text.is_null() {
        drop(unsafe { CString::from_raw(text) });
    }
}

/// 核心的版本号（静态字符串，不用放）
#[unsafe(no_mangle)]
pub extern "C" fn friday_core_version() -> *const c_char {
    static VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "\0");
    VERSION.as_ptr().cast()
}

/// C 接口的版本（== FRIDAY_CORE_ABI_VERSION；不一致说明头文件和库不是同一版）
#[unsafe(no_mangle)]
pub extern "C" fn friday_core_abi_version() -> u32 {
    friday_core::ABI_VERSION
}

/// 核心有事找两端时调的回调：context 原样带回，event_json 是 {"event", …} 的 JSON 文本（只在这次调用里有效，要留着就复制）
pub type EventHandler = Option<unsafe extern "C" fn(context: *mut c_void, event_json: *const c_char)>;

struct Callback {
    handler: unsafe extern "C" fn(*mut c_void, *const c_char),
    context: usize,
}

/// 交给核心一个回调（handler 为 NULL：不再推）。核心在自己的线程上调它：推事件、请两端做事（两端做完调 core.reply）。
/// 回调里不能卡住，也不要同步调回核心：收到就排到自己的线程上处理（见 friday-core 的 events.rs）
///
/// # Safety
/// handler 在换掉之前一直能调，context 在这期间一直有效，handler 能在任意线程上调。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn friday_core_set_event_handler(handler: EventHandler, context: *mut c_void) {
    let callback = handler.map(|handler| Callback { handler, context: context as usize });
    friday_core::events::set_handler(callback.map(|callback| -> friday_core::events::Handler {
        Box::new(move |text: &str| {
            if let Ok(text) = CString::new(text) {
                let _ = catch_unwind(AssertUnwindSafe(|| unsafe { (callback.handler)(callback.context as *mut c_void, text.as_ptr()) }));
            }
        })
    }));
}

unsafe fn text(pointer: *const c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(pointer) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let method = CString::new("quota.duration").unwrap();
        let params = CString::new(r#"{"text":"1h"}"#).unwrap();
        unsafe {
            let response = friday_core_call(method.as_ptr(), params.as_ptr());
            assert_eq!(CStr::from_ptr(response).to_str().unwrap(), r#"{"result":3600.0}"#);
            friday_core_free(response);
            let response = friday_core_call(std::ptr::null(), std::ptr::null());
            assert!(CStr::from_ptr(response).to_str().unwrap().contains("unknownMethod"));
            friday_core_free(response);
            assert_eq!(CStr::from_ptr(friday_core_version()).to_str().unwrap(), friday_core::VERSION);
        }
        assert_eq!(friday_core_abi_version(), friday_core::ABI_VERSION);
    }

    static RECEIVED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    unsafe extern "C" fn collect(context: *mut c_void, event: *const c_char) {
        assert_eq!(context as usize, 42);
        RECEIVED.lock().unwrap().push(unsafe { CStr::from_ptr(event) }.to_string_lossy().into_owned());
    }

    #[test]
    fn events() {
        unsafe {
            friday_core_set_event_handler(Some(collect), 42 as *mut c_void);
            let method = CString::new("core.ping").unwrap();
            let params = CString::new(r#"{"data":{"n":1}}"#).unwrap();
            friday_core_free(friday_core_call(method.as_ptr(), params.as_ptr()));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while RECEIVED.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(RECEIVED.lock().unwrap().first().map(String::as_str), Some(r#"{"data":{"n":1},"event":"core.pong"}"#));
        unsafe { friday_core_set_event_handler(None, std::ptr::null_mut()) };
    }
}
