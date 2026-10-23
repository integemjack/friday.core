# 给 Qt 版（CMake）编 friday.core 并导入成静态库目标 FridayCore::core：
#
#   include(<friday.core>/cmake/FridayCore.cmake)
#   target_link_libraries(<目标> PUBLIC FridayCore::core)
#
# 构建时调 cargo 编 friday-core-ffi（一律 release；Rust 的源码改了才重编），产物在 <构建目录>/friday-core-target；
# 头文件 include/friday_core.h 跟着目标走。按 CMake 的目标平台选 Rust 的 target（Windows MSVC / Linux / macOS，x64 / arm64），
# 没装的 target 用 rustup 补上。
# 变量：
#   FRIDAY_CORE_CARGO     cargo 的路径（默认 PATH 里的，或 ~/.cargo/bin 里的）
#   FRIDAY_CORE_TARGET    Rust 的 target，自己给就不按平台推断（交叉编译时用）
#   FRIDAY_CORE_LIBRARY   现成的静态库：给了就不调 cargo（没有 Rust 的机器、或别处编好的）
include_guard(GLOBAL)

get_filename_component(FRIDAY_CORE_ROOT "${CMAKE_CURRENT_LIST_DIR}/.." ABSOLUTE)

if(NOT FRIDAY_CORE_LIBRARY)
    find_program(FRIDAY_CORE_CARGO cargo HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin" "$ENV{USERPROFILE}/.cargo/bin")
    if(NOT FRIDAY_CORE_CARGO)
        message(FATAL_ERROR "编 friday.core 要 Rust（https://rustup.rs）；或用 -DFRIDAY_CORE_LIBRARY=<静态库> 指定编好的")
    endif()

    if(NOT FRIDAY_CORE_TARGET)
        set(_arch "${CMAKE_SYSTEM_PROCESSOR}")
        if(APPLE AND CMAKE_OSX_ARCHITECTURES)
            list(LENGTH CMAKE_OSX_ARCHITECTURES _count)
            if(_count GREATER 1)
                message(FATAL_ERROR "friday.core：一次只编一个架构（CMAKE_OSX_ARCHITECTURES=${CMAKE_OSX_ARCHITECTURES}）")
            endif()
            set(_arch "${CMAKE_OSX_ARCHITECTURES}")
        elseif(MSVC AND CMAKE_GENERATOR_PLATFORM)
            set(_arch "${CMAKE_GENERATOR_PLATFORM}")
        endif()
        string(TOLOWER "${_arch}" _arch)
        if(_arch MATCHES "^(arm64|aarch64)$")
            set(_arch aarch64)
        elseif(_arch MATCHES "^(x86_64|amd64|x64)$")
            set(_arch x86_64)
        else()
            message(FATAL_ERROR "friday.core：不认识的架构 ${_arch}（用 -DFRIDAY_CORE_TARGET=<Rust target> 指定）")
        endif()
        if(APPLE)
            set(FRIDAY_CORE_TARGET "${_arch}-apple-darwin")
        elseif(WIN32 AND MSVC)
            set(FRIDAY_CORE_TARGET "${_arch}-pc-windows-msvc")
        elseif(WIN32)
            set(FRIDAY_CORE_TARGET "${_arch}-pc-windows-gnullvm")
        else()
            set(FRIDAY_CORE_TARGET "${_arch}-unknown-linux-gnu")
        endif()
    endif()

    set(_target_dir "${CMAKE_BINARY_DIR}/friday-core-target")
    if(WIN32 AND MSVC)
        set(_name fridaycore.lib)
    else()
        set(_name libfridaycore.a)
    endif()
    set(FRIDAY_CORE_LIBRARY "${_target_dir}/${FRIDAY_CORE_TARGET}/release/${_name}")

    file(GLOB_RECURSE _sources CONFIGURE_DEPENDS "${FRIDAY_CORE_ROOT}/crates/*.rs" "${FRIDAY_CORE_ROOT}/crates/*/Cargo.toml")
    find_program(FRIDAY_CORE_RUSTUP rustup HINTS "$ENV{CARGO_HOME}/bin" "$ENV{HOME}/.cargo/bin" "$ENV{USERPROFILE}/.cargo/bin")
    set(_commands)
    if(FRIDAY_CORE_RUSTUP)
        # 已经装了的话几乎不花时间
        list(APPEND _commands COMMAND "${FRIDAY_CORE_RUSTUP}" target add "${FRIDAY_CORE_TARGET}")
    endif()
    if(APPLE AND CMAKE_OSX_DEPLOYMENT_TARGET)
        set(_env "${CMAKE_COMMAND}" -E env "MACOSX_DEPLOYMENT_TARGET=${CMAKE_OSX_DEPLOYMENT_TARGET}")
    else()
        set(_env)
    endif()
    add_custom_command(
        OUTPUT "${FRIDAY_CORE_LIBRARY}"
        ${_commands}
        COMMAND ${_env} "${FRIDAY_CORE_CARGO}" build --release --locked -p friday-core-ffi --target "${FRIDAY_CORE_TARGET}"
                --target-dir "${_target_dir}"
        WORKING_DIRECTORY "${FRIDAY_CORE_ROOT}"
        DEPENDS ${_sources} "${FRIDAY_CORE_ROOT}/Cargo.toml" "${FRIDAY_CORE_ROOT}/Cargo.lock" "${FRIDAY_CORE_ROOT}/rust-toolchain.toml"
        COMMENT "编 friday.core（${FRIDAY_CORE_TARGET}）"
        USES_TERMINAL
        VERBATIM)
    add_custom_target(friday_core_cargo DEPENDS "${FRIDAY_CORE_LIBRARY}")
endif()

add_library(FridayCore::core STATIC IMPORTED GLOBAL)
set_target_properties(FridayCore::core PROPERTIES
    IMPORTED_LOCATION "${FRIDAY_CORE_LIBRARY}"
    INTERFACE_INCLUDE_DIRECTORIES "${FRIDAY_CORE_ROOT}/include")
# Rust 标准库要的系统库（== cargo rustc -- --print native-static-libs）
if(APPLE)
    set_property(TARGET FridayCore::core PROPERTY INTERFACE_LINK_LIBRARIES "-framework CoreFoundation")
elseif(WIN32)
    set_property(TARGET FridayCore::core PROPERTY INTERFACE_LINK_LIBRARIES kernel32 advapi32 ntdll userenv ws2_32 dbghelp)
else()
    set_property(TARGET FridayCore::core PROPERTY INTERFACE_LINK_LIBRARIES pthread dl m)
endif()
if(TARGET friday_core_cargo)
    add_dependencies(FridayCore::core friday_core_cargo)
endif()
