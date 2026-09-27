#!/usr/bin/env bash
# Reads changed paths on stdin, prints which OS suites CI must run:
# "macos windows linux" (any subset), or nothing for a docs-only change.
#
# A path is OS-specific only if no other OS compiles or reads it. The lists
# come from the `#[cfg(target_os = ...)]`-gated `mod` declarations in the
# workspace, the per-OS screenshot baselines, and the macOS-only AU crate
# and scripts. Anything not listed (Cargo.lock, moose-core, cargo-moose,
# macros, templates, workflows, ...) is shared and runs every OS.
#
# When you gate a new module on target_os, add its path here.
set -euo pipefail

mac=; win=; lin=
while IFS= read -r p; do
  [ -n "$p" ] || continue
  case "$p" in
    *.md | docs/*) ;;
    # macOS: cfg(target_os = "macos") modules; cfg(target_os = "ios")
    # modules (no CI job builds iOS, the macOS jobs are the closest Apple
    # toolchain); moose-au, which only does real work on macOS (its items
    # and build.rs are target_os-gated, other OSes compile an empty shell,
    # and main's push run still builds it everywhere); the Swift au3
    # templates; the macOS-only gui-window scripts; macOS baselines.
    crates/moose-baseview/src/platform/macos/* | \
    crates/moose-baseview/src/wrappers/appkit.rs | \
    crates/moose-baseview/src/wrappers/appkit/* | \
    crates/cargo-moose/src/commands/package/macos.rs | \
    crates/cargo-moose/src/util/bundle_link.rs | \
    crates/moose-standalone/src/menu_macos.rs | \
    crates/moose-standalone/src/windowed_macos.rs | \
    crates/cargo-moose/src/commands/install/au_v3.rs | \
    crates/moose-egui/src/editor_ios.rs | \
    crates/moose-gui/src/editor_ios.rs | \
    crates/moose-gui-types/src/ios.rs | \
    crates/moose-au/* | \
    crates/cargo-moose/templates/au3/* | \
    scripts/gui-window-test.sh | scripts/macos-cursor-poke.swift | \
    */screenshots/*_macos.png)
      mac=1 ;;
    # cfg(target_os = "windows") (Inno Setup script is generated in windows.rs)
    crates/moose-baseview/src/platform/win/* | \
    crates/moose-baseview/src/wrappers/win32.rs | \
    crates/moose-baseview/src/wrappers/win32/* | \
    crates/cargo-moose/src/commands/package/windows.rs | \
    crates/cargo-moose/src/commands/package/windows_manifest.rs | \
    crates/moose-egui/src/render_thread.rs | \
    crates/moose-standalone/src/menu_windows.rs | \
    crates/moose-standalone/src/windowed_windows.rs | \
    */screenshots/*_windows.png)
      win=1 ;;
    # cfg(target_os = "linux") / X11
    crates/moose-baseview/src/platform/x11/* | \
    crates/moose-baseview/src/wrappers/glx.rs | \
    crates/moose-baseview/src/wrappers/xkbcommon.rs | \
    crates/moose-baseview/src/wrappers/xlib.rs | \
    crates/moose-baseview/src/wrappers/xlib/* | \
    crates/moose-standalone/src/windowed_x11.rs | \
    */screenshots/*_linux.png)
      lin=1 ;;
    *) mac=1; win=1; lin=1 ;;
  esac
done
echo ${mac:+macos} ${win:+windows} ${lin:+linux}
