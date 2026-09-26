//! Owned desktop file-dialog lifecycle for plugin editors.
//!
//! Linux dialogs are child processes polled from the editor frame. No Rust
//! worker outlives the editor or keeps executing code from an unloaded plugin
//! module. Dropping the service cancels and reaps its child.

use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

/// A named group of accepted file extensions (without leading dots).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

impl DialogFilter {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        extensions: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            extensions: extensions.into_iter().map(Into::into).collect(),
        }
    }
}

/// One desktop file-dialog request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogRequest {
    OpenFile {
        title: String,
        filters: Vec<DialogFilter>,
    },
    SaveFile {
        title: String,
        file_name: Option<String>,
        directory: Option<PathBuf>,
        filters: Vec<DialogFilter>,
    },
    PickFolder {
        title: String,
        directory: Option<PathBuf>,
    },
}

/// Backend that completed a dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DialogBackend {
    /// Linux `zenity` desktop dialog.
    Zenity,
}

/// Completed dialog outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogResult {
    Selected {
        path: PathBuf,
        backend: DialogBackend,
    },
    Cancelled {
        backend: DialogBackend,
    },
    Failed(String),
}

/// Error returned before a request starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogError {
    Unavailable,
    Busy,
    ResultPending,
    EditorClosed,
    StartFailed(String),
}

impl fmt::Display for DialogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("desktop file dialogs are unavailable"),
            Self::Busy => formatter.write_str("a desktop file dialog is already open"),
            Self::ResultPending => formatter.write_str("the previous dialog result was not read"),
            Self::EditorClosed => formatter.write_str("the editor window is closing"),
            Self::StartFailed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for DialogError {}

/// One editor's bounded dialog service.
///
/// A service owns at most one request and one unread completion. Call
/// [`Self::try_result`] from an editor frame. [`Self::request`] binds the
/// service to that frame's egui context; the editor then cancels and reaps it
/// before any close callback runs. Windows and macOS intentionally report
/// unavailable: Moose does not yet have a cancellable, correctly parented
/// native-dialog lifecycle on those platforms.
pub struct DialogService {
    state: Arc<Mutex<DialogState>>,
}

#[derive(Default)]
struct DialogState {
    #[cfg(target_os = "linux")]
    active: Option<LinuxDialog>,
    completed: Option<DialogResult>,
}

impl Default for DialogService {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(DialogState::default())),
        }
    }
}

impl DialogService {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this target has an owned dialog implementation.
    #[must_use]
    pub const fn is_available() -> bool {
        cfg!(target_os = "linux")
    }

    /// Whether a dialog currently owns a platform child process.
    #[must_use]
    pub fn is_active(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Start one dialog.
    ///
    /// # Errors
    /// Returns [`DialogError::Busy`] while a request is open,
    /// [`DialogError::ResultPending`] until its completion is consumed, or a
    /// platform/startup error before ownership is established. A context not
    /// owned by a live `EguiEditor` returns [`DialogError::EditorClosed`].
    pub fn request(
        &mut self,
        context: &egui::Context,
        request: DialogRequest,
    ) -> Result<(), DialogError> {
        let Some(registry) = DialogRegistry::from_context(context) else {
            return Err(DialogError::EditorClosed);
        };
        registry.request(&self.state, request)
    }

    /// Consume the completed result, or return `None` while no result is ready.
    #[must_use]
    pub fn try_result(&mut self) -> Option<DialogResult> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        #[cfg(target_os = "linux")]
        if state.completed.is_none()
            && let Some(active) = state.active.as_mut()
        {
            match active.child.try_wait() {
                Ok(Some(status)) => {
                    if let Some(active) = state.active.take() {
                        state.completed = Some(
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                active.finish(status)
                            }))
                            .unwrap_or_else(|_| {
                                DialogResult::Failed("desktop file dialog panicked".to_owned())
                            }),
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(mut active) = state.active.take() {
                        active.cancel_and_reap();
                    }
                    state.completed = Some(DialogResult::Failed(format!(
                        "could not poll desktop file dialog: {error}"
                    )));
                }
            }
        }
        state.completed.take()
    }

    /// Cancel the active request and consume any cached completion.
    pub fn cancel(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
    }
}

impl Drop for DialogService {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl DialogState {
    // Linux transfers the owned request into the process command. Other
    // targets intentionally reject it, which makes cross-target clippy see a
    // non-consuming branch even though the public ownership contract is real.
    #[allow(clippy::needless_pass_by_value)]
    fn request(&mut self, request: DialogRequest) -> Result<(), DialogError> {
        if self.completed.is_some() {
            return Err(DialogError::ResultPending);
        }
        #[cfg(target_os = "linux")]
        {
            if self.active.is_some() {
                return Err(DialogError::Busy);
            }
            self.active = Some(LinuxDialog::start(request)?);
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = request;
            Err(DialogError::Unavailable)
        }
    }

    fn cancel(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(mut active) = self.active.take() {
            active.cancel_and_reap();
        }
        self.completed = None;
    }
}

impl Drop for DialogState {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone)]
pub(crate) struct DialogRegistry {
    inner: Arc<Mutex<DialogRegistryInner>>,
}

