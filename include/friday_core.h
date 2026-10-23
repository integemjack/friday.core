#pragma once
// friday.core（Rust）的 C 接口：Swift 版（import FridayCore，见同目录的 module.modulemap）和 Qt 版（cmake/FridayCore.cmake）
// 都静态链 libfridaycore.a（Windows 是 fridaycore.lib）。实现在 crates/friday-core-ffi/src/lib.rs，改这里要一起改。
//
// 调用方式只有一种：方法名 + JSON 参数进，JSON 结果出（方法和参数见 crates/friday-core/src/api.rs、docs/ARCHITECTURE.md）。
// 结果总是对象：成功 {"result": …}，失败 {"error": {"code": "…", "message": "…"}}。时间用 Unix 毫秒，时长用秒。
// 字符串都是 UTF-8、以 NUL 结尾。任意线程都能调。
#include <stdint.h>

#define FRIDAY_CORE_ABI_VERSION 2

#ifdef __cplusplus
extern "C" {
#endif

/// 执行一个方法。method：方法名（如 "quota.classify"）；params_json：JSON 对象的文本，NULL 或空串 = {}。
/// 返回的 JSON 文本归调用方，用 friday_core_free 放掉。
char* friday_core_call(const char* method, const char* params_json);

/// 放掉 friday_core_call 返回的字符串（NULL 什么都不做）
void friday_core_free(char* text);

/// 核心的版本号（静态字符串，不用放）
const char* friday_core_version(void);

/// 链进来的库的 C 接口版本；和 FRIDAY_CORE_ABI_VERSION 不一致说明头文件和库不是同一版
uint32_t friday_core_abi_version(void);

/// v2.79 核心有事找两端时调的回调（ABI 2 起）：context 原样带回；event_json 是 JSON 文本，只在这次调用里有效：
/// 事件 {"event": "<名字>", "data": {…}}；请两端做事 {"event": "request", "request": <编号>, "kind": "…", "data": {…}}，
/// 做完调 friday_core_call("core.reply", {"request": 编号, "result": … | "error": "…"})。
/// 在核心的线程上调：不能卡住，也不要同步调回核心，收到就排到自己的线程上处理。
typedef void (*friday_core_event_handler)(void* context, const char* event_json);

/// 交给核心一个回调（handler 为 NULL：不再推）。handler 在换掉之前要一直能调，context 一直有效
void friday_core_set_event_handler(friday_core_event_handler handler, void* context);

#ifdef __cplusplus
}
#endif
