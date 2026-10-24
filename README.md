# friday.core

Friday 的核心程序（Rust）。Swift 版（macOS，`integemjack/Friday` 的 `Friday/`）和 Qt 版（Windows / Linux，`qt/`）都把它**静态链接**进去，
经一个很小的 C 接口调用（`include/friday_core.h`）：方法名 + JSON 参数进，JSON 结果出。

以前同一套逻辑在 Swift 和 C++ 里各写一份、逐行对齐（Swift 为准）；现在一块一块搬到这里，搬过来的部分两端只剩调用它的薄包装，
行为只写一次、测一次。搬的顺序和进度见 [docs/PORTING.md](docs/PORTING.md)，结构和约定见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

## 目录

```
Cargo.toml              工作区（依赖版本写在这里）；Cargo.lock 提交进仓库，构建一律 --locked
rust-toolchain.toml     Rust 版本（本机和 CI 一致）
crates/friday-core/     核心逻辑（纯 Rust，不碰 C 接口）：api.rs 是统一入口，其余每个模块对应两端原来的一块
crates/friday-core-ffi/ C 接口，编成静态库 libfridaycore.a（Windows：fridaycore.lib）
include/                friday_core.h + module.modulemap（Swift 里 import FridayCore）
cmake/FridayCore.cmake  Qt 版用：构建时调 cargo，导入成 FridayCore::core
scripts/build_apple.sh  Swift 版用：Xcode 构建阶段按 ARCHS 编、lipo 合成
docs/                   架构、移植计划
```

## 在 Friday 里的位置

Friday 仓库把它作为子模块放在 `core/`（钉住提交）。改核心：在这里提交、推送，再在 Friday 的功能分支里
`git -C core pull` 并提交子模块指针。

## 开发

```bash
cargo test                      # 全部测试
cargo clippy --all-targets -- -D warnings
cargo fmt
ARCHS="arm64 x86_64" scripts/build_apple.sh   # 编 macOS 通用静态库到 target/apple/
```

本机（M1 iMac）的 Rust 装在外置盘：`~/.cargo`、`~/.rustup` 是指向 `/Volumes/9100 PRO/rust/` 的软链（系统盘空间不够）。

## 许可证

MIT 或 Apache-2.0 任选（[LICENSE-MIT](LICENSE-MIT)、[LICENSE-APACHE](LICENSE-APACHE)）。除非另有说明，提交进来的贡献按同样的双许可证授权。
