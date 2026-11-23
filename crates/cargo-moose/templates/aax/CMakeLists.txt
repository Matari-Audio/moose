cmake_minimum_required(VERSION 3.19)
# CMP0091 must be NEW before the compiler is initialized so the runtime
# property selects /MT or /MD for both this target and the SDK.
cmake_policy(SET CMP0091 NEW)
project(MooseAAXPlugin LANGUAGES CXX)
if(APPLE)
    enable_language(OBJCXX)
endif()

if(MSVC)
    if(MOOSE_AAX_WINDOWS_ARCH STREQUAL "arm64" AND
       NOT MSVC_CXX_ARCHITECTURE_ID STREQUAL "ARM64")
        message(FATAL_ERROR "ARM64 AAX requires an ARM64 compiler target; ${CMAKE_GENERATOR} selected ${MSVC_CXX_ARCHITECTURE_ID}")
    endif()
    if(MOOSE_MSVC_CRT STREQUAL "MD")
        set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>DLL")
    elseif("${MOOSE_MSVC_CRT}" STREQUAL "" OR MOOSE_MSVC_CRT STREQUAL "MT")
        set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
    else()
        message(FATAL_ERROR "MOOSE_MSVC_CRT must be MT or MD")
    endif()
elseif(WIN32)
    message(FATAL_ERROR "AAX on Windows requires an MSVC-compatible compiler")
endif()

if(NOT DEFINED AAX_SDK_PATH)
    if(DEFINED ENV{AAX_SDK_PATH})
        set(AAX_SDK_PATH $ENV{AAX_SDK_PATH})
    else()
        message(FATAL_ERROR "Set AAX_SDK_PATH to the Avid AAX SDK root directory.")
    endif()
endif()

if(MOOSE_AAX_SDK_ONLY)
    add_library(AAX_SDK_Interface INTERFACE)
    target_include_directories(AAX_SDK_Interface INTERFACE
        "${AAX_SDK_PATH}/Interfaces" "${AAX_SDK_PATH}/Interfaces/ACF")
    add_subdirectory("${AAX_SDK_PATH}/Libs/AAXLibrary" AAXLibrary)
    string(TOUPPER "${MOOSE_AAX_CONFIG}" archive_config)
    set_target_properties(AAXLibrary PROPERTIES
        "ARCHIVE_OUTPUT_DIRECTORY_${archive_config}" "${MOOSE_AAX_ARCHIVE_DIR}"
        OUTPUT_NAME AAXLibrary
    )
    if(MSVC)
        set_property(TARGET AAXLibrary PROPERTY MSVC_RUNTIME_LIBRARY "${CMAKE_MSVC_RUNTIME_LIBRARY}")
    endif()
    return()
endif()

if(NOT DEFINED MOOSE_RUST_STATICLIB OR NOT EXISTS "${MOOSE_RUST_STATICLIB}")
    message(FATAL_ERROR "MOOSE_RUST_STATICLIB must point to the built Rust staticlib archive.")
endif()

if(NOT DEFINED MOOSE_PLUGIN_STEM)
    set(MOOSE_PLUGIN_STEM "MoosePlugin")
endif()

if(NOT DEFINED MOOSE_BUNDLE_ID)
    set(MOOSE_BUNDLE_ID "com.moose.plugin.aax")
endif()

if(NOT DEFINED MOOSE_PLUGIN_VERSION)
    set(MOOSE_PLUGIN_VERSION "1.0.0")
endif()

include(AAXSDK.cmake)
moose_aax_sdk_archive(archive)
add_library(AAXLibrary STATIC IMPORTED)
set_target_properties(AAXLibrary PROPERTIES IMPORTED_LOCATION "${archive}")

add_library(MooseAAXPlugin MODULE
    src/MooseAAX_Describe.cpp
    src/MooseAAX_Parameters.cpp
    src/MooseAAX_GUI.cpp
    ${AAX_SDK_PATH}/Extensions/Topology/AAX_CMonolithicParameters.cpp
    ${AAX_SDK_PATH}/Interfaces/AAX_Exports.cpp
)

target_include_directories(MooseAAXPlugin PRIVATE
    ${AAX_SDK_PATH}/Interfaces
    ${AAX_SDK_PATH}/Interfaces/ACF
    ${AAX_SDK_PATH}/Extensions/Topology
    ${AAX_SDK_PATH}/Extensions/GUI
    ${CMAKE_CURRENT_SOURCE_DIR}/src
)

# Link the Rust staticlib directly into the .aaxplugin binary (zero dlopen).
target_link_libraries(MooseAAXPlugin PRIVATE
    AAXLibrary
    "${MOOSE_RUST_STATICLIB}"
)

if(APPLE)
    target_link_libraries(MooseAAXPlugin PRIVATE
        "-framework CoreFoundation"
        "-framework Cocoa"
        "-framework QuartzCore"
        "-framework Metal"
        "-framework CoreGraphics"
        "-framework CoreText"
        "-framework Security"
        "-framework SystemConfiguration"
        iconv
        resolv
    )
    set_target_properties(MooseAAXPlugin PROPERTIES
        OUTPUT_NAME "${MOOSE_PLUGIN_STEM}"
        BUNDLE TRUE
        BUNDLE_EXTENSION "aaxplugin"
        MACOSX_BUNDLE_INFO_PLIST ${CMAKE_CURRENT_SOURCE_DIR}/src/Info.plist.in
    )
elseif(WIN32)
    set_property(TARGET MooseAAXPlugin PROPERTY
        MSVC_RUNTIME_LIBRARY "${CMAKE_MSVC_RUNTIME_LIBRARY}")
    target_link_libraries(MooseAAXPlugin PRIVATE
        ws2_32
        userenv
        bcrypt
        ntdll
        advapi32
        ole32
        shell32
        gdi32
        user32
        dwmapi
        opengl32
        imm32
        winmm
    )
    set_target_properties(MooseAAXPlugin PROPERTIES
        OUTPUT_NAME "${MOOSE_PLUGIN_STEM}"
        SUFFIX ".aaxplugin"
        PREFIX ""
    )
endif()

target_compile_features(MooseAAXPlugin PRIVATE cxx_std_17)
