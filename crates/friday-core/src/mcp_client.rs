//! MCP（Model Context Protocol）客户端（== Qt 原来的 mcp/McpClient、Swift 原来用的 MCP Swift SDK：Client + 传输）。
//!
//! 协议：JSON-RPC 2.0。握手 initialize（protocolVersion "2025-06-18"，clientInfo {name: "Friday", version: "1.0"}）
//! → notifications/initialized；之后 tools/list（分页 cursor）、tools/call。
//! 传输：
//! - stdio：核心起 server 进程（程序路径、环境变量、工作目录由两端给：两端按自己的 PATH 找程序、替换 ${secret:…}），
//!   按行（\n 分隔的 JSON）读写；stderr 按块解码后推 `mcpClient.stderr`（Windows 上某块不是合法 UTF-8 时改按系统代码页）；
//! - Streamable HTTP：发 HTTP 留在两端（走系统代理）：核心推 `mcpClient.httpPost`，两端把响应头、一块块的响应体、结束
//!   经 `mcpClient.httpHead / httpData / httpEnd` 交回来；响应是 JSON 或 SSE，SSE 在核心里逐行解。响应已经到了而服务端不关
//!   SSE 流时核心推 `mcpClient.httpAbort` 让两端断开。记住响应头 Mcp-Session-Id 并在后续请求里带上。
//!
//! 服务端发来的请求：ping 回空结果，其余回 -32601（== SDK 没注册处理器时的回复）；通知忽略。
//! 进程自己退出（启动就报错退出、运行中崩溃）：待决请求立即以「服务进程已退出（退出码 N）」/「服务进程被信号 N 结束」失败
//! （不用等到超时），并推 `mcpClient.exited`；我们 disconnect 结束的不算。
//! 结束进程：先关 stdin，再 SIGTERM（== Process.terminate()），3 秒后强制结束。Unix 上 server 在新会话里（终端上的 Ctrl+C
//! 不连带结束它）；Windows 上不弹控制台窗口，并把进程放进「关闭即结束」的作业对象，npx.cmd 经 cmd.exe 起的 node 等子孙进程随之结束。
//!
//! 公开的方法都阻塞（两端在工作线程里调）：超时「操作超时（N 秒）」（== Swift 的 withTimeout）；两端给了 call 编号的，
//! `mcpClient.cancel` 让它马上以「已取消」结束。错误文案照抄 SDK 的 MCPError.errorDescription（英文原文）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::events;

/// 客户端声明的协议版本（服务端可以回自己支持的版本，之后 HTTP 请求头带协商结果）
pub const PROTOCOL_VERSION: &str = "2025-06-18";

// SDK 的 MCPError.errorDescription 原文
pub const CONNECTION_CLOSED: &str = "Connection closed";
pub const CLIENT_DISCONNECTED: &str = "Internal error: Client disconnected";
pub const NOT_INITIALIZED: &str = "Internal error: Client connection not initialized";
/// SDK 的 AnyPendingRequest：响应的 result 解不成 M.Result 时抛 TypeMismatchError，这是它的 zh-Hans localizedDescription
pub const TYPE_MISMATCH: &str = "未能完成操作。（MCP.TypeMismatchError错误1。）";
/// 两端取消了这次调用（mcpClient.cancel）
pub const CANCELLED: &str = "已取消";

/// 一次失败：message 是给人看的；cancelled 是两端取消的
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub message: String,
    pub cancelled: bool,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Failure { message: message.into(), cancelled: false }
    }
}

type Reply = Result<Value, Failure>;

pub fn timeout_message(timeout_ms: u64) -> String {
    format!("操作超时（{} 秒）", timeout_ms / 1000)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    None,
    Stdio,
    Http,
}

/// 一次 HTTP POST 的解析状态
#[derive(Default)]
struct Post {
    /// 这个 POST 带的请求（通知、回服务端的话没有）
    request: Option<i64>,
    /// 通知：两端说发完了（或失败）时告诉等着的人
    done: Option<Sender<Result<(), Failure>>>,
    headers_handled: bool,
    /// 非空 = 状态码错误（SDK 的 processHTTPResponse 抛出的文字）
    status_error: String,
    is_sse: bool,
    is_json: bool,
    buffer: Vec<u8>,
    /// SSE：当前事件累积的 data 行
    event_data: Vec<Vec<u8>>,
    /// 响应已经到了、正在让两端断开
    responded: bool,
}

#[derive(Default)]
struct Process {
    pid: u32,
    /// 对外：还算在运行（我们断开后就不算了）
    running: bool,
    /// 真的退出了
    exited: bool,
    #[cfg(windows)]
    job: Option<windows::Job>,
}

struct State {
    mode: Mode,
    /// disconnect 之后（不再接受新请求）
    closed: bool,
    connected: bool,
    protocol_version: String,
    pending: HashMap<i64, Sender<Reply>>,
    /// 两端的 call 编号 → 请求 id
    calls: HashMap<u64, i64>,
    process: Option<Process>,
    endpoint: String,
    headers: Vec<(String, String)>,
    session_id: String,
    posts: HashMap<u64, Post>,
    next_post: u64,
}

pub struct Client {
    id: u64,
    name: String,
    version: String,
    state: Mutex<State>,
    /// 写 stdin 单独一把锁：写的时候（管道满了会卡住）不挡读线程分发响应
    stdin: Mutex<Option<ChildStdin>>,
    next_id: AtomicI64,
}

static CLIENTS: LazyLock<Mutex<HashMap<u64, Arc<Client>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT: AtomicU64 = AtomicU64::new(1);

pub fn open(name: &str, version: &str) -> u64 {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let client = Arc::new(Client {
        id,
        name: name.to_string(),
        version: version.to_string(),
        state: Mutex::new(State {
            mode: Mode::None,
            closed: false,
            connected: false,
            protocol_version: PROTOCOL_VERSION.to_string(),
            pending: HashMap::new(),
            calls: HashMap::new(),
            process: None,
            endpoint: String::new(),
            headers: Vec::new(),
            session_id: String::new(),
            posts: HashMap::new(),
            next_post: 1,
        }),
        stdin: Mutex::new(None),
        next_id: AtomicI64::new(0),
    });
    CLIENTS.lock().unwrap().insert(id, client);
    id
}

pub fn get(id: u64) -> Option<Arc<Client>> {
    CLIENTS.lock().unwrap().get(&id).cloned()
}

/// 断开并丢掉句柄（还在体面退出的进程照样 3 秒后强制结束）
pub fn close(id: u64) {
    let client = CLIENTS.lock().unwrap().remove(&id);
    if let Some(client) = client {
        client.disconnect();
    }
}

impl Client {
    pub fn is_connected(&self) -> bool {
        self.state.lock().unwrap().connected
    }

    pub fn process_running(&self) -> bool {
        self.state.lock().unwrap().process.as_ref().is_some_and(|process| process.running)
    }

    pub fn pid(&self) -> Option<u32> {
        self.state.lock().unwrap().process.as_ref().map(|process| process.pid)
    }

    // MARK: 连接

