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
| 4 | 其余存档：capabilities.json（三组提供方分开存、disabledKinds 写空数组、旧版按能力整体关的交给两端迁移）、jobs.json（只留 50 个）、mcp.json、脚本工具（tool.json 的默认值、存新版本归档旧版、历史版本、运行摘要和产出文件）、脚本提供方的包（provider.json 的默认值和检查、读、存新版本、导入时定位并换文件） | `Capabilities/`、`MCP/`、`Agent/ScriptTools`；`capabilities/`、`mcp/`、`agent/ScriptTools` | 完成（`capabilities.*`、`scriptTools.*`、`providers.*`、`mcp.*`；每个条目按两端的类型再解一遍（能力定义、设置项），起进程跑脚本和沙盒、解 / 打 zip、移到废纸篓、注册表留在两端；provider.json 的报错统一成 Qt 那样具体的说法） |
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
| 4 | MCP 客户端（起 server 进程、stdio 读写、JSON-RPC、服务端的 ping、进程退出、结束进程（Unix 新会话 + SIGTERM、Windows 作业对象）、stderr 解码（Windows 按系统代码页兜底）、Streamable HTTP 的 SSE 解析和会话 id、SDK 原文的报错、严格的结果解码）、配置变了哪些要重连 | `MCP/`；`mcp/` | 完成（`mcpClient.*` 句柄、`mcp.syncPlan`；发 HTTP 留在两端（`mcpClient.httpPost` 事件，两端把响应一块块交回）；Swift 不再用 MCP Swift SDK（包还链着，没改 Package.resolved）；找程序、环境变量、${secret:…}、插件和能力带来的服务、服务状态和工具描述、媒体落盘留在两端） |
| 4 | 事件回调：核心推事件、请两端做事再等回话（`friday_core_set_event_handler`，ABI 2；见 ARCHITECTURE.md §2.3） | `Core/CoreEvents.swift`；`util/CoreBridge` | 完成 |
| 4 | 会话存储：落盘（核心留一份存档，两端只交改过的会话和顺序，核心在后台拼整份写；空白不写、只剩草稿的写；序号防先后乱；落盘间隔按编码耗时）、fork 新会话的拼法；增删改、防抖计时、界面观察的数组、正在生成的那条、搜索留在两端 | `Models/ConversationStore.swift`；`models/ConversationStore.cpp` | 完成（`archive.*`、`conversations.fork`；以前每次落盘编码整份存档，现在只编码改过的几段，没改的照核心读入时留着的写，两端认不出的字段也不丢；fork 带上任务模式（Swift 原来没带，按 Qt）；Qt 的落盘间隔原来固定 3 秒，现在和 Swift 一样按耗时放大；搜索要 Unicode 的分解和大小写折叠表，两端系统自带，核心没有这类依赖，留在两端） |
| 4 | 引擎的对话循环（历史、收尾、按会话折叠、本地大模型压历史、超长重发、统计、复读处理、截断续写、工具结果和截图、说了没做的提醒） | `LLM/Engine/OpenAIChatEngine.swift` 的 run；`llm/engine/OpenAIChatEngine.cpp` | 完成（`loop.*` 句柄，两端只照着执行：发 HTTP、跑工具、问本机上限；参数不是合法 JSON 的工具调用历史里记 "{}"（Qt 原来的做法））；v2.86 同 Friday：已折叠步骤的上限（`folding::trim`，设置 `foldedBudget` / `historyTurn`，`prepare` 回 `trimmed`）、在线模型按已知上限（超长报错里的、服务商报的）先压、被说超长压完重发一次、在线也校准、`loop.appended` 拿这个循环接上的原样消息 |
| 4 | 智能体循环里两端共有的规则：审批（删除类、系统改动、按权限模式；Windows 上再认 PowerShell / cmd 的删除命令）、看门狗（期限、卡住、原地打转、被停时的说明和建议、做到一半的结果）、交付检查和写完没验证、任务状态、子智能体（参数、起名、角色、用哪个大脑、按活挑模型和思考档位、太长结果的截断）、工作流（计划校验、按依赖推进、交回的文字）、团队调度（原样重派 / 重复派 / 原样往下传的检查、汇报的写法、在整个任务里的位置）、运行记录 | `Agent/AgentRunner.swift`、`SubAgentRunner.swift`、`Workflow.swift`、`AgentTeamControl.swift`、`AgentLiveness`、`DeliveryCheck`、`TurnCheck`、`TaskState`、`ModelChooser`；`agent/` | 完成（`approval.*`、`agents.*`、`workflow.*`、`team.*`、`delivery.*`、`turnCheck.*`、`agentLog.*`、`taskState.*`、`models.pick` 等；一轮怎么排、跑子智能体、团队状态、界面上的卡片、自动恢复的重试循环、钩子、命令留在两端——它们是执行和状态，绑着两端的线程、观察对象和任务取消） |
| 5 | 远程：两端客户端共有的部署规则（远程脚本只留一份、报告解析、包的键、从 latest.json / 本地目录挑包、版本比较、该不该升级、装服务端包还是桌面端、失败原因） | Swift `Remote/RemoteDeploy.swift`；Qt `remote/RemoteDeploy` | 完成（`deploy.*`）。服务端的方法分派（RemoteServer）直接读写 Qt 里的会话、智能体、终端、浏览器这些对象，Swift 只做控制端（v2.52）、没有第二份，留在 Qt；增量消息（MessagePatch：每 80 毫秒比一次整条消息）和 WebRTC 工作通道的分帧是热路径，经 JSON 过 C 接口要每次编码整条消息 / 二进制分片，留在两端；客户端那边远程设置的代理（RemoteConfig 等）是对象映射，留在两端 |

**会话存储本身（ConversationStore：增删改、分叉、落盘的防抖）和智能体循环一起搬（第 4 阶段）**：流式生成时每个字都改一次正在写的消息，
存储先搬进核心的话每个字都要过一次 C 接口。第 2 阶段先把数据模型和读入规整搬过来，两端照旧在内存里改、自己落盘。

界面、平台相关的部分（SwiftUI / QML、终端画面、浏览器、通话的音视频采集、系统托盘、自动更新换包）留在两端。
