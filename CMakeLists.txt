cmake_minimum_required(VERSION 3.16)

project(fcitx5-voice VERSION 0.1.0 LANGUAGES CXX)

# fcitx5 5.1 的头文件用到了 std::span、std::source_location
set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

include(GNUInstallDirs)

option(VOICE_CAPTURE "Build with microphone capture (cpal)" ON)

# 默认识别后端命令行（末尾自动追加 WAV 路径）。
# 不编进 .so，而是写进安装的 share/fcitx5/conf/voice.conf（纯文本，便于 nix 覆盖）。
set(VOICE_DEFAULT_BACKEND "whisper-cli -m /usr/share/whisper/ggml-base.bin -l zh -nt -f"
    CACHE STRING "Default ASR backend command line")

if(VOICE_CAPTURE)
    find_package(ALSA REQUIRED)
endif()

find_package(Fcitx5Core 5.0 REQUIRED)
find_package(Fcitx5Config 5.0 REQUIRED)
find_package(Fcitx5Utils 5.0 REQUIRED)

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
