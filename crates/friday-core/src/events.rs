//! 核心主动找两端：推事件、请两端做一件事再等它回话（v2.79 第 4 阶段起）。
//!
//! 两端在启动时经 C 接口 `friday_core_set_event_handler` 交给核心一个回调，核心有事时在自己的线程上调它，传一段 JSON：
//! - 事件：`{"event": "<名字>", "data": {…}}`，比如提示词文件变了（`promptStore.changed`）；
//! - 请求：`{"event": "request", "request": <编号>, "kind": "<要做的事>", "data": {…}}`，两端做完调 `core.reply`
//!   （`{request, result}` 或 `{request, error}`），核心这边等着的那个线程拿到结果接着走。用在核心做不了的事上：
//!   发 HTTP（两端走系统代理）、跑工具、问用户。
//!
//! 回调里不能卡住，也不要在回调里同步调回核心：两端收到就排到自己的线程（主线程、工作线程）上处理。
//! 核心调回调时不拿着任何锁。没有回调时事件丢掉、请求马上失败。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{LazyLock, Mutex, RwLock};
use std::time::Duration;

use serde_json::{Value, json};

/// 两端交来的回调（收一段 JSON 文本）
pub type Handler = Box<dyn Fn(&str) + Send + Sync>;

type Reply = Result<Value, String>;

static HANDLER: LazyLock<RwLock<Option<std::sync::Arc<Handler>>>> = LazyLock::new(|| RwLock::new(None));
static PENDING: LazyLock<Mutex<HashMap<u64, Sender<Reply>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

/// 换掉回调（None：不再推）
pub fn set_handler(handler: Option<Handler>) {
    *HANDLER.write().unwrap() = handler.map(std::sync::Arc::new);
}

pub fn has_handler() -> bool {
    HANDLER.read().unwrap().is_some()
}

fn deliver(message: &Value) -> bool {
    // 先把回调取出来再调，调的时候不拿着锁（回调里换回调也不会死锁）
    let handler = HANDLER.read().unwrap().clone();
    match handler {
        Some(handler) => {
            handler(&message.to_string());
            true
        }
        None => false,
    }
}

/// 推一个事件（没有回调就丢掉）
pub fn emit(event: &str, data: Value) {
    deliver(&json!({ "event": event, "data": data }));
}

/// 请两端做一件事，等它回话（最多等 timeout；None 一直等）。没有回调、等不到、两端报错都是 Err
pub fn request(kind: &str, data: Value, timeout: Option<Duration>) -> Result<Value, String> {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = mpsc::channel();
    PENDING.lock().unwrap().insert(id, sender);
    if !deliver(&json!({ "event": "request", "request": id, "kind": kind, "data": data })) {
        PENDING.lock().unwrap().remove(&id);
        return Err(format!("没有接上两端（{kind}）"));
    }
    let outcome = match timeout {
        Some(timeout) => receiver.recv_timeout(timeout).map_err(|_| format!("等不到两端回话（{kind}）")),
        None => receiver.recv().map_err(|_| format!("等不到两端回话（{kind}）")),
    };
    PENDING.lock().unwrap().remove(&id);
    outcome?
}

/// 两端回话（core.reply）；没有在等这个编号的（等超时了、编号不对）返回 false
pub fn reply(id: u64, outcome: Result<Value, String>) -> bool {
    let sender = PENDING.lock().unwrap().remove(&id);
    sender.is_some_and(|sender| sender.send(outcome).is_ok())
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn events_and_requests() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        set_handler(None);
        assert!(request("x", json!({}), Some(Duration::from_millis(10))).unwrap_err().starts_with("没有接上两端"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        // 两端的样子：收到请求就排到别的线程上回话
        set_handler(Some(Box::new(move |text: &str| {
            let message: Value = serde_json::from_str(text).unwrap();
            log.lock().unwrap().push(message.clone());
            if message["event"] == "request" {
                let id = message["request"].as_u64().unwrap();
                let data = message["data"].clone();
                std::thread::spawn(move || {
                    let outcome = if data["fail"] == true { Err("不行".to_string()) } else { Ok(json!({ "echo": data["x"] })) };
                    assert!(reply(id, outcome));
                });
            }
        })));
        emit("demo.changed", json!({ "keys": ["a"] }));
        assert_eq!(request("echo", json!({ "x": 3 }), Some(Duration::from_secs(5))), Ok(json!({ "echo": 3 })));
        assert_eq!(request("echo", json!({ "fail": true }), None), Err("不行".to_string()));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0], json!({ "event": "demo.changed", "data": { "keys": ["a"] } }));
        assert_eq!((seen[1]["event"].clone(), seen[1]["kind"].clone()), (json!("request"), json!("echo")));
        assert!(!reply(999_999, Ok(Value::Null)));
        drop(seen);
        // 没人回话：等到超时
        set_handler(Some(Box::new(|_| {})));
        assert!(request("silent", json!({}), Some(Duration::from_millis(20))).unwrap_err().starts_with("等不到两端回话"));
        set_handler(None);
    }
}
