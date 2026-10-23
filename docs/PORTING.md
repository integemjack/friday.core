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
9. Swift 传 `JSONValue` 这类 Codable 值用 `CoreBridge.call(_:encoding:as:)`，不经过 `Any`（整数和小数不会混）。

## 顺序

| 阶段 | 内容 | 两端原来的位置 | 状态 |
|---|---|---|---|
| 0 | 仓库、C 接口、两端静态链接、CoreBridge | — | 完成 |
| 1 | 额度与限流：报错分类、恢复时间、定时试的间隔 | Swift `LLM/Engine/QuotaPolicy.swift`；Qt `llm/engine/QuotaPolicy.cpp` | 完成 |
| 1 | 工具名、参数名纠正（别家叫法换成 Friday 的；「注册表里有没有」留在两端） | `Agent/ToolAliases.swift`；`agent/ToolAliases.cpp` | 完成 |
| 1 | 密钥识别与替换（认、遮、清理存档、名字合不合法；密钥库本身留在两端的 SecretStore） | `Secrets/SecretGuard.swift`；`secrets/SecretGuard.cpp` | 完成 |
| 1 | 上下文折叠、会话折叠（摘要、子 Agent 会话的折叠、估算 token；主会话标折叠、调试用的预算开关留在两端） | `Agent/ContextFolding.swift`、`LLM/Engine/SessionFolding.swift`；`agent/ContextFolding.cpp`、`llm/engine/SessionFolding.cpp` | 完成 |
| 1 | 密钥库：存储、收进库、执行时换回明文 | `Secrets/SecretStore.swift`；`secrets/SecretStore.cpp` | |
| 1 | 等额度的文案和时间（等到什么时候、消息上那一行、时间和「还有多久」的写法；停下、换大脑、定时检查留在两端） | `Agent/QuotaWaiting.swift`；`agent/QuotaWaiting.cpp` | 完成 |
| 2 | 数据模型和存档格式（会话、消息块、工具事件的 JSON 与两端完全一致） | `Models/`；`models/` | |
| 2 | 提示词拼装、工具定义 | `Prompts/`、`Agent/ToolRegistry.swift`；`agent/` | |
| 3 | 在线引擎：请求、流式解析、重试、限流排队 | `LLM/Engine/`；`llm/engine/` | |
| 3 | 工具执行：文件、搜索、shell、MCP | `Agent/AgentTools*`、`MCP/`；`agent/`、`mcp/` | |
| 4 | 智能体循环、子智能体、工作流（有状态：句柄 + 事件回调） | `Agent/AgentRunner.swift`、`SubAgentRunner.swift`；`agent/` | |
| 5 | 远程服务端（照 REMOTE.md） | Qt `remote/` | |

界面、平台相关的部分（SwiftUI / QML、终端画面、浏览器、通话的音视频采集、系统托盘、自动更新换包）留在两端。
