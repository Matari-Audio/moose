//! Plug-in format identity (CLAP / VST3).
//!
//! `Format` is decoupled from any particular subcommand: install,
//! uninstall, doctor, validate, package, and scaffold all need to
//! talk about "the CLAP version of this plug-in" without redefining
//! a label-or-extension table. Per-format methods live here; the
//! one method that depends on install scope ([`Format::dir`])
//! borrows [`InstallScope`] from `install_scope`.

use std::path::PathBuf;

use crate::install_scope::InstallScope;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Clap,
    Vst3,
}

impl Format {
    /// Human-readable display name (`"CLAP"`, `"VST3"`). Used for
    /// log/UI labels in `doctor`, `validate`, and install/uninstall
    /// messaging.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Clap => "CLAP",
            Self::Vst3 => "VST3",
        }
    }

    /// Per-format install directory for the requested scope.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn dir(self, scope: InstallScope) -> PathBuf {
        match self {
            Self::Clap => scope.clap_dir(),
            Self::Vst3 => scope.vst3_dir(),
        }
    }
}
