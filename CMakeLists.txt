cmake_minimum_required(VERSION 3.16)

project(fcitx5-voice VERSION 0.2.1 LANGUAGES CXX)

# fcitx5 5.1 的头文件用到了 std::span、std::source_location
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

include(GNUInstallDirs)

option(VOICE_CAPTURE "Build with microphone capture (cpal)" ON)

# 常駩 sherpa-onnx 识别器（script/sherpa-serve.cpp），供 stream: 后端使用。
# 它只依赖 sherpa-onnx 的 C API；没装 sherpa-onnx 时关掉即可，其余后端照常。
option(VOICE_SHERPA_SERVE "Build the resident sherpa-onnx recogniser (sherpa-serve)" ON)

# 默认识别后端命令行（末尾自动追加 WAV 路径）。
# 不编进 .so，而是写进安装的 share/fcitx5/conf/voice.conf（纯文本，便于 nix 覆盖）。
#
# 注意：后端命令行里必然带空格，而 CMake 的 -D 取值不认引号、也不能带空格；
# cmakeFlags 又是逐元素直传（不经 shell），所以无法从 nix 可靠地传进来。
# 折中：这里留一个固定占位串，nix 的 postInstall 统一把它替换成真实命令。
set(VOICE_DEFAULT_BACKEND "whisper-cli -m /usr/share/whisper/ggml-base.bin -l zh -nt -f"
    CACHE STRING "Default ASR backend command line")

if(VOICE_CAPTURE)
    find_package(ALSA REQUIRED)
endif()

find_package(Fcitx5Core 5.0 REQUIRED)
find_package(Fcitx5Config 5.0 REQUIRED)
find_package(Fcitx5Utils 5.0 REQUIRED)

# 常駩识别器需要 sherpa-onnx 的 C API（头文件 c-api.h + 共享库）。
if(VOICE_SHERPA_SERVE)
    # 最高优先级：nix 侧探测后通过环境变量精确提供
    if(DEFINED ENV{SHERPA_ONNX_INCLUDE_DIR})
        set(SHERPA_ONNX_INCLUDE_DIR "$ENV{SHERPA_ONNX_INCLUDE_DIR}")
    endif()
    if(DEFINED ENV{SHERPA_ONNX_LIBRARY})
        set(SHERPA_ONNX_LIBRARY "$ENV{SHERPA_ONNX_LIBRARY}")
    endif()
    # 兜底 1：pkg-config（sherpa-onnx.pc）
    if(NOT SHERPA_ONNX_INCLUDE_DIR OR NOT SHERPA_ONNX_LIBRARY)
        find_package(PkgConfig QUIET)
        if(PkgConfig_FOUND)
            pkg_check_modules(SHERPA_ONNX_PC QUIET sherpa-onnx)
        endif()
        if(SHERPA_ONNX_PC_FOUND)
            if(NOT SHERPA_ONNX_INCLUDE_DIR)
                set(SHERPA_ONNX_INCLUDE_DIR ${SHERPA_ONNX_PC_INCLUDE_DIRS})
            endif()
            if(NOT SHERPA_ONNX_LIBRARY)
                find_library(SHERPA_ONNX_LIBRARY NAMES sherpa-onnx-c-api sherpa-onnx
                    HINTS ${SHERPA_ONNX_PC_LIBRARY_DIRS})
            endif()
        endif()
    endif()
    # 兜底 2：常规查找路径
    if(NOT SHERPA_ONNX_INCLUDE_DIR)
        find_path(SHERPA_ONNX_INCLUDE_DIR "sherpa-onnx/c-api/c-api.h")
    endif()
    if(NOT SHERPA_ONNX_LIBRARY)
        # 上游把 C API 装成 libsherpa-onnx-c-api（没有 libsherpa-onnx.so）
        find_library(SHERPA_ONNX_LIBRARY NAMES sherpa-onnx-c-api sherpa-onnx)
    endif()
    if(NOT SHERPA_ONNX_INCLUDE_DIR OR NOT SHERPA_ONNX_LIBRARY)
        message(FATAL_ERROR
            "找不到 sherpa-onnx（c-api.h 与 libsherpa-onnx）。"
            "装好 sherpa-onnx，或用 -DVOICE_SHERPA_SERVE=OFF 关掉常駩识别器。")
    endif()
    message(STATUS "sherpa-onnx include: ${SHERPA_ONNX_INCLUDE_DIR}")
    message(STATUS "sherpa-onnx lib: ${SHERPA_ONNX_LIBRARY}")
endif()

# Rust 核心产物：先执行 `cargo build --release -p fcitx5-voice-core`
# 默认静态链接 libfcitx5_voice_core.a，插件自成一体，不用再装 .so。
option(VOICE_CORE_STATIC "Link the Rust core statically" ON)
if(VOICE_CORE_STATIC)
    set(_voice_core_default "${CMAKE_CURRENT_SOURCE_DIR}/target/release/libfcitx5_voice_core.a")
else()
    set(_voice_core_default "${CMAKE_CURRENT_SOURCE_DIR}/target/release/libfcitx5_voice_core.so")
