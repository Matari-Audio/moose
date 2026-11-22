cmake_minimum_required(VERSION 3.19)
if(APPLE)
    project(MooseAAXPlugin LANGUAGES CXX OBJCXX)
else()
    project(MooseAAXPlugin LANGUAGES CXX)
endif()

if(NOT DEFINED AAX_SDK_PATH)
    if(DEFINED ENV{AAX_SDK_PATH})
        set(AAX_SDK_PATH $ENV{AAX_SDK_PATH})
    else()
        message(FATAL_ERROR "Set AAX_SDK_PATH to the Avid AAX SDK root directory.")
    endif()
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

# Use pre-built AAXLibrary when available (matching requested architectures on macOS),
# otherwise build AAXLibrary from source.
if(NOT DEFINED AAX_LIB_PATH)
    if(APPLE)
        set(AAX_LIB_PATH "${AAX_SDK_PATH}/build-lib/Libs/AAXLibrary/libAAXLibrary.a")
    else()
        set(AAX_LIB_PATH "${AAX_SDK_PATH}/build-lib/Libs/AAXLibrary/AAXLibrary.lib")
    endif()
endif()

set(AAX_LIB_USABLE FALSE)
if(EXISTS "${AAX_LIB_PATH}")
    set(AAX_LIB_USABLE TRUE)
    if(APPLE AND CMAKE_OSX_ARCHITECTURES)
        execute_process(
            COMMAND lipo -info "${AAX_LIB_PATH}"
            OUTPUT_VARIABLE _aax_lipo_out
            OUTPUT_STRIP_TRAILING_WHITESPACE
        )
        foreach(arch IN LISTS CMAKE_OSX_ARCHITECTURES)
            if(NOT "${_aax_lipo_out}" MATCHES "${arch}")
                set(AAX_LIB_USABLE FALSE)
                break()
            endif()
        endforeach()
    endif()
endif()

if(NOT AAX_LIB_USABLE)
    if(NOT TARGET AAX_SDK_Interface)
        add_library(AAX_SDK_Interface INTERFACE)
        target_include_directories(AAX_SDK_Interface INTERFACE
            ${AAX_SDK_PATH}/Interfaces
            ${AAX_SDK_PATH}/Interfaces/ACF
        )
    endif()
    add_subdirectory(${AAX_SDK_PATH}/Libs/AAXLibrary ${CMAKE_BINARY_DIR}/AAXLibrary)
    set(AAX_LIB_TARGET AAXLibrary)
else()
    add_library(AAXLibrary STATIC IMPORTED)
    set_target_properties(AAXLibrary PROPERTIES IMPORTED_LOCATION "${AAX_LIB_PATH}")
    set(AAX_LIB_TARGET AAXLibrary)
endif()

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
    ${AAX_LIB_TARGET}
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