    /// 起 stdio server 并完成握手
    pub fn connect_stdio(
        self: &Arc<Self>,
        program: &str,
        arguments: &[String],
        environment: &HashMap<String, String>,
        working_directory: &str,
        timeout_ms: u64,
        call: Option<u64>,
    ) -> Result<(), Failure> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        if self.state.lock().unwrap().closed {
            return Err(Failure::new(CLIENT_DISCONNECTED));
        }
        let mut command = Command::new(program);
        command.args(arguments).env_clear().envs(environment).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if !working_directory.is_empty() {
            command.current_dir(working_directory);
        }
        detach_from_terminal(&mut command);
        let mut child = command.spawn().map_err(|error| Failure::new(error.to_string()))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        {
            let mut state = self.state.lock().unwrap();
            state.mode = Mode::Stdio;
            state.process = Some(Process {
                pid: child.id(),
                running: true,
                exited: false,
                #[cfg(windows)]
                job: windows::Job::attach(&child),
            });
        }
        *self.stdin.lock().unwrap() = stdin;
        let (finished, readers_done) = mpsc::channel::<()>();
        if let Some(stdout) = stdout {
            let client = self.clone();
            let finished = finished.clone();
            spawn("friday-mcp-stdout", move || {
                client.read_stdout(stdout);
                let _ = finished.send(());
            });
        }
        if let Some(stderr) = stderr {
            let client = self.clone();
            spawn("friday-mcp-stderr", move || {
                client.read_stderr(stderr);
                let _ = finished.send(());
            });
        }
        let client = self.clone();
        spawn("friday-mcp-wait", move || client.wait_for_exit(child, readers_done));
        self.handshake(deadline, timeout_ms, call)
    }

    /// 连远程 Streamable HTTP server 并完成握手；headers：每个请求带上的请求头（令牌等，${secret:…} 已替换）
    pub fn connect_http(
        self: &Arc<Self>,
        url: &str,
        headers: Vec<(String, String)>,
        timeout_ms: u64,
        call: Option<u64>,
    ) -> Result<(), Failure> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return Err(Failure::new(CLIENT_DISCONNECTED));
            }
            state.mode = Mode::Http;
            state.endpoint = url.to_string();
            state.headers = headers;
        }
        self.handshake(deadline, timeout_ms, call)
    }

    /// == Client._initialize()：initialize → notifications/initialized
    fn handshake(self: &Arc<Self>, deadline: Instant, timeout_ms: u64, call: Option<u64>) -> Result<(), Failure> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": self.name, "version": self.version },
        });
        let result = self.request("initialize", params, deadline, timeout_ms, call)?;
        if let Some(negotiated) = result.get("protocolVersion").and_then(Value::as_str).filter(|version| !version.is_empty()) {
            // == httpTransport.updateNegotiatedProtocolVersion：之后的 MCP-Protocol-Version 请求头用协商结果
            self.state.lock().unwrap().protocol_version = negotiated.to_string();
        }
        self.notify("notifications/initialized", deadline, timeout_ms)?;
        self.state.lock().unwrap().connected = true;
        Ok(())
    }

    // MARK: 请求

    /// 发一个请求并等响应的 result
    pub fn request(self: &Arc<Self>, method: &str, params: Value, deadline: Instant, timeout_ms: u64, call: Option<u64>) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let (sender, receiver) = mpsc::channel();
        let mode = {
            let mut state = self.state.lock().unwrap();
            if state.closed || state.mode == Mode::None {
                return Err(Failure::new(NOT_INITIALIZED));
            }
            if state.mode == Mode::Stdio && !state.process.as_ref().is_some_and(|process| process.running) {
                return Err(Failure::new(CONNECTION_CLOSED));
            }
            state.pending.insert(id, sender);
            if let Some(call) = call {
                state.calls.insert(call, id);
            }
            state.mode
        };
        match mode {
            Mode::Stdio => {
                self.write_line(&message);
            }
            _ => self.post(&message, Some(id), None),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let outcome = match receiver.recv_timeout(remaining) {
            Ok(reply) => reply,
            Err(RecvTimeoutError::Timeout) => Err(Failure::new(timeout_message(timeout_ms))),
            Err(RecvTimeoutError::Disconnected) => Err(Failure::new(CLIENT_DISCONNECTED)),
        };
        if outcome.is_err() {
            self.forget(id);
        }
        if let Some(call) = call {
            self.state.lock().unwrap().calls.remove(&call);
        }
        outcome
    }

    /// 发一个通知；HTTP 时等到服务端确认（202）
    fn notify(self: &Arc<Self>, method: &str, deadline: Instant, timeout_ms: u64) -> Result<(), Failure> {
        let message = json!({ "jsonrpc": "2.0", "method": method });
        let mode = {
            let state = self.state.lock().unwrap();
            if state.closed || state.mode == Mode::None {
                return Err(Failure::new(NOT_INITIALIZED));
            }
            state.mode
        };
        if mode == Mode::Stdio {
            return if self.write_line(&message) { Ok(()) } else { Err(Failure::new(CONNECTION_CLOSED)) };
        }
        let (sender, receiver) = mpsc::channel();
        self.post(&message, None, Some(sender));
        match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(outcome) => outcome,
            Err(_) => Err(Failure::new(timeout_message(timeout_ms))),
        }
    }

    /// 调用方超时 / 取消：不再等这个响应（HTTP 的请求让两端断开）
    fn forget(&self, id: i64) {
        let abort = {
            let mut state = self.state.lock().unwrap();
            state.pending.remove(&id);
            let post = state.posts.iter().find(|(_, post)| post.request == Some(id)).map(|(key, _)| *key);
            if let Some(key) = post
                && let Some(post) = state.posts.get_mut(&key)
            {
                post.request = None;
            }
            post
        };
        if let Some(post) = abort {
            events::emit("mcpClient.httpAbort", json!({ "client": self.id, "post": post }));
        }
    }

    /// 两端取消了这次调用：马上以「已取消」结束
    pub fn cancel(&self, call: u64) {
        let sender = {
            let mut state = self.state.lock().unwrap();
            let id = state.calls.remove(&call);
            id.and_then(|id| state.pending.remove(&id))
        };
        if let Some(sender) = sender {
            let _ = sender.send(Err(Failure { message: CANCELLED.to_string(), cancelled: true }));
        }
    }

    /// tools/list 的一页：工具宽松地解（有意比 SDK 宽松：SDK 要求 name、inputSchema 必填，否则整个 tools/list 失败）
    pub fn list_tools(self: &Arc<Self>, cursor: Option<&str>, timeout_ms: u64, call: Option<u64>) -> Result<Value, Failure> {
        // == ListTools.request(.init()) / .init(cursor:)：没有游标时 params 是 {}
        let params = match cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let result = self.request("tools/list", params, deadline, timeout_ms, call)?;
        let tools: Vec<Value> = result
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item.is_object())
            .map(normalize_tool)
            .collect();
        let mut page = json!({ "tools": tools });
        if let Some(cursor) = result.get("nextCursor").and_then(Value::as_str) {
            page["nextCursor"] = json!(cursor);
        }
        Ok(page)
    }

    /// tools/call：结果按 SDK 的 CallTool.Result 严格解码（content 必填、isError 必须是布尔…），解不出来是 TYPE_MISMATCH
    pub fn call_tool(self: &Arc<Self>, name: &str, arguments: Value, timeout_ms: u64, call: Option<u64>) -> Result<Value, Failure> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let result = self.request("tools/call", json!({ "name": name, "arguments": arguments }), deadline, timeout_ms, call)?;
        decode_call_result(&result).map_err(|_| Failure::new(TYPE_MISMATCH))
    }

    /// == Client.disconnect()：待决请求失败、断开传输、结束进程；可以重复调用
    pub fn disconnect(self: &Arc<Self>) {
        let (waiting, posts, pid) = {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.connected = false;
            let waiting: Vec<Sender<Reply>> = state.pending.drain().map(|(_, sender)| sender).collect();
            state.calls.clear();
            let posts: Vec<u64> = state.posts.keys().copied().collect();
            let pid = state.process.as_mut().filter(|process| !process.exited).map(|process| {
                process.running = false;
                process.pid
            });
            (waiting, posts, pid)
        };
        for sender in waiting {
            let _ = sender.send(Err(Failure::new(CLIENT_DISCONNECTED)));
        }
        for post in posts {
            events::emit("mcpClient.httpAbort", json!({ "client": self.id, "post": post }));
        }
        // 先关 stdin：多数 server 读到 EOF 就自己退出
        self.stdin.lock().unwrap().take();
        if let Some(pid) = pid {
            terminate(pid);
            // 3 秒还没退出就强制结束（Windows 上连同作业对象里的子孙进程）
            let client = self.clone();
            spawn("friday-mcp-kill", move || {
                std::thread::sleep(Duration::from_secs(3));
                let state = client.state.lock().unwrap();
                if let Some(process) = state.process.as_ref().filter(|process| !process.exited) {
                    kill(process);
                }
            });
        }
    }

    // MARK: 分发

    fn handle_data(self: &Arc<Self>, data: &[u8]) {
        // == SDK：解不出来的消息只记日志
        match serde_json::from_slice::<Value>(data) {
            Ok(Value::Array(items)) => items.iter().filter(|item| item.is_object()).for_each(|item| self.handle_message(item)),
            Ok(message @ Value::Object(_)) => self.handle_message(&message),
            _ => {}
        }
    }

    fn handle_message(self: &Arc<Self>, message: &Value) {
        let has_method = message.get("method").is_some();
        let id = message.get("id").filter(|id| !id.is_null());
        if !has_method && (message.get("result").is_some() || message.get("error").is_some()) {
            let key = id.map_or(-1, request_key);
            let (sender, abort) = {
                let mut state = self.state.lock().unwrap();
                let sender = state.pending.remove(&key);
                // 响应已经到了：服务端不关 SSE 流时让两端主动断开（挂着的流会占住连接）
                let abort = sender.as_ref().and_then(|_| {
                    let post = state.posts.iter_mut().find(|(_, post)| post.request == Some(key));
                    post.map(|(post_id, post)| {
                        post.responded = true;
                        *post_id
                    })
                });
                (sender, abort)
            };
            let Some(sender) = sender else { return };
            let reply = match message.get("error") {
                Some(Value::Object(error)) => Err(Failure::new(describe_rpc_error(error))),
                _ => Ok(match message.get("result") {
                    Some(Value::Object(result)) => Value::Object(result.clone()),
                    _ => json!({}),
                }),
            };
            let _ = sender.send(reply);
            if let Some(post) = abort {
                events::emit("mcpClient.httpAbort", json!({ "client": self.id, "post": post }));
            }
            return;
        }
        if has_method && let Some(id) = id {
            // 服务端发来的请求：ping 回空结果；其余 == SDK 没有注册处理器时回 methodNotFound
            let method = message.get("method").and_then(Value::as_str).unwrap_or("");
            let reply = if method == "ping" {
                json!({ "jsonrpc": "2.0", "id": id, "result": {} })
            } else {
                let detail = format!("Unknown method: {method}");
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Method not found: {detail}"),
                    "data": { "detail": detail } } })
            };
            let mode = self.state.lock().unwrap().mode;
            match mode {
                Mode::Stdio => {
                    self.write_line(&reply);
                }
                Mode::Http => self.post(&reply, None, None),
                Mode::None => {}
            }
        }
        // 通知：忽略
    }

    fn fail_all(&self, message: &str) {
        let waiting: Vec<Sender<Reply>> = {
            let mut state = self.state.lock().unwrap();
            state.calls.clear();
            state.pending.drain().map(|(_, sender)| sender).collect()
        };
        for sender in waiting {
            let _ = sender.send(Err(Failure::new(message)));
        }
    }

    // MARK: stdio

    fn write_line(&self, message: &Value) -> bool {
        let mut line = message.to_string().into_bytes();
        line.push(b'\n');
        let mut stdin = self.stdin.lock().unwrap();
        match stdin.as_mut() {
            Some(pipe) => pipe.write_all(&line).and_then(|_| pipe.flush()).is_ok(),
            None => false,
        }
    }

    fn read_stdout(self: &Arc<Self>, stdout: impl Read) {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = line.strip_suffix(b"\n").unwrap_or(&line);
                    let text = text.strip_suffix(b"\r").unwrap_or(text);
                    if !text.is_empty() {
                        self.handle_data(text);
                    }
                }
            }
        }
    }

    fn read_stderr(self: &Arc<Self>, mut stderr: impl Read) {
        let mut decoder = StderrDecoder::default();
        let mut buffer = [0u8; 8192];
        loop {
            match stderr.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let text = decoder.decode(&buffer[..count]);
                    if !text.is_empty() {
                        events::emit("mcpClient.stderr", json!({ "client": self.id, "text": text }));
                    }
                }
            }
        }
    }

    fn wait_for_exit(self: &Arc<Self>, mut child: Child, readers_done: mpsc::Receiver<()>) {
        let status = child.wait();
        // 先等管道里剩下的读完（stderr 末尾是连接失败时最有用的信息），再让待决请求失败；子孙进程还拿着管道时不等太久
        let until = Instant::now() + Duration::from_millis(500);
        for _ in 0..2 {
            if readers_done.recv_timeout(until.saturating_duration_since(Instant::now())).is_err() {
                break;
            }
        }
        let reason = match status {
            Ok(status) => exit_reason(status),
            Err(error) => error.to_string(),
        };
        let ours = {
            let mut state = self.state.lock().unwrap();
            if let Some(process) = state.process.as_mut() {
                process.running = false;
                process.exited = true;
            }
            state.connected = false;
            state.closed
        };
        self.stdin.lock().unwrap().take();
        // 进程自己退出的（我们断开的不算）：不用等到超时，立即以退出原因失败，并告诉两端
        if !ours {
            events::emit("mcpClient.exited", json!({ "client": self.id, "reason": reason }));
            self.fail_all(&reason);
        }
    }

    // MARK: Streamable HTTP（发请求在两端）

    fn post(&self, message: &Value, request: Option<i64>, done: Option<Sender<Result<(), Failure>>>) {
        let (post, url, headers) = {
            let mut state = self.state.lock().unwrap();
            let post = state.next_post;
            state.next_post += 1;
            state.posts.insert(post, Post { request, done, ..Post::default() });
            let mut headers: Vec<(String, String)> =
                vec![("Accept".into(), "application/json, text/event-stream".into()), ("Content-Type".into(), "application/json".into())];
            if !state.protocol_version.is_empty() {
                headers.push(("MCP-Protocol-Version".into(), state.protocol_version.clone()));
            }
            if !state.session_id.is_empty() {
                headers.push(("MCP-Session-Id".into(), state.session_id.clone()));
            }
            // 配置里的请求头（令牌等）；和上面的重名时以配置为准（== requestModifier 在 SDK 设好请求头之后执行）
            for (key, value) in &state.headers {
                headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(key));
                headers.push((key.clone(), value.clone()));
            }
            (post, state.endpoint.clone(), headers)
        };
        let headers: Vec<Value> = headers.into_iter().map(|(key, value)| json!([key, value])).collect();
        events::emit(
            "mcpClient.httpPost",
            json!({ "client": self.id, "post": post, "url": url, "headers": headers, "body": message.to_string() }),
        );
    }

    /// 两端：响应头到了
    pub fn http_head(&self, post: u64, status: i64, content_type: &str, session_id: &str) {
        let mut state = self.state.lock().unwrap();
        let had_session = !state.session_id.is_empty();
        // == SDK：先记下会话 id，再检查状态码
        if !session_id.is_empty() {
            state.session_id = session_id.to_string();
        }
        let status_error = http_status_error(status, content_type, had_session);
        if status == 404 && had_session {
            state.session_id.clear();
        }
        let Some(entry) = state.posts.get_mut(&post) else { return };
        entry.headers_handled = true;
        entry.status_error = status_error;
        if entry.status_error.is_empty() {
            entry.is_sse = content_type.contains("text/event-stream");
            entry.is_json = !entry.is_sse && content_type.contains("application/json");
        }
    }

    /// 两端：一块响应体
    pub fn http_data(self: &Arc<Self>, post: u64, data: &[u8]) {
        let messages = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.posts.get_mut(&post) else { return };
            if !entry.status_error.is_empty() || !(entry.is_sse || entry.is_json) {
                return;
            }
            entry.buffer.extend_from_slice(data);
            if entry.is_sse { process_sse(entry, false) } else { Vec::new() }
        };
        for message in messages {
            self.handle_data(&message);
        }
    }

    /// 两端：这个 POST 结束了。error：网络层错误（连不上、TLS…）；aborted：我们让断开的（httpAbort）或两端取消的
    pub fn http_end(self: &Arc<Self>, post: u64, error: Option<&str>, aborted: bool) {
        let Some(mut entry) = self.state.lock().unwrap().posts.remove(&post) else { return };
        let mut failure = String::new();
        let mut messages = Vec::new();
        if aborted {
            if !entry.responded {
                failure = CLIENT_DISCONNECTED.to_string();
            }
        } else if let Some(error) = error.filter(|_| !entry.headers_handled) {
            failure = error.to_string();
        } else if !entry.status_error.is_empty() {
            failure = entry.status_error.clone();
        } else if let Some(error) = error {
            failure = error.to_string();
        } else if entry.is_sse {
            messages = process_sse(&mut entry, true);
        } else if entry.is_json {
            messages.push(std::mem::take(&mut entry.buffer));
        }
        for message in messages {
            self.handle_data(&message);
        }
        if let Some(request) = entry.request {
            // 请求结束了还没等到响应：出错时带上原因；否则按连接已关闭处理（SDK 会一直等到超时）
            let sender = self.state.lock().unwrap().pending.remove(&request);
            if let Some(sender) = sender {
                let _ = sender.send(Err(Failure::new(if failure.is_empty() { CONNECTION_CLOSED.to_string() } else { failure.clone() })));
            }
        }
        if let Some(done) = entry.done {
            let _ = done.send(if failure.is_empty() { Ok(()) } else { Err(Failure::new(failure)) });
        }
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name(name.to_string()).spawn(body);
}

