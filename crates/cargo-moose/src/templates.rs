//! Embedded template files for AU v3 builds.
//!
//! These are compiled into the binary via `include_str!` so the tool
//! works without the `au3-template/` directory.

// ---------------------------------------------------------------------------
// AU v3 template files
// ---------------------------------------------------------------------------

// AU is macOS-only. Gating the module silences dead-code warnings on
// other platforms for the embedded `include_str!` constants.
#[cfg(target_os = "macos")]
pub mod au3 {
    pub use moose_shim_types::AU_SHIM_TYPES_H as SHIM_TYPES_H;
    pub const SWIFT_SOURCE: &str = include_str!("../templates/au3/AudioUnitFactory.swift");
    pub const BRIDGING_HEADER: &str = include_str!("../templates/au3/BridgingHeader.h");
    pub const APP_MAIN_M: &str = include_str!("../templates/au3/main.m");
    pub const APPEX_ENTITLEMENTS: &str = include_str!("../templates/au3/AUExt.entitlements");
    pub const APP_ENTITLEMENTS: &str = include_str!("../templates/au3/App.entitlements");
    pub const APPEX_INFO_PLIST: &str = include_str!("../templates/au3/AUExt-Info.plist");
    pub const APP_INFO_PLIST: &str = include_str!("../templates/au3/App-Info.plist");

    /// Values substituted into `APPEX_INFO_PLIST` by
    /// [`render_appex_info_plist`]. A new placeholder added to the
    /// template forces a struct-field update, so a missed substitution is
    /// a compile error rather than a runtime `(null) platform` bundle
    /// rejection. Xcode `$(...)` tokens stay for xcodebuild to expand
    /// from the pbxproj.
    pub struct AppexPlistValues<'a> {
        pub au_name: &'a str,
        pub au_type: &'a str,
        pub au_sub: &'a str,
        pub au_mfr: &'a str,
        pub au_tag: &'a str,
        pub au_ver: &'a str,
        pub min_os: &'a str,
        pub supported_platform: &'a str,
    }

    /// Render `APPEX_INFO_PLIST` against `values`. After substitution,
    /// asserts every placeholder we tried to replace is actually gone -
    /// catches typos where the renderer says `MINIOS` but the template
    /// was renamed to `MIN_OS` (or vice versa) before they ship as a
    /// literal token in the bundle.
    pub fn render_appex_info_plist(values: &AppexPlistValues<'_>) -> String {
        let subs: [(&str, &str); 8] = [
            ("AUNAME", values.au_name),
            ("AUTYPE", values.au_type),
            ("AUSUB", values.au_sub),
            ("AUMFR", values.au_mfr),
            ("AUTAG", values.au_tag),
            ("AUVER", values.au_ver),
            ("MINIOS", values.min_os),
            ("SUPPORTEDPLAT", values.supported_platform),
        ];
        let mut plist = APPEX_INFO_PLIST.to_string();
        for (placeholder, value) in &subs {
            plist = plist.replace(placeholder, value);
        }
        for (placeholder, _) in &subs {
            assert!(
                !plist.contains(placeholder),
                "appex Info.plist still contains `{placeholder}` after substitution; \
                 template and renderer disagree on token spelling",
            );
        }
        plist
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn macos_values() -> AppexPlistValues<'static> {
            AppexPlistValues {
                au_name: "Acme: Tremolo",
                au_type: "aufx",
                au_sub: "Trem",
                au_mfr: "Acme",
                au_tag: "Effect",
                au_ver: "1",
                min_os: "13.0",
                supported_platform: "MacOSX",
            }
        }

        #[test]
        fn macos_render_substitutes_platform_and_min_os() {
            // Direct regression test for the (null)-platform xcodebuild
            // failure: the appex's CFBundleSupportedPlatforms must be
            // `MacOSX`, not the placeholder `SUPPORTEDPLAT`, and
            // MinimumOSVersion must match the pbxproj's
            // MACOSX_DEPLOYMENT_TARGET (13.0).
            let plist = render_appex_info_plist(&macos_values());
            assert!(plist.contains("<string>MacOSX</string>"));
            assert!(plist.contains("<string>13.0</string>"));
            assert!(!plist.contains("SUPPORTEDPLAT"));
            assert!(!plist.contains("MINIOS"));
            // macOS path leaves Xcode tokens for xcodebuild to expand.
            assert!(plist.contains("$(PRODUCT_BUNDLE_IDENTIFIER)"));
        }

        #[test]
        fn render_substitutes_audio_component_fields() {
            let plist = render_appex_info_plist(&macos_values());
            assert!(plist.contains("<string>aufx</string>"));
            assert!(plist.contains("<string>Trem</string>"));
            assert!(plist.contains("<string>Acme</string>"));
            assert!(plist.contains("<string>Effect</string>"));
            assert!(plist.contains("<string>Acme: Tremolo</string>"));
        }
    }
}
