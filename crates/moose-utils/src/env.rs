//! Environment variables with the pre-fork `TRUCE_` prefix as a
//! fallback, so a truce setup keeps working after the move to moose.

use std::sync::Mutex;

/// Read `name` (a `MOOSE_*` variable). When it is unset or empty and
/// the matching `TRUCE_*` variable is set, return that instead and
/// print a one-time deprecation warning for it.
#[must_use]
pub fn var(name: &str) -> Option<String> {
    resolve(name, non_empty)
}

fn resolve(name: &str, get: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(v) = get(name) {
        return Some(v);
    }
    let legacy = legacy_name(name)?;
    let v = get(&legacy)?;
    warn_once(&legacy, name);
    Some(v)
}

/// `MOOSE_FOO` -> `TRUCE_FOO`; `None` for names without the prefix.
#[must_use]
pub fn legacy_name(name: &str) -> Option<String> {
    name.strip_prefix("MOOSE_")
        .map(|rest| format!("TRUCE_{rest}"))
}

/// Print `legacy is deprecated, use name` once per legacy name.
pub fn warn_once(legacy: &str, name: &str) {
    static WARNED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let Ok(mut warned) = WARNED.lock() else {
        return;
    };
    if !warned.iter().any(|w| w == legacy) {
        warned.push(legacy.to_string());
        eprintln!("warning: {legacy} is deprecated, set {name} instead");
    }
}

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_name_maps_prefix() {
        assert_eq!(legacy_name("MOOSE_SCALE").as_deref(), Some("TRUCE_SCALE"));
        assert_eq!(legacy_name("PLUGINVAL"), None);
    }

    #[test]
    fn falls_back_to_truce_prefix() {
        let only_legacy = |n: &str| (n == "TRUCE_X").then(|| "legacy".to_string());
        assert_eq!(resolve("MOOSE_X", only_legacy).as_deref(), Some("legacy"));
        let both = |n: &str| Some(if n == "MOOSE_X" { "new" } else { "legacy" }.to_string());
        assert_eq!(resolve("MOOSE_X", both).as_deref(), Some("new"));
        assert_eq!(resolve("MOOSE_X", |_| None), None);
    }
}