endif()
set(VOICE_CORE_LIB "${_voice_core_default}"
    CACHE FILEPATH "Path to libfcitx5_voice_core.a (or .so with -DVOICE_CORE_STATIC=OFF)")
if(NOT EXISTS "${VOICE_CORE_LIB}")
    message(FATAL_ERROR
        "找不到 ${VOICE_CORE_LIB}。请先在仓库根目录执行 `cargo build --release -p fcitx5-voice-core`，"
        "或用 -DVOICE_CORE_LIB=/path/to/libfcitx5_voice_core.a 指定。")
endif()

if(NOT DEFINED FCITX_INSTALL_PKGDATADIR)
    set(FCITX_INSTALL_PKGDATADIR "${CMAKE_INSTALL_DATADIR}/fcitx5")
endif()

add_library(voice MODULE
    src/voice.cpp
)

target_include_directories(voice PRIVATE
    "${CMAKE_CURRENT_SOURCE_DIR}/core/include"
)

target_link_libraries(voice PRIVATE
    Fcitx5::Core
    Fcitx5::Config
    Fcitx5::Utils
    ${VOICE_CORE_LIB}
)

# fcitx5 5.1 把 StandardPath 标成 deprecated（改名 StandardPaths），
# 但我们用它的 Type 枚举做两代兼容，这里关掉这个警告（避免 -Werror 挂）。
target_compile_options(voice PRIVATE "-Wno-deprecated-declarations")

# 光标处的「🎙 正在听…」提示用 InputPanel::setOverlayMessage，5.1.22 起才有。
# 老版本不定义这个宏，插件里对应的显示代码自动跳过（其余功能照常）。
if(Fcitx5Core_VERSION VERSION_GREATER_EQUAL 5.1.22)
    target_compile_definitions(voice PRIVATE FCITX5_VOICE_OVERLAY_MESSAGE=1)
endif()

if(VOICE_CAPTURE)
    target_link_libraries(voice PRIVATE ALSA::ALSA)
endif()


if(NOT VOICE_CORE_STATIC)
    set_target_properties(voice PROPERTIES INSTALL_RPATH "$ORIGIN/..")
endif()

# 静态库里的 Rust 符号不导出，不和别的插件冲突
target_link_options(voice PRIVATE "-Wl,--exclude-libs,ALL")

# 安装位置用 fcitx5 自己的 addon 目录（Arch: /usr/lib/fcitx5，Debian: /usr/lib/<multiarch>/fcitx5）
# FCITX_INSTALL_ADDONDIR 由 Fcitx5Core 的 CMake 包在 find_package 时设好；
# 宿主前缀不同（含 Nix store）时按本包的 GNUInstallDirs 计算。
if(NOT DEFINED FCITX_INSTALL_ADDONDIR)
    set(FCITX_INSTALL_ADDONDIR "${CMAKE_INSTALL_LIBDIR}/fcitx5")
endif()

install(TARGETS voice
    LIBRARY DESTINATION "${FCITX_INSTALL_ADDONDIR}"
)

# sherpa-serve：常駩识别器。装在 libexec 下，由默认配置直接指向它。
if(VOICE_SHERPA_SERVE)
    add_executable(sherpa-serve script/sherpa-serve.cpp)
    target_include_directories(sherpa-serve PRIVATE
        "${SHERPA_ONNX_INCLUDE_DIR}"
    )
    target_link_libraries(sherpa-serve PRIVATE "${SHERPA_ONNX_LIBRARY}")
    # 显式把 sherpa-onnx 库目录写进 RPATH：它依赖 libsherpa-onnx-core 与
    # libonnxruntime，Nix 的 fixupPhase 一般会补，但不靠运气。
    get_filename_component(SHERPA_ONNX_LIB_DIR "${SHERPA_ONNX_LIBRARY}" DIRECTORY)
    set_target_properties(sherpa-serve PROPERTIES
        INSTALL_RPATH "${SHERPA_ONNX_LIB_DIR}"
        INSTALL_RPATH_USE_LINK_PATH TRUE
    )
    install(TARGETS sherpa-serve
        RUNTIME DESTINATION "${CMAKE_INSTALL_LIBEXECDIR}/fcitx5-voice"
    )
endif()

# addon 注册文件（名字必须是 <addon>.conf，里面 Library= 指向 libvoice）
configure_file(conf/voice.conf.in
    "${CMAKE_CURRENT_BINARY_DIR}/voice.conf" @ONLY)
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/voice.conf"
    DESTINATION "${FCITX_INSTALL_PKGDATADIR}/addon")

# 默认配置（含识别后端命令行），插件启动时作为 PkgData 读入；
# 用户的 ~/.config/fcitx5/conf/voice.conf 会覆盖它。
configure_file(conf/voice-default.conf.in
    "${CMAKE_CURRENT_BINARY_DIR}/voice-default.conf" @ONLY)
install(FILES "${CMAKE_CURRENT_BINARY_DIR}/voice-default.conf"
    DESTINATION "${FCITX_INSTALL_PKGDATADIR}/conf" RENAME voice.conf)
