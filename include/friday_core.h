#pragma once
// friday.core（Rust）的 C 接口：Swift 版（import FridayCore，见同目录的 module.modulemap）和 Qt 版（cmake/FridayCore.cmake）
// 都静态链 libfridaycore.a（Windows 是 fridaycore.lib）。实现在 crates/friday-core-ffi/src/lib.rs，改这里要一起改。
//
// 调用方式只有一种：方法名 + JSON 参数进，JSON 结果出（方法和参数见 crates/friday-core/src/api.rs、docs/ARCHITECTURE.md）。
// 结果总是对象：成功 {"result": …}，失败 {"error": {"code": "…", "message": "…"}}。时间用 Unix 毫秒，时长用秒。
// 字符串都是 UTF-8、以 NUL 结尾。任意线程都能调。
#include <stdint.h>

#define FRIDAY_CORE_ABI_VERSION 1

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

#ifdef __cplusplus
}
#endif
