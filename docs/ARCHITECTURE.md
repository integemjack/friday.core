# friday.core 架构

## 1. 为什么是这样

Friday 有两个前端：Swift 版（SwiftUI，macOS）和 Qt 版（QML + C++，Windows / Linux，也是远程服务端）。界面各用各平台的原生做法，
不共用；界面底下的逻辑——智能体循环、提示词、工具、额度、密钥、上下文折叠、存档格式——以前两边各写一份。
friday.core 把这部分收成一份 Rust，两边**静态链接**、进程内调用（不是起一个服务再连过去）。

## 2. C 接口

整个接口只有四个函数（`include/friday_core.h`）：

| 函数 | 作用 |
|---|---|
| `friday_core_call(method, params_json)` | 执行一个方法，返回 JSON 文本（调用方 `friday_core_free`） |
| `friday_core_free(text)` | 放掉上面返回的字符串 |
| `friday_core_version()` | 版本号 |
| `friday_core_abi_version()` | C 接口版本，和头文件的 `FRIDAY_CORE_ABI_VERSION` 比 |

为什么用「方法名 + JSON」而不是每个功能一个 C 函数：

- 接口不随功能增长，加方法不改头文件、不改两边的链接设置；
- 两边已经有成熟的 JSON 编解码（Swift `JSONSerialization` / `Codable`、Qt `QJsonDocument`），类型映射不用手写；
- 和远程协议（Friday 的 `qt/docs/REMOTE.md`，JSON-RPC）同一种形状：以后智能体内核搬过来时，进程内调用和远程连接可以是同一套方法和事件。

代价是每次调用要编解码一次 JSON。目前搬过来的都是低频调用（报错分类之类），这点开销可以忽略；以后遇到高频的
（流式输出逐个字更新界面），再加专门的函数或回调，不硬塞进 JSON。

### 2.1 结果

成功 `{"result": …}`，失败 `{"error": {"code": "…", "message": "…"}}`：

| code | 意思 |
|---|---|
| `unknownMethod` | 没有这个方法（多半是链进来的库比包装层旧） |
| `invalidParams` | 参数类型不对、缺参数 |
| `internal` | 核心里出错；Rust 的 panic 在 C 接口层兜住，也报这个，不会跨过 C 边界 |

时间一律用 Unix 毫秒（参数名 `now` 这类，不传 = 现在），时长一律用秒（浮点）。

### 2.2 线程

任意线程都能调。纯逻辑的方法没有状态；要用两端数据的（`secrets.*` 要库里的密钥值）由两端在数据变化时交给核心
（`secrets.update`），核心每个进程存一份、加读写锁。以后有状态的部分（会话、智能体运行）用句柄：`*.open` 返回 id，后续方法带上它；
需要推送事件时（流式输出、状态变化）加一个注册回调的函数，回调在核心的线程上调，两边各自排回主线程——同 Friday 里
通话内核（`FridayCall.h`）的做法。

## 3. 两边怎么接

### 3.1 Swift 版（Xcode）

- 构建阶段「Rust 核心」调 `Friday/Tools/CI/build_rust_core.sh` → `core/scripts/build_apple.sh`，按 `ARCHS` 每个架构编一份、`lipo` 合成
  `core/target/apple/libfridaycore.a`；
- `SWIFT_INCLUDE_PATHS` 加 `core/include`（module.modulemap 里 `link "fridaycore"`），`LIBRARY_SEARCH_PATHS` 加 `core/target/apple`；
- Swift 里 `import FridayCore`，只在 `Core/CoreBridge.swift` 里直接调 C 函数，其余代码用 `CoreBridge.call(方法, 参数)`（字典进、`Any` 出），
  或 `CoreBridge.call(方法, encoding: 参数, as: 类型)`（Codable 进出，`JSONValue` 这类用它）。

### 3.2 Qt 版（CMake）

- 顶层 `CMakeLists.txt` `include(core/cmake/FridayCore.cmake)`，`friday_core`（Qt 的核心静态库）`PUBLIC` 链 `FridayCore::core`；
- 构建时 cargo 编到 `<构建目录>/friday-core-target/<target>/release/`，Rust 源码改了才重编；
- C++ 里只在 `util/CoreBridge.cpp` 里直接调 C 函数，其余代码用 `CoreBridge::call(方法, 参数)`。

### 3.3 一块逻辑搬过来以后

两边原来的类型和函数签名**保留**（调用的地方不动），实现换成调 CoreBridge；原来的测试照跑，作为「包装层没接错」的检查。
规则本身的测试搬到 Rust（`cargo test`），以后改规则只改 Rust 和它的测试。

## 4. 构建与发布

- Rust 版本钉在 `rust-toolchain.toml`，依赖钉在 `Cargo.lock`，一律 `--locked`；
- Friday 仓库用子模块钉住 friday.core 的提交，Friday.build 的工作流拉源码时带上子模块（`submodules: true`），各平台的运行器都自带 rustup；
- 本仓库的 CI（`.github/workflows/ci.yml`）在 Linux x64 / arm64、macOS、Windows 上跑格式、clippy、测试和静态库。
