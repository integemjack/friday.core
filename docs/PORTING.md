# 移植计划

目标：Swift 版和 Qt 版界面以下的逻辑全部收进 friday.core。一次搬一块，每块搬完两端照常能发版。

## 规则

1. **Swift 为准**。搬的时候照 Swift 的实现写 Rust；Qt 和 Swift 不一致的地方记在这里，按 Swift。
2. **先搬纯逻辑，后搬有状态的**。纯函数（输入 → 输出）最好验证：两端原来的测试就是验收标准。
3. **两端的签名不动**，只换实现（见 ARCHITECTURE.md §3.3）。
4. **规则的测试跟着搬到 Rust**；两端原来的测试保留，检查包装层。
5. 每块单独提交（friday.core 一个提交，Friday 一个提交带子模块指针）。
6. 要用两端状态的判断（比如「注册表里有没有这个工具」）留在包装层，核心只收结果；不要为此给核心加回调。
7. 两端各自维护、核心要用的数据（比如库里的密钥值），在两端变化时交给核心（`secrets.update`），核心每个进程存一份；不要每次调用都带上。
8. 整段请求历史过 C 接口要编解码一次（估算 token、子 Agent 会话折叠每次请求都要）：核心这边 3 MB 约 1 ms，两端的 JSON 编码另算。
   到第 3 阶段在线引擎搬进核心后，历史就在核心里，不用再过来回。
9. 有状态的一块用句柄（`vault.open` → id，后面的方法带上，`vault.close` 关掉）：两端每个实例各持一个，界面要观察的东西每次改动后取快照。
   存档文件由核心读写，写法照两端原来的（`swift_json::pretty`：Swift JSONEncoder 的 pretty + 键排序），降级回旧版也读得了。
10. 热路径上的小函数（要对大量数据反复调、本身只有几行的：给整个工作目录算行指纹、界面每次画都要算的合计）可以两端各留一份，
    但核心也要有同样的实现，并在三边的测试里用同一组值对照。64 位的数过 C 接口写成十六进制字符串（JSON 的数字只有 53 位精度）。
11. 频繁、文件大的写盘交给核心的后台写盘（`writer`：同一个文件只写最新的那份），不在调用方的线程上写；两端退出前调 `core.flush`。
12. Swift 传 `JSONValue` 这类 Codable 值用 `CoreBridge.call(_:encoding:as:)`，不经过 `Any`（整数和小数不会混）。

## 顺序