/// JSON-RPC id → 我们发出的整数 id；不是我们的格式返回 -1
fn request_key(id: &Value) -> i64 {
    match id {
        Value::Number(number) => number.as_i64().unwrap_or(-1),
        Value::String(text) => text.parse().unwrap_or(-1),
        _ => -1,
    }
}

/// 逐行解析 SSE：data: 行累积，空行结束一个事件（data 以 \n 连接），其他字段忽略。返回解出的消息
fn process_sse(post: &mut Post, at_end: bool) -> Vec<Vec<u8>> {
    let mut messages = Vec::new();
    while let Some(newline) = post.buffer.iter().position(|&byte| byte == b'\n') {
        let mut line: Vec<u8> = post.buffer.drain(..=newline).collect();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        sse_line(post, &line, &mut messages);
    }
    if at_end {
        if !post.buffer.is_empty() {
            let mut line = std::mem::take(&mut post.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            sse_line(post, &line, &mut messages);
        }
        sse_line(post, &[], &mut messages);
    }
    messages
}

fn sse_line(post: &mut Post, line: &[u8], messages: &mut Vec<Vec<u8>>) {
    if line.is_empty() {
        if !post.event_data.is_empty() {
            let data = std::mem::take(&mut post.event_data).join(&b'\n');
            if !data.is_empty() {
                messages.push(data);
            }
        }
        return;
    }
    if let Some(value) = line.strip_prefix(b"data:") {
        post.event_data.push(value.strip_prefix(b" ").unwrap_or(value).to_vec());
    }
    // event: / id: / retry: / 注释行：不需要
}

/// JSON-RPC error 对象 → SDK 解码成 MCPError 后的 errorDescription
pub fn describe_rpc_error(error: &Map<String, Value>) -> String {
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
    let message = error.get("message").and_then(Value::as_str).unwrap_or("").to_string();
    let data = error.get("data").and_then(Value::as_object);
    // == MCPError.init(from:) 里的 unwrapDetail：data.detail 是字符串就用它，否则用后备值
    let detail =
        |fallback: Option<String>| data.and_then(|data| data.get("detail")).and_then(Value::as_str).map(str::to_string).or(fallback);
    let with_detail = |title: &str, detail: Option<String>| match detail {
        Some(detail) => format!("{title}: {detail}"),
        None => title.to_string(),
    };
    match code {
        -32700 => with_detail("Parse error: Invalid JSON", detail(Some(message))),
        -32600 => with_detail("Invalid Request", detail(Some(message))),
        -32601 => with_detail("Method not found", detail(Some(message))),
        -32602 => with_detail("Invalid params", detail(Some(message))),
        -32603 => with_detail("Internal error", detail(None)),
        -32042 => format!("URL elicitation required: {message}"),
        -32000 => CONNECTION_CLOSED.to_string(),
        -32001 => {
            let underlying = data.and_then(|data| data.get("error")).and_then(Value::as_str).map(str::to_string);
            format!("Transport error: {}", underlying.unwrap_or(message))
        }
        _ => format!("Server error: {message}"),
    }
}

/// HTTP 状态码 → SDK HTTPClientTransport.processHTTPResponse 抛出的错误文字（2xx 返回空）
pub fn http_status_error(status: i64, content_type: &str, had_session: bool) -> String {
    match status {
        200..=299 => String::new(),
        400 => "Internal error: Bad request".into(),
        401 => "Internal error: Authentication required".into(),
        403 => "Internal error: Access forbidden".into(),
        404 if had_session => "Internal error: Session expired".into(),
        404 => "Internal error: Endpoint not found".into(),
        // Swift 用的是 streaming = true 的默认初始化
        405 => "Internal error: Server does not support streaming".into(),
        408 => "Internal error: Request timeout".into(),
        429 => "Internal error: Too many requests".into(),
        500..=599 => format!("Internal error: Server error: {status}"),
        _ => format!("Internal error: Unexpected HTTP response: {status} ({content_type})"),
    }
}

// MARK: - 工具和结果

/// tools/list 里的一个工具：{name, title?, description?, inputSchema, annotationsTitle?, readOnlyHint?}；
/// inputSchema 不是对象时换成 {"type":"object","properties":{}}
pub fn normalize_tool(tool: &Value) -> Value {
    let string = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_string);
    let mut out = Map::new();
    out.insert("name".into(), json!(string(tool.get("name")).unwrap_or_default()));
    if let Some(title) = string(tool.get("title")) {
        out.insert("title".into(), json!(title));
    }
    if let Some(description) = string(tool.get("description")) {
        out.insert("description".into(), json!(description));
    }
    let schema = match tool.get("inputSchema") {
        Some(schema @ Value::Object(_)) => schema.clone(),
        _ => json!({ "type": "object", "properties": {} }),
    };
    out.insert("inputSchema".into(), schema);
    let annotations = tool.get("annotations");
    if let Some(title) = string(annotations.and_then(|annotations| annotations.get("title"))) {
        out.insert("annotationsTitle".into(), json!(title));
    }
    if let Some(hint) = annotations.and_then(|annotations| annotations.get("readOnlyHint")).and_then(Value::as_bool) {
        out.insert("readOnlyHint".into(), json!(hint));
    }
    Value::Object(out)
}