#[derive(Default)]
struct DialogRegistryInner {
    closed: bool,
    services: Vec<Weak<Mutex<DialogState>>>,
}

impl DialogRegistry {
    const CONTEXT_ID: &'static str = "moose-egui-dialog-registry";

    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(DialogRegistryInner::default())),
        }
    }

    pub(crate) fn install(&self, context: &egui::Context) {
        context.data_mut(|data| {
            data.insert_temp(egui::Id::new(Self::CONTEXT_ID), self.clone());
        });
    }

    fn from_context(context: &egui::Context) -> Option<Self> {
        context.data_mut(|data| data.get_temp(egui::Id::new(Self::CONTEXT_ID)))
    }

    fn request(
        &self,
        state: &Arc<Mutex<DialogState>>,
        request: DialogRequest,
    ) -> Result<(), DialogError> {
        let mut registry = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry.closed {
            return Err(DialogError::EditorClosed);
        }
        registry
            .services
            .retain(|service| service.strong_count() > 0);
        if !registry
            .services
            .iter()
            .filter_map(Weak::upgrade)
            .any(|registered| Arc::ptr_eq(&registered, state))
        {
            registry.services.push(Arc::downgrade(state));
        }
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request(request)
    }

    pub(crate) fn close(&self) {
        let services = {
            let mut registry = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if registry.closed {
                return;
            }
            registry.closed = true;
            registry
                .services
                .drain(..)
                .filter_map(|service| service.upgrade())
                .collect::<Vec<_>>()
        };
        for service in services {
            service
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .cancel();
        }
    }
}

#[cfg(target_os = "linux")]
struct LinuxDialog {
    child: std::process::Child,
}

#[cfg(target_os = "linux")]
impl LinuxDialog {
    fn start(request: DialogRequest) -> Result<Self, DialogError> {
        use std::process::{Command, Stdio};

        let mut command = Command::new("zenity");
        command
            .arg("--file-selection")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match request {
            DialogRequest::OpenFile { title, filters } => {
                command.arg(format!("--title={title}"));
                add_zenity_filters(&mut command, &filters);
            }
            DialogRequest::SaveFile {
                title,
                file_name,
                directory,
                filters,
            } => {
                command.args(["--save", "--confirm-overwrite"]);
                command.arg(format!("--title={title}"));
                if let Some(path) = dialog_start_path(directory, file_name) {
                    command.arg(filename_argument(&path, false));
                }
                add_zenity_filters(&mut command, &filters);
            }
            DialogRequest::PickFolder { title, directory } => {
                command.arg("--directory");
                command.arg(format!("--title={title}"));
                if let Some(directory) = directory {
                    command.arg(filename_argument(&directory, true));
                }
            }
        }
        command
            .spawn()
            .map(|child| Self { child })
            .map_err(|error| {
                DialogError::StartFailed(format!("could not open zenity file dialog: {error}"))
            })
    }

    fn finish(self, status: std::process::ExitStatus) -> DialogResult {
        let output = match self.child.wait_with_output() {
            Ok(output) => output,
            Err(error) => {
                return DialogResult::Failed(format!(
                    "could not collect desktop file dialog result: {error}"
                ));
            }
        };
        if status.success() {
            selected_path(output.stdout).map_or_else(
                || DialogResult::Failed("desktop file dialog returned an empty path".to_owned()),
                |path| DialogResult::Selected {
                    path,
                    backend: DialogBackend::Zenity,
                },
            )
        } else if status.code() == Some(1) {
            DialogResult::Cancelled {
                backend: DialogBackend::Zenity,
            }
        } else {
            DialogResult::Failed(format!(
                "zenity exited with {status}: {}",
                String::from_utf8_lossy(&output.stderr).trim_end_matches(['\r', '\n'])
            ))
        }
    }

    fn cancel_and_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(target_os = "linux")]
fn selected_path(mut bytes: Vec<u8>) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    (!bytes.is_empty()).then(|| PathBuf::from(OsString::from_vec(bytes)))
}

#[cfg(target_os = "linux")]
fn filename_argument(path: &std::path::Path, trailing_separator: bool) -> std::ffi::OsString {
    let mut argument = std::ffi::OsString::from("--filename=");
    argument.push(path);
    if trailing_separator {
        argument.push("/");
    }
    argument
}

#[cfg(target_os = "linux")]
fn add_zenity_filters(command: &mut std::process::Command, filters: &[DialogFilter]) {
    for filter in filters {
        let patterns = filter
            .extensions
            .iter()
            .map(|extension| format!("*.{}", extension.trim_start_matches('.')))
            .collect::<Vec<_>>()
            .join(" ");
        command.arg(format!("--file-filter={} | {patterns}", filter.name));
    }
}

#[cfg(target_os = "linux")]
fn dialog_start_path(directory: Option<PathBuf>, file_name: Option<String>) -> Option<PathBuf> {
    match (directory, file_name) {
        (Some(directory), Some(file_name)) => Some(directory.join(file_name)),
        (Some(directory), None) => Some(directory),
        (None, Some(file_name)) => Some(PathBuf::from(file_name)),
        (None, None) => None,
    }
}
