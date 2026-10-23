//! Friday 的核心程序。Swift 版（macOS）和 Qt 版（Windows / Linux）都把它静态链进去，经 C 接口
//! （`friday-core-ffi`，头文件 `include/friday_core.h`）调用：方法名 + JSON 参数进，JSON 结果出（见 [`api`]）。
//!
//! 从两端各写一份的逻辑一块一块搬过来（docs/PORTING.md）；搬过来的一块，两端原来的实现就只剩调用这里的薄包装。

pub mod api;
pub mod chat_models;
pub mod conversations;
pub mod folding;
pub mod line_diff;
pub mod quota;
pub mod secret_guard;
pub mod secret_store;
pub mod swift_json;
pub mod tool_aliases;

/// 版本号（== Cargo.toml 的 workspace.package.version）
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// C 接口的版本：方法改了不兼容的参数或结果时加一（两端的包装按它检查链进来的库）
pub const ABI_VERSION: u32 = 1;