struct Mismatch;

fn absent(value: Option<&Value>) -> bool {
    value.is_none_or(Value::is_null)
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, Mismatch> {
    object.get(key).and_then(Value::as_str).map(str::to_string).ok_or(Mismatch)
}

/// == decodeIfPresent(String.self)：缺失或 null 是 None，其他类型失败
fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, Mismatch> {
    match object.get(key) {
        value if absent(value) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        _ => Err(Mismatch),
    }
}

/// == decodeIfPresent(Metadata.self)：只要求是对象
fn check_metadata(object: &Map<String, Value>, key: &str) -> Result<(), Mismatch> {
    let value = object.get(key);
    if absent(value) || value.is_some_and(Value::is_object) { Ok(()) } else { Err(Mismatch) }
}

/// == decodeIfPresent(Resource.Annotations.self)：audience [user|assistant]?、priority Double?、lastModified String?
fn check_annotations(object: &Map<String, Value>) -> Result<(), Mismatch> {
    let value = object.get("annotations");
    if absent(value) {
        return Ok(());
    }
    let annotations = value.and_then(Value::as_object).ok_or(Mismatch)?;
    let audience = annotations.get("audience");
    if !absent(audience) {
        let roles = audience.and_then(Value::as_array).ok_or(Mismatch)?;
        if !roles.iter().all(|role| role == "user" || role == "assistant") {
            return Err(Mismatch);
        }
    }
    let priority = annotations.get("priority");
    if !absent(priority) && !priority.is_some_and(Value::is_number) {
        return Err(Mismatch);
    }
    optional_string(annotations, "lastModified")?;
    Ok(())
}

/// tools/call 结果里的一段内容（== SDK 的 Tool.Content.init(from:)，一样严格）→
/// {kind: text|image|audio|resource|resource_link, text?, data?, mimeType?, uri?, name?, resourceText?}
fn decode_content(object: &Map<String, Value>) -> Result<Value, Mismatch> {
    let kind = required_string(object, "type")?;
    let mut out = Map::new();
    match kind.as_str() {
        "text" => {
            out.insert("text".into(), json!(required_string(object, "text")?));
            check_annotations(object)?;
            check_metadata(object, "_meta")?;
        }
        "image" | "audio" => {
            out.insert("data".into(), json!(required_string(object, "data")?));
            out.insert("mimeType".into(), json!(required_string(object, "mimeType")?));
            check_annotations(object)?;
            check_metadata(object, "_meta")?;
        }
        "resource" => {
            // == Resource.Content：uri 必填，mimeType / text / blob 可选
            let resource = object.get("resource").and_then(Value::as_object).ok_or(Mismatch)?;
            out.insert("uri".into(), json!(required_string(resource, "uri")?));
            out.insert("mimeType".into(), json!(optional_string(resource, "mimeType")?.unwrap_or_default()));
            if let Some(text) = optional_string(resource, "text")? {
                out.insert("resourceText".into(), json!(text));
            }
            optional_string(resource, "blob")?;
            check_metadata(resource, "_meta")?;
            check_annotations(object)?;
            check_metadata(object, "_meta")?;
        }
        "resource_link" => {
            out.insert("uri".into(), json!(required_string(object, "uri")?));
            out.insert("name".into(), json!(required_string(object, "name")?));
            optional_string(object, "title")?;
            optional_string(object, "description")?;
            out.insert("mimeType".into(), json!(optional_string(object, "mimeType")?.unwrap_or_default()));
            check_annotations(object)?;
        }
        _ => return Err(Mismatch),
    }
    out.insert("kind".into(), json!(kind));
    Ok(Value::Object(out))
}

/// == CallTool.Result.init(from:)：content 必填（[Tool.Content]），isError Bool?，_meta 对象 → {content, isError?}
fn decode_call_result(result: &Value) -> Result<Value, Mismatch> {
    let object = result.as_object().ok_or(Mismatch)?;
    let items = object.get("content").and_then(Value::as_array).ok_or(Mismatch)?;
    let content = items.iter().map(|item| item.as_object().ok_or(Mismatch).and_then(decode_content)).collect::<Result<Vec<_>, _>>()?;
    let mut out = json!({ "content": content });
    match object.get("isError") {
        value if absent(value) => {}
        // JSONDecoder 不把 0 / 1 当 Bool
        Some(Value::Bool(flag)) => out["isError"] = json!(flag),
        _ => return Err(Mismatch),
    }
    check_metadata(object, "_meta")?;
    Ok(out)
}

/// 公开给测试和两端：结果解码（解不出来是 None）
pub fn decode_result(result: &Value) -> Option<Value> {
    decode_call_result(result).ok()
}

/// 一段内容按 SDK 的 Tool.Content 解码（解不出来是 None）
pub fn decode_one(content: &Value) -> Option<Value> {
    content.as_object().and_then(|object| decode_content(object).ok())
}

// MARK: - 进程

#[cfg(unix)]
fn detach_from_terminal(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // 新会话：没有控制终端，终端上的 Ctrl+C（SIGINT 发给前台进程组）只交给命令行的 friday 处理（== Swift 从访达启动的情形）
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

#[cfg(windows)]
fn detach_from_terminal(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // 图形界面程序起控制台程序（cmd.exe、node）时不要弹出黑窗口；新进程组：命令行 friday 所在控制台上的 Ctrl+C 不会连带结束 MCP server
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach_from_terminal(_command: &mut Command) {}

/// == Process.terminate()（SIGTERM）；Windows 的控制台程序不理 WM_CLOSE，等 3 秒后的强制结束
fn terminate(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn kill(process: &Process) {
    #[cfg(unix)]
    unsafe {
        libc::kill(process.pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(windows)]
    windows::kill(process);
}

/// == Swift：terminationReason == .uncaughtSignal ? 「服务进程被信号 N 结束」 : 「服务进程已退出（退出码 N）」
fn exit_reason(status: std::process::ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("服务进程被信号 {signal} 结束");
        }
    }
    let code = status.code().unwrap_or(-1);
    #[cfg(windows)]
    {
        // Windows 上没有信号：异常结束时退出码是异常代码（如 0xC0000005）
        if (code as u32) >= 0xC000_0000 {
            return format!("服务进程异常结束（0x{:X}）", code as u32);
        }
    }
    format!("服务进程已退出（退出码 {code}）")
}

/// stderr 的解码：按 UTF-8，块末尾被截断的字符留到下一块；Windows 上某块不是合法 UTF-8（中文系统上 uvx 起的 Python、cmd.exe 的
/// 报错用 ANSI 代码页）改按系统代码页解；其他平台照 Swift 的 String(decoding:as: UTF8.self) 把坏字节换成 �
#[derive(Default)]
struct StderrDecoder {
    pending: Vec<u8>,
}

impl StderrDecoder {
    fn decode(&mut self, bytes: &[u8]) -> String {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(bytes);
        match std::str::from_utf8(&data) {
            Ok(text) => text.to_string(),
            Err(error) if error.error_len().is_none() => {
                // 只是末尾不完整
                let valid = error.valid_up_to();
                self.pending = data[valid..].to_vec();
                String::from_utf8_lossy(&data[..valid]).into_owned()
            }
            Err(_) => {
                #[cfg(windows)]
                {
                    windows::decode_system(&data)
                }
                #[cfg(not(windows))]
                {
                    String::from_utf8_lossy(&data).into_owned()
                }
            }
        }
    }
}

#[cfg(windows)]
mod windows {
    //! Windows：作业对象（server 进程连同子孙进程一起结束）、按系统代码页解码
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    type Handle = *mut core::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimit {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        values: [u64; 6],
    }

    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const PROCESS_TERMINATE: u32 = 0x0001;
    const LOCALE_IDEFAULTANSICODEPAGE: u32 = 0x1004;
    const LOCALE_RETURN_NUMBER: u32 = 0x2000_0000;
    const CP_UTF8: u32 = 65001;

    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *mut core::ffi::c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *mut core::ffi::c_void, length: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, code: u32) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn TerminateProcess(process: Handle, code: u32) -> i32;
        fn GetLocaleInfoEx(locale: *const u16, kind: u32, data: *mut u16, length: i32) -> i32;
        fn GetACP() -> u32;
        fn MultiByteToWideChar(page: u32, flags: u32, text: *const u8, length: i32, wide: *mut u16, wide_length: i32) -> i32;
    }

    /// 「句柄关闭即结束」的作业对象
    pub struct Job(Handle);

    // 句柄只在拿着 State 的锁时用
    unsafe impl Send for Job {}

    impl Job {
        pub fn attach(child: &Child) -> Option<Job> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if handle.is_null() {
                    return None;
                }
                let mut info = ExtendedLimit::default();
                info.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    handle,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &mut info as *mut ExtendedLimit as *mut core::ffi::c_void,
                    std::mem::size_of::<ExtendedLimit>() as u32,
                );
                if AssignProcessToJobObject(handle, child.as_raw_handle() as Handle) == 0 {
                    CloseHandle(handle);
                    return None;
                }
                Some(Job(handle))
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub fn kill(process: &super::Process) {
        unsafe {
            if let Some(job) = &process.job {
                TerminateJobObject(job.0, 1);
            }
            let handle = OpenProcess(PROCESS_TERMINATE, 0, process.pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }

    /// 系统（不是本进程）的 ANSI 代码页：friday.exe 嵌了 activeCodePage=UTF-8 清单，本进程的 ANSI 代码页成了 UTF-8，
    /// 而起出来的 server（Python、cmd.exe）没有这个清单，照旧按系统代码页写
    fn system_code_page() -> u32 {
        // LOCALE_NAME_SYSTEM_DEFAULT = "!x-sys-default-locale"
        let name: Vec<u16> = "!x-sys-default-locale".encode_utf16().chain(std::iter::once(0)).collect();
        let mut value: u32 = 0;
        let read = unsafe {
            GetLocaleInfoEx(name.as_ptr(), LOCALE_IDEFAULTANSICODEPAGE | LOCALE_RETURN_NUMBER, &mut value as *mut u32 as *mut u16, 2)
        };
        if read > 0 && value != 0 { value } else { unsafe { GetACP() } }
    }

    pub fn decode_system(data: &[u8]) -> String {
        let page = system_code_page();
        if page == CP_UTF8 || data.is_empty() {
            return String::from_utf8_lossy(data).into_owned();
        }
        unsafe {
            let length = MultiByteToWideChar(page, 0, data.as_ptr(), data.len() as i32, std::ptr::null_mut(), 0);
            if length <= 0 {
                return data.iter().map(|&byte| byte as char).collect();
            }
            let mut wide = vec![0u16; length as usize];
            MultiByteToWideChar(page, 0, data.as_ptr(), data.len() as i32, wide.as_mut_ptr(), length);
            String::from_utf16_lossy(&wide)
        }
    }
}

// MARK: - base64（两端交来的 HTTP 响应体）

pub fn decode_base64(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some((byte - b'A') as u32),
            b'a'..=b'z' => Some((byte - b'a' + 26) as u32),
            b'0'..=b'9' => Some((byte - b'0' + 52) as u32),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut accumulator = 0u32;
        for (index, &byte) in chunk.iter().enumerate() {
            accumulator |= value(byte)? << (18 - 6 * index);
        }
        let produced = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        for index in 0..produced {
            out.push((accumulator >> (16 - 8 * index)) as u8);
        }
    }
    Some(out)
}

// MARK: - 同步：配置变了哪些要重连

/// 按新配置算哪些服务要断开、哪些要（重新）连接（== 两端 MCPManager.sync 的前半段）：
/// - 新配置里没有的：断开；
/// - 连接配置（停用、信任之外的部分）和停用都没变、也不在 force 里的：留着（只换配置里的信任）；
/// - 其余：断开旧的，停用的标成停用，没停用的重新连接
///
/// current：[{name, config}]；next：{名字: config}。返回 {detach: [名字], keep: [名字], connect: [名字], disabled: [名字]}（都按名字排好）
pub fn sync_plan(current: &[(String, Value)], next: &Map<String, Value>, force: &[String]) -> Value {
    let connection = |config: &Value| {
        let mut copy = config.as_object().cloned().unwrap_or_default();
        copy.remove("disabled");
        copy.remove("autoApprove");
        Value::Object(copy)
    };
    let disabled = |config: &Value| config.get("disabled").and_then(Value::as_bool) == Some(true);
    let mut detach: Vec<&str> = current.iter().filter(|(name, _)| !next.contains_key(name)).map(|(name, _)| name.as_str()).collect();
    let (mut keep, mut connect, mut off) = (Vec::new(), Vec::new(), Vec::new());
    let mut names: Vec<&String> = next.keys().collect();
    names.sort();
    for name in names {
        let config = &next[name];
        let existing = current.iter().find(|(current, _)| current == name);
        if let Some((_, old)) = existing
            && connection(old) == connection(config)
            && disabled(old) == disabled(config)
            && !force.contains(name)
        {
            keep.push(name.as_str());
            continue;
        }
        detach.push(name.as_str());
        if disabled(config) {
            off.push(name.as_str());
        } else {
            connect.push(name.as_str());
        }
    }
    detach.sort();
    json!({ "detach": detach, "keep": keep, "connect": connect, "disabled": off })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_errors_read_like_sdk() {
        let error = |value: Value| describe_rpc_error(value.as_object().unwrap());
        assert_eq!(error(json!({ "code": -32602, "message": "bad arguments" })), "Invalid params: bad arguments");
        assert_eq!(
            error(json!({ "code": -32601, "message": "x", "data": { "detail": "Unknown tool: t" } })),
            "Method not found: Unknown tool: t"
        );
        assert_eq!(error(json!({ "code": -32603, "message": "x" })), "Internal error");
        assert_eq!(error(json!({ "code": -32000, "message": "x" })), "Connection closed");
        assert_eq!(error(json!({ "code": -32001, "message": "m", "data": { "error": "e" } })), "Transport error: e");
        assert_eq!(error(json!({ "code": 7, "message": "oops" })), "Server error: oops");
        assert_eq!(http_status_error(404, "text/plain", true), "Internal error: Session expired");
        assert_eq!(http_status_error(404, "text/plain", false), "Internal error: Endpoint not found");
        assert_eq!(http_status_error(502, "", false), "Internal error: Server error: 502");
        assert_eq!(http_status_error(302, "text/html", false), "Internal error: Unexpected HTTP response: 302 (text/html)");
        assert!(http_status_error(202, "", false).is_empty());
    }

    #[test]
    fn results_decode_strictly() {
        let ok = decode_result(&json!({ "content": [{ "type": "text", "text": "hi" }, { "type": "image", "data": "AA==", "mimeType": "image/png" },
            { "type": "resource", "resource": { "uri": "file:///a", "text": "body" } }, { "type": "resource_link", "uri": "u", "name": "n" }],
            "isError": false }))
        .unwrap();
        assert_eq!(ok["content"][0], json!({ "kind": "text", "text": "hi" }));
        assert_eq!(ok["content"][1]["mimeType"], "image/png");
        assert_eq!(ok["content"][2]["resourceText"], "body");
        assert_eq!(ok["content"][3]["name"], "n");
        assert_eq!(ok["isError"], false);
        for bad in [
            json!({ "isError": false }),
            json!({ "content": [{ "type": "video" }] }),
            json!({ "content": [{ "type": "text" }] }),
            json!({ "content": [], "isError": 1 }),
            json!({ "content": [{ "type": "text", "text": "x", "annotations": { "audience": ["robot"] } }] }),
            json!({ "content": [], "_meta": 3 }),
        ] {
            assert!(decode_result(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn tools_are_lenient() {
        let tool = normalize_tool(&json!({ "name": "t", "inputSchema": 3, "annotations": { "title": "T", "readOnlyHint": true } }));
        assert_eq!(
            tool,
            json!({ "name": "t", "inputSchema": { "type": "object", "properties": {} }, "annotationsTitle": "T", "readOnlyHint": true })
        );
    }

    #[test]
    fn sse_parses_events() {
        let mut post = Post {
            buffer: b": keep-alive\n\nevent: message\ndata: {\"a\":\ndata: 1}\r\n\r\nid: 7\ndata: {\"b\":2}".to_vec(),
            ..Post::default()
        };
        let messages = process_sse(&mut post, false);
        assert_eq!(messages, vec![b"{\"a\":\n1}".to_vec()]);
        assert_eq!(process_sse(&mut post, true), vec![b"{\"b\":2}".to_vec()]);
    }

    #[test]
    fn base64_round_trip() {
        assert_eq!(decode_base64("aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
        assert_eq!(decode_base64("5Lit5paH").unwrap(), "中文".as_bytes());
        assert_eq!(decode_base64("").unwrap(), b"");
        assert!(decode_base64("a").is_none());
    }

    #[test]
    fn stderr_keeps_split_characters() {
        let mut decoder = StderrDecoder::default();
        let bytes = "错误".as_bytes();
        assert_eq!(decoder.decode(&bytes[..2]), "");
        assert_eq!(decoder.decode(&bytes[2..]), "错误");
    }

    #[test]
    fn sync_plan_reconnects_only_changes() {
        let current = vec![
            ("a".to_string(), json!({ "command": "x" })),
            ("b".to_string(), json!({ "command": "y", "autoApprove": false })),
            ("c".to_string(), json!({ "command": "z" })),
            ("gone".to_string(), json!({ "command": "w" })),
        ];
        let next = json!({ "a": { "command": "x2" }, "b": { "command": "y", "autoApprove": true }, "c": { "command": "z", "disabled": true },
            "d": { "url": "http://h" } });
        let plan = sync_plan(&current, next.as_object().unwrap(), &[]);
        assert_eq!(plan, json!({ "detach": ["a", "c", "d", "gone"], "keep": ["b"], "connect": ["a", "d"], "disabled": ["c"] }));
        let forced = sync_plan(&current, next.as_object().unwrap(), &["b".to_string()]);
        assert_eq!(forced["connect"], json!(["a", "b", "d"]));
    }

    /// 真起一个 stdio server（sh 写的回声）：握手、tools/list、tools/call、服务端 ping、进程退出
    #[cfg(unix)]
    #[test]
    fn stdio_end_to_end() {
        let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) printf '{"jsonrpc":"2.0","method":"ping","id":"s1"}\n{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26"}}\n' "$id" ;;
    *'"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id" ;;
    *'"exit"'*) echo "要退出了" >&2; exit 3 ;;
    *'"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"pong"}]}}\n' "$id" ;;
  esac
done
"#;
        let client_id = open("Friday", "1.0");
        let client = get(client_id).unwrap();
        let environment: HashMap<String, String> = std::env::vars().collect();
        client.connect_stdio("/bin/sh", &["-c".to_string(), script.to_string()], &environment, "", 10_000, None).unwrap();
        assert!(client.is_connected());
        assert_eq!(client.state.lock().unwrap().protocol_version, "2025-03-26");
        let page = client.list_tools(None, 5000, None).unwrap();
        assert_eq!(page["tools"][0]["name"], "echo");
        let result = client.call_tool("echo", json!({}), 5000, None).unwrap();
        assert_eq!(result["content"][0]["text"], "pong");
        let error = client.call_tool("exit", json!({}), 5000, None).unwrap_err();
        assert_eq!(error.message, "服务进程已退出（退出码 3）");
        assert!(!client.process_running());
        let again = client.call_tool("echo", json!({}), 5000, None).unwrap_err();
        assert_eq!(again.message, CONNECTION_CLOSED);
        close(client_id);
        assert!(get(client_id).is_none());
    }

    /// 超时、取消、断开
    #[cfg(unix)]
    #[test]
    fn stdio_timeout_cancel_disconnect() {
        let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in *'"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;; esac
done"#;
        let client_id = open("Friday", "1.0");
        let client = get(client_id).unwrap();
        let environment: HashMap<String, String> = std::env::vars().collect();
        client.connect_stdio("/bin/sh", &["-c".to_string(), script.to_string()], &environment, "/tmp", 10_000, None).unwrap();
        let started = Instant::now();
        let error = client.list_tools(None, 1000, None).unwrap_err();
        assert_eq!(error.message, "操作超时（1 秒）");
        assert!(started.elapsed() < Duration::from_secs(3));
        let canceller = client.clone();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            canceller.cancel(42);
        });
        let error = client.call_tool("x", json!({}), 10_000, Some(42)).unwrap_err();
        assert!(error.cancelled);
        cancel.join().unwrap();
        let pid = client.pid().unwrap();
        let waiter = client.clone();
        let pending = std::thread::spawn(move || waiter.call_tool("x", json!({}), 10_000, None));
        std::thread::sleep(Duration::from_millis(200));
        client.disconnect();
        assert_eq!(pending.join().unwrap().unwrap_err().message, CLIENT_DISCONNECTED);
        // 断开后马上不算在运行；读到 EOF 自己退出 / SIGTERM：很快就没了
        assert!(!client.process_running());
        let until = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(unsafe { libc::kill(pid as libc::pid_t, 0) } != 0);
        close(client_id);
    }

    /// Streamable HTTP：两端（这里是测试里的事件回调）发请求、交回响应头和响应体；会话 id、协商的协议版本、SSE 回完不关流时让断开、
    /// 状态码错误、请求头里配置的令牌
    #[test]
    fn http_through_hosts() {
        let _guard = events::TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let aborted: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
        let (seen_in, aborted_in) = (seen.clone(), aborted.clone());
        events::set_handler(Some(Box::new(move |text: &str| {
            let message: Value = serde_json::from_str(text).unwrap();
            let data = message["data"].clone();
            match message["event"].as_str().unwrap() {
                "mcpClient.httpAbort" => {
                    let post = data["post"].as_u64().unwrap();
                    aborted_in.lock().unwrap().push(post);
                    let client = get(data["client"].as_u64().unwrap()).unwrap();
                    std::thread::spawn(move || client.http_end(post, None, true));
                }
                "mcpClient.httpPost" => {
                    seen_in.lock().unwrap().push(data.clone());
                    std::thread::spawn(move || {
                        let client = get(data["client"].as_u64().unwrap()).unwrap();
                        let post = data["post"].as_u64().unwrap();
                        let body: Value = serde_json::from_str(data["body"].as_str().unwrap()).unwrap();
                        let id = body["id"].clone();
                        let result = |value: Value| json!({ "jsonrpc": "2.0", "id": id, "result": value }).to_string();
                        match body["method"].as_str().unwrap_or("") {
                            "initialize" => {
                                client.http_head(post, 200, "application/json", "sess-1");
                                client.http_data(post, result(json!({ "protocolVersion": "2025-03-26" })).as_bytes());
                                client.http_end(post, None, false);
                            }
                            "notifications/initialized" => {
                                client.http_head(post, 202, "", "");
                                client.http_end(post, None, false);
                            }
                            "tools/list" => {
                                // SSE：先一条通知，再把响应拆成两块；回完不关（等核心让断开）
                                client.http_head(post, 200, "text/event-stream", "");
                                let response = result(json!({ "tools": [{ "name": "echo" }] }));
                                let (head, tail) = response.split_at(10);
                                client.http_data(post, b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n");
                                client.http_data(post, format!("data: {head}").as_bytes());
                                client.http_data(post, format!("{tail}\r\n\r\n").as_bytes());
                            }
                            _ if body["params"]["name"] == "gone" => {
                                client.http_head(post, 404, "text/plain", "");
                                client.http_end(post, None, false);
                            }
                            _ => {
                                client.http_head(post, 200, "application/json; charset=utf-8", "");
                                client.http_data(post, result(json!({ "content": [{ "type": "text", "text": "http" }] })).as_bytes());
                                client.http_end(post, None, false);
                            }
                        }
                    });
                }
                _ => {}
            }
        })));
        let client_id = open("Friday", "1.0");
        let client = get(client_id).unwrap();
        client
            .connect_http("http://h/mcp", vec![("Authorization".into(), "Bearer t".into()), ("accept".into(), "*/*".into())], 5000, None)
            .unwrap();
        let page = client.list_tools(None, 5000, None).unwrap();
        assert_eq!(page["tools"][0]["name"], "echo");
        let until = Instant::now() + Duration::from_secs(2);
        while aborted.lock().unwrap().is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(aborted.lock().unwrap().len(), 1, "SSE 回完让两端断开");
        assert_eq!(client.call_tool("echo", json!({}), 5000, None).unwrap()["content"][0]["text"], "http");
        assert_eq!(client.call_tool("gone", json!({}), 5000, None).unwrap_err().message, "Internal error: Session expired");
        let posts = seen.lock().unwrap().clone();
        let header = |post: &Value, name: &str| {
            post["headers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|pair| pair[0].as_str().unwrap().eq_ignore_ascii_case(name))
                .map(|pair| pair[1].clone())
        };
        assert_eq!(header(&posts[0], "MCP-Session-Id"), None);
        assert_eq!(header(&posts[0], "MCP-Protocol-Version"), Some(json!(PROTOCOL_VERSION)));
        assert_eq!(header(&posts[1], "MCP-Session-Id"), Some(json!("sess-1")));
        assert_eq!(header(&posts[1], "MCP-Protocol-Version"), Some(json!("2025-03-26")));
        assert_eq!(header(&posts[2], "Authorization"), Some(json!("Bearer t")));
        // 配置的请求头盖掉同名的（不分大小写）
        assert_eq!(header(&posts[2], "Accept"), Some(json!("*/*")));
        assert_eq!(
            posts[2]["headers"].as_array().unwrap().iter().filter(|pair| pair[0].as_str().unwrap().eq_ignore_ascii_case("accept")).count(),
            1
        );
        // 404 之后会话 id 清掉
        assert!(client.state.lock().unwrap().session_id.is_empty());
        close(client_id);
        events::set_handler(None);
    }

    #[test]
    fn spawn_failure_and_not_initialized() {
        let client_id = open("Friday", "1.0");
        let client = get(client_id).unwrap();
        assert_eq!(client.list_tools(None, 1000, None).unwrap_err().message, NOT_INITIALIZED);
        let error = client.connect_stdio("/nonexistent/friday-mcp", &[], &HashMap::new(), "", 1000, None).unwrap_err();
        assert!(!error.message.is_empty());
        close(client_id);
    }
}
