//! friday-core 的 C 接口（头文件 include/friday_core.h，两边要一起改）。编成静态库链进 Swift 版和 Qt 版。
//!
//! 约定：字符串都是 UTF-8、以 NUL 结尾；`friday_core_call` 返回的字符串归调用方，用 `friday_core_free` 放掉。
//! 任意线程都能调（核心里没有全局可变状态）。核心里的 panic 在这里兜住，变成 `{"error": {"code": "internal", …}}`，不会跨过 C 边界。

use std::ffi::{CStr, CString, c_char};
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
}