| 阶段 | 内容 | 两端原来的位置 | 状态 |
|---|---|---|---|
| 0 | 仓库、C 接口、两端静态链接、CoreBridge | — | 完成 |
| 1 | 额度与限流：报错分类、恢复时间、定时试的间隔 | Swift `LLM/Engine/QuotaPolicy.swift`；Qt `llm/engine/QuotaPolicy.cpp` | 完成 |
| 1 | 工具名、参数名纠正（别家叫法换成 Friday 的；「注册表里有没有」留在两端） | `Agent/ToolAliases.swift`；`agent/ToolAliases.cpp` | 完成 |
| 1 | 密钥识别与替换（认、遮、清理存档、名字合不合法；密钥库本身留在两端的 SecretStore） | `Secrets/SecretGuard.swift`；`secrets/SecretGuard.cpp` | 完成 |
| 1 | 上下文折叠、会话折叠（摘要、子 Agent 会话的折叠、估算 token；主会话标折叠、调试用的预算开关留在两端） | `Agent/ContextFolding.swift`、`LLM/Engine/SessionFolding.swift`；`agent/ContextFolding.cpp`、`llm/engine/SessionFolding.cpp` | 完成 |
| 1 | 密钥库：存储、收进库、执行时换回明文（句柄；界面观察的名字和记录是快照，远程副本留在两端） | `Secrets/SecretStore.swift`；`secrets/SecretStore.cpp` | 完成 |
| 1 | 等额度的文案和时间（等到什么时候、消息上那一行、时间和「还有多久」的写法；停下、换大脑、定时检查留在两端） | `Agent/QuotaWaiting.swift`；`agent/QuotaWaiting.cpp` | 完成 |
| 2 | 会话数据模型（Conversation、ChatMessage、MessageBlock、ToolEvent……，JSON 和 Swift Codable 一致，认不出的字段原样保留）+ 读 conversations.json 时的规整（旧存档补父子链、上次中途退出的标成中断、按更新时间排） | `Models/ChatModels.swift`、`ConversationStore.swift` 的读入；`models/ChatModels.cpp`、`ConversationStore.cpp` 的 loadFromDisk | 完成（核心只把用到的字段定成类型，其余在 extra 里；本机 8 MB 真实存档往返一字不差：`cargo run --release --example check_archive <文件>`） |
| 2 | 改文件的增删行数（LineDiff：Myers 按全文 / 按指纹算、改动视图；分行和行指纹两端各留一份，热路径，三边有对照测试） | `Models/LineDiff.swift`；`models/LineDiff.cpp` | 完成 |
| 2 | 诊断日志（journal.json，整个存储，句柄，后台写盘）、引擎配置（engines.json 的读写格式） | `Agent/AgentJournal.swift`、`LLM/Engine/EngineStore.swift`；`agent/AgentJournal.cpp`、`llm/engine/EngineStore.cpp` | 完成 |
| 4 | 其余存档（capabilities.json、jobs.json、mcp.json、工具和提供方的清单）：读写和各自的注册表搅在一起（迁移、原文原样保存），注册表要看两端的状态、MCP 要事件回调，跟着各自的模块在第 4 阶段搬 | `Capabilities/`、`MCP/`、`Agent/ScriptTools`；`capabilities/`、`mcp/`、`agent/ScriptTools` | |
| 2 | 提示词引擎（解析、展开参数和引用、装到数据目录、按段三方合并、校验、改一段、恢复成自带的、临时换一套段） | `Prompts/Prompts.swift`；`util/Prompts.cpp` | 完成（两端导出的全部提示词和旧实现逐字一样：`FRIDAY_PROMPT_DUMP`） |
| 2 | 提示词版本历史里两端都要的规则（git 设置、git log 解成版本、提交说明、自动回滚的判断、上次提交时用的段） | `Prompts/PromptStore.swift`；`models/PromptStore.cpp` | 完成 |
| 4 | PromptStore 其余部分（每秒看文件、停一会儿再提交、跑 git、界面状态）；找 git 留在两端（Swift 由核心在 git 队列里请两端找，Qt 启动时同步找好交给核心） | `Prompts/PromptStore.swift`；`models/PromptStore.cpp` | 完成（Qt 原来用 QFileSystemWatcher，改成和 Swift 一样每秒看一次） |
| 3 | 工具定义：内置工具的清单（名字、标题、图标、权限、参数；Qt 图标库缺的三个给替代图标）、spec 拼法（说明取提示词 tool.<名字>.<参数>）、参数的 schema、调用前查必填参数 | `Agent/AgentTools.swift`、`ToolTypes.swift`、`ToolRegistry.swift` 的预检；`agent/AgentTools.cpp`、`ToolTypes.cpp`、`ToolRegistry.cpp` | 完成（ToolRegistry 其余的调度、元工具、脚本工具要看两端的状态，第 4 阶段） |
| 3 | 模型特征（档位、计费、是不是对话模型）和引擎的推断规则（思考方式、接口自带的思考开关、是不是本机、要不要带回思考、并行调工具、可选的思考档位） | `LLM/Engine/ModelTraits.swift`、`EngineStore.swift`；`llm/engine/ModelTraits.cpp`、`EngineStore.cpp` | 完成（`models.traits`、`engines.rules`；词表和地址规则只剩核心一份） |
| 3 | 流式解析（句柄：每个 chunk 的负载喂一次、吐出要交给界面的文字；工具调用拼接、复读检测 Repetition、被掐断的判断、流里的额度和限流；SSE 分行和 [DONE] 属于网络传输，留在两端） | `LLM/Engine/OpenAIChatEngine.swift` 的 readStream、`Repetition.swift`；`llm/engine/OpenAIChatEngine.cpp` 的 processChunk、`Repetition.cpp` | 完成（负载原样拼进 `{"chunk":…,"stream":N}` 不用再编码；复读搜索加了下界，结果和原算法对照一致，1.4 MB 正文时每个 chunk 从 343 µs 降到 19 µs：`cargo run --release --example bench_stream [个数]`） |
| 3 | 请求体拼装（含遮密钥）、不接受的参数（认报错 + 按引擎记住）、重试和限流的等待、上下文超长的报错解析；发请求、闸门、退避计时、网络层临时故障的认法（URLError / QNetworkReply）、Qt 的 errorText 留在两端 | 同上 | 完成（Swift 请求体直接用核心给的字节当 httpBody，比原来少编解码一遍） |
| 3 | 限流排队的自适应规则（RequestGate：名额、排队先后、并发上限怎么变；排着的请求怎么等——Swift 的 continuation、Qt 的 Promise——留在两端，按编号由核心说放行哪几个）、按上下文上限压历史（ContextFit，连同各模型的上限、估算校准）、上下文进度环（ContextUsage：估算、缩放、上限、数字写法、服务商报的上限；问服务商的网络请求和「问过没有」留在两端） | `LLM/Engine/RequestGate.swift`、`ContextFit.swift`、`ContextUsage.swift`；`llm/engine/RequestGate.cpp`、`ContextFit.cpp`、`ContextUsage.cpp` | 完成（Qt 的 ContextUsageView.qml 里还有一份 JS 的数字写法，界面上的，留着） |
| 3 | 网络传输（HTTP、SSE）：先留在两端——URLSession / Qt 走系统代理，Rust 的 HTTP 库默认只认环境变量里的代理 | 同上 | 暂不搬 |
| 3 | 工具执行：列目录、读写文件、改文件（FileEdit）、找文件和搜内容（FileSearch）、路径提示、命令前后的改动（FileChanges，扫描和读文件都在核心）、run_shell 的规则（超时、拒绝结束自己、结果后的提示、结果写法）、截断 / 分页 / 去 HTML；起进程、联网（web_search、fetch_url）、读媒体文件、会话改动的原文留在两端 | `Agent/AgentTools.swift`、`FileEdit.swift`、`FileSearch.swift`、`FileChanges.swift`；`agent/AgentTools.cpp`、`ToolTypes.cpp`（ToolFiles）、`FileSearch.cpp`、`FileChanges.cpp` | 完成（Qt 的 FileEdit 删了；报错文案照 Qt 逐字复刻的 Cocoa 中文说明；结束 Friday 的规则用 Qt 那份（多认 friday-server / friday-headless）；去 HTML 时 &amp; 放最后解） |
| 4 | MCP（起 server 进程、JSON-RPC、通知、重连；Swift 用 MCP SDK、Qt 自己写的客户端）：有状态、要事件回调 | `MCP/`；`mcp/` | |
| 4 | 事件回调：核心推事件、请两端做事再等回话（`friday_core_set_event_handler`，ABI 2；见 ARCHITECTURE.md §2.3） | `Core/CoreEvents.swift`；`util/CoreBridge` | 完成 |
| 4 | 会话存储（增删改、分叉、落盘防抖，界面观察的快照） | `Models/ConversationStore.swift`；`models/ConversationStore.cpp` | |
| 4 | 引擎的对话循环（请求 → 流式 → 工具调用 → 续写、复读处理、收尾）：发 HTTP、跑工具经「请两端做事」 | `LLM/Engine/OpenAIChatEngine.swift` 的 run；`llm/engine/OpenAIChatEngine.cpp` | |
| 4 | 智能体循环（AgentRunner：一轮的调度、审批、恢复、看门狗）、子智能体、工作流、团队调度 | `Agent/AgentRunner.swift`、`SubAgentRunner.swift`、`Workflow.swift`、`AgentTeamControl.swift`；`agent/` | |
| 5 | 远程服务端（照 REMOTE.md） | Qt `remote/` | |

**会话存储本身（ConversationStore：增删改、分叉、落盘的防抖）和智能体循环一起搬（第 4 阶段）**：流式生成时每个字都改一次正在写的消息，
存储先搬进核心的话每个字都要过一次 C 接口。第 2 阶段先把数据模型和读入规整搬过来，两端照旧在内存里改、自己落盘。

界面、平台相关的部分（SwiftUI / QML、终端画面、浏览器、通话的音视频采集、系统托盘、自动更新换包）留在两端。
