//! Friday 的核心程序。Swift 版（macOS）和 Qt 版（Windows / Linux）都把它静态链进去，经 C 接口
//! （`friday-core-ffi`，头文件 `include/friday_core.h`）调用：方法名 + JSON 参数进，JSON 结果出（见 [`api`]）。
//!
//! 从两端各写一份的逻辑一块一块搬过来（docs/PORTING.md）；搬过来的一块，两端原来的实现就只剩调用这里的薄包装。

pub mod agents;
pub mod api;
pub mod archive;
pub mod chat_models;
pub mod chat_request;
pub mod chat_stream;
pub mod chooser;
pub mod context;
pub mod conversations;
pub mod delivery;
pub mod engine_loop;
pub mod engines;
pub mod events;
pub mod file_changes;
pub mod file_edit;
pub mod file_search;
pub mod folding;
pub mod gate;
pub mod journal;
pub mod line_diff;
pub mod liveness;
pub mod mcp_client;
pub mod mcp_config;
pub mod model_traits;
pub mod prompt_history;
pub mod prompt_store;
pub mod prompts;
pub mod quota;
pub mod repetition;
pub mod secret_guard;
pub mod secret_store;
pub mod shell_rules;
pub mod swift_json;
pub mod task_state;
pub mod tool_aliases;
pub mod tool_defs;
pub mod tool_files;
pub mod writer;

/// 版本号（== Cargo.toml 的 workspace.package.version）
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// C 接口的版本：方法改了不兼容的参数或结果时加一（两端的包装按它检查链进来的库）。2：加了事件回调 friday_core_set_event_handler
pub const ABI_VERSION: u32 = 2;
