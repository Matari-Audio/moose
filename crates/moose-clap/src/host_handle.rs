//! Typed phase-gated handles to the CLAP host (`clap_host`).
//!
//! CLAP divides host callbacks into three threading contracts:
//! - **`[thread-safe]`** ([`Any`]): `request_callback`, `request_restart`,
//!   `request_process`, `get_extension`, `params.request_flush`, and
//!   `gui.request_resize`. Safe to carry across threads (`Send + Sync`)
//!   while the plugin instance (and therefore the host) is alive.
//! - **`[main-thread]`** ([`MainThread`]): `params.rescan`, `params.clear`,
//!   `state.mark_dirty`, `latency.changed`, `audio_ports.rescan`,
//!   `note_name.changed`, `voice_info.changed`, `remote_controls.changed`,
//!   `preset_load.loaded`, `track_info.get`, `gui.closed`, `context_menu.*`,
//!   etc. Not `Send` and not `Sync`; obtained only by proving main-thread
//!   execution with [`moose_core::thread::MainThread`].
//! - **`[audio-thread]`** ([`Audio`]): audio-thread view of the host.
//!   Not `Send` and not `Sync`.

use std::ffi::{CStr, CString, c_char};
use std::marker::PhantomData;

use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_host_audio_ports};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};
use clap_sys::ext::latency::clap_host_latency;
use clap_sys::ext::log::{clap_host_log, clap_log_severity};
use clap_sys::ext::note_name::{CLAP_EXT_NOTE_NAME, clap_host_note_name};
use clap_sys::ext::params::clap_host_params;
use clap_sys::ext::preset_load::{CLAP_EXT_PRESET_LOAD, clap_host_preset_load};
use clap_sys::ext::remote_controls::{CLAP_EXT_REMOTE_CONTROLS, clap_host_remote_controls};
use clap_sys::ext::state::clap_host_state;
use clap_sys::ext::tail::clap_host_tail;
use clap_sys::ext::thread_check::clap_host_thread_check;
use clap_sys::ext::track_info::{clap_host_track_info, clap_track_info};
use clap_sys::ext::voice_info::{CLAP_EXT_VOICE_INFO, clap_host_voice_info};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use moose_core::thread::MainThread as MainThreadToken;

/// Thread-safe CLAP host phase (`[thread-safe]` callbacks only).
pub(crate) struct Any;

/// Main-thread CLAP host phase (`[main-thread]` callbacks).
pub(crate) struct MainThread;

/// Audio-thread CLAP host phase (`[audio-thread]` callbacks).
pub(crate) struct Audio;

/// Phase-gated handle around a CLAP `*const clap_host`.
///
/// Only [`HostHandle<Any>`] is `Send + Sync`, and it exposes only the
/// `[thread-safe]` CLAP host operations. Narrower phases (`MainThread`,
/// `Audio`) inherit `!Send + !Sync` from the raw pointer so they cannot
/// escape the thread whose contract they represent.
pub(crate) struct HostHandle<Phase = Any> {
    raw: *const clap_host,
    _phase: PhantomData<Phase>,
}

impl<Phase> Copy for HostHandle<Phase> {}
impl<Phase> Clone for HostHandle<Phase> {
    fn clone(&self) -> Self {
        *self
    }
}

// SAFETY: `HostHandle<Any>` exposes only CLAP `[thread-safe]` host methods
// (`request_callback`, `request_restart`, `request_process`, `get_extension`,
// `params.request_flush`). Callers constructing `HostHandle::any` promise the
// host outlives every use (enforced by `HostQueue::close` / `with_open`).
unsafe impl Send for HostHandle<Any> {}
unsafe impl Sync for HostHandle<Any> {}

impl<Phase> HostHandle<Phase> {
    /// Raw `*const clap_host` pointer for FFI calls inside the matching phase.
    #[must_use]
    pub(crate) fn as_ptr(self) -> *const clap_host {
        self.raw
    }

    /// Whether the underlying host pointer is null.
    #[must_use]
    pub(crate) fn is_null(self) -> bool {
        self.raw.is_null()
    }

    /// Query a host extension table (`host.get_extension`, `[thread-safe]`).
    ///
    /// # Safety
    /// The underlying `clap_host` must still be alive, and `E` must match the
    /// ABI of the extension identified by `id`.
    #[must_use]
    pub(crate) unsafe fn extension<E>(self, id: &CStr) -> Option<&'static E> {
        if self.raw.is_null() {
            return None;
        }
        // SAFETY: caller guarantees `self.raw` is live; `get_extension` is
        // `[thread-safe]` per CLAP `host.h`.
        unsafe {
            let get = (*self.raw).get_extension?;
            get(self.raw, id.as_ptr()).cast::<E>().as_ref()
        }
    }
}

impl HostHandle<Any> {
    /// Wrap a CLAP host pointer in the thread-safe handle.
    ///
    /// # Safety
    /// `raw` is either null or a live `clap_host` that outlives every call
    /// made through the returned handle.
    #[must_use]
    pub(crate) unsafe fn any(raw: *const clap_host) -> Self {
        Self {
            raw,
            _phase: PhantomData,
        }
    }

    /// Ask the host to invoke `on_main_thread` (`host.request_callback`,
    /// `[thread-safe]`).
    ///
    /// # Safety
    /// The underlying `clap_host` must still be alive.
    pub(crate) unsafe fn request_callback(self) {
        if !self.raw.is_null() {
            // SAFETY: caller guarantees `self.raw` is live; `request_callback`
            // is `[thread-safe]` per CLAP `host.h`.
            unsafe {
                if let Some(request) = (*self.raw).request_callback {
                    request(self.raw);
                }
            }
        }
    }

    /// Ask the host to deactivate and reactivate the plugin
    /// (`host.request_restart`, `[thread-safe]`).
    ///
    /// # Safety
    /// The underlying `clap_host` must still be alive.
    pub(crate) unsafe fn request_restart(self) {
        if !self.raw.is_null() {
            // SAFETY: caller guarantees `self.raw` is live; `request_restart`
            // is `[thread-safe]` per CLAP `host.h`.
            unsafe {
                if let Some(restart) = (*self.raw).request_restart {
                    restart(self.raw);
                }
            }
        }
    }

    /// Ask the host to flush queued parameter edits (`params.request_flush`,
    /// `[thread-safe]`).
    ///
    /// # Safety
    /// Both `self.raw` and `host_params` must still be alive.
    pub(crate) unsafe fn request_flush(self, host_params: HostParamsHandle) {
        let ext = host_params.as_ptr();
        if self.raw.is_null() || ext.is_null() {
            return;
        }
        // SAFETY: caller guarantees host and extension are live;
        // `request_flush` is `[thread-safe]` per CLAP `ext/params.h`.
        unsafe {
            if let Some(flush) = (*ext).request_flush {
                flush(self.raw);
            }
        }
    }

    /// Log a diagnostic message through the host's `clap.log` extension
    /// (`[thread-safe]`, non-audio paths only since it builds a C string).
    /// Returns `true` when the host accepted the message, `false` when the
    /// host does not expose `clap.log`.
    ///
    /// # Safety
    /// Both `self.raw` and `ext` (if non-null) must still be alive.
    #[must_use]
    pub(crate) unsafe fn log(
        self,
        ext: *const clap_host_log,
        severity: clap_log_severity,
        msg: &str,
    ) -> bool {
        if self.raw.is_null() || ext.is_null() {
            return false;
        }
        // SAFETY: caller guarantees host and extension are live; `log` is
        // `[thread-safe]` per CLAP `ext/log.h`.
        let Some(log_fn) = (unsafe { (*ext).log }) else {
            return false;
        };
        let sanitized: String = msg.chars().filter(|&c| c != '\0').collect();
        let Ok(cstr) = CString::new(sanitized) else {
            return false;
        };
        unsafe { log_fn(self.raw, severity, cstr.as_ptr()) };
        true
    }

    /// Query `clap_host_thread_check::is_main_thread` (`[thread-safe]`), or
    /// `None` if the host does not expose `clap.thread-check`.
    ///
    /// # Safety
    /// Both `self.raw` and `ext` (if non-null) must still be alive.
    #[cfg(any(debug_assertions, test))]
    #[must_use]
    pub(crate) unsafe fn is_main_thread(self, ext: *const clap_host_thread_check) -> Option<bool> {
        if self.raw.is_null() || ext.is_null() {
            return None;
        }
        // SAFETY: caller guarantees host and extension are live;
        // `is_main_thread` is `[thread-safe]` per CLAP `ext/thread-check.h`.
        let check = unsafe { (*ext).is_main_thread }?;
        Some(unsafe { check(self.raw) })
    }

    /// Query `clap_host_thread_check::is_audio_thread` (`[thread-safe]`), or
    /// `None` if the host does not expose `clap.thread-check`.
    ///
    /// # Safety
    /// Both `self.raw` and `ext` (if non-null) must still be alive.
    #[cfg(any(debug_assertions, test))]
    #[must_use]
    pub(crate) unsafe fn is_audio_thread(self, ext: *const clap_host_thread_check) -> Option<bool> {
        if self.raw.is_null() || ext.is_null() {
            return None;
        }
        // SAFETY: caller guarantees host and extension are live;
        // `is_audio_thread` is `[thread-safe]` per CLAP `ext/thread-check.h`.
        let check = unsafe { (*ext).is_audio_thread }?;
        Some(unsafe { check(self.raw) })
    }

    /// Debug-assert that this callback runs on the host's main thread when
    /// the host exposes `clap.thread-check`. No-op in release builds or when
    /// the host omits the extension.
    ///
    /// # Safety
    /// Both `self.raw` and `ext` (if non-null) must still be alive.
    pub(crate) unsafe fn debug_assert_main_thread(self, ext: *const clap_host_thread_check) {
        #[cfg(debug_assertions)]
        debug_assert_ne!(
            unsafe { self.is_main_thread(ext) },
            Some(false),
            "CLAP [main-thread] callback invoked off the host main thread"
        );
        #[cfg(not(debug_assertions))]
        let _ = (self, ext);
    }

    /// Debug-assert that this callback runs on the host's audio thread when
    /// the host exposes `clap.thread-check`. No-op in release builds or when
    /// the host omits the extension.
    ///
    /// # Safety
    /// Both `self.raw` and `ext` (if non-null) must still be alive.
    pub(crate) unsafe fn debug_assert_audio_thread(self, ext: *const clap_host_thread_check) {
        #[cfg(debug_assertions)]
        debug_assert_ne!(
            unsafe { self.is_audio_thread(ext) },
            Some(false),
            "CLAP [audio-thread] callback invoked off the host audio thread"
        );
        #[cfg(not(debug_assertions))]
        let _ = (self, ext);
    }

    /// Narrow to the main-thread phase using a [`MainThreadToken`] proof.
    #[must_use]
    pub(crate) fn on_main_thread(self, _main: &MainThreadToken) -> HostHandle<MainThread> {
        HostHandle {
            raw: self.raw,
            _phase: PhantomData,
        }
    }

    /// Narrow to the audio-thread phase.
    #[must_use]
    pub(crate) fn on_audio_thread(self) -> HostHandle<Audio> {
        HostHandle {
            raw: self.raw,
            _phase: PhantomData,
        }
    }
}

impl HostHandle<MainThread> {
    /// Widen back to the thread-safe subset.
    #[must_use]
    pub(crate) fn as_any(self) -> HostHandle<Any> {
        HostHandle {
            raw: self.raw,
            _phase: PhantomData,
        }
    }

    /// `[main-thread]` `state.mark_dirty`, when the host exposes `clap.state`.
    ///
    /// # Safety
    /// The underlying host and `ext` (if non-null) must still be alive.
    pub(crate) unsafe fn mark_dirty(self, ext: *const clap_host_state) {
        if !self.raw.is_null() && !ext.is_null() {
            // SAFETY: caller guarantees host and extension are live;
            // `mark_dirty` is `[main-thread]` per `ext/state.h`.
            unsafe {
                if let Some(mark_dirty) = (*ext).mark_dirty {
                    mark_dirty(self.raw);
                }
            }
        }
    }

    /// `[main-thread & being-activated]` `latency.changed`, when the host
    /// exposes `clap.latency`.
    ///
    /// # Safety
    /// The underlying host and `ext` (if non-null) must still be alive, and the
    /// caller must be inside `clap_plugin.activate`.
    pub(crate) unsafe fn latency_changed(self, ext: *const clap_host_latency) {
        if !self.raw.is_null() && !ext.is_null() {
            // SAFETY: caller guarantees host and extension are live;
            // `changed` is `[main-thread]` per `ext/latency.h`.
            unsafe {
                if let Some(changed) = (*ext).changed {
                    changed(self.raw);
                }
            }
        }
    }

    /// `[main-thread]` `params.rescan`, when the host exposes `clap.params`.
    ///
    /// # Safety
    /// The underlying host and `ext` (if non-null) must still be alive.
    pub(crate) unsafe fn params_rescan(self, ext: *const clap_host_params, flags: u32) {
        if !self.raw.is_null() && !ext.is_null() {
            // SAFETY: caller guarantees host and extension are live;
            // `rescan` is `[main-thread]` per `ext/params.h`.
            unsafe {
                if let Some(rescan) = (*ext).rescan {
                    rescan(self.raw, flags);
                }
            }
        }
    }

    /// `[main-thread]` `params.clear`, when the host exposes `clap.params`.
    ///
    /// # Safety
    /// The underlying host and `ext` (if non-null) must still be alive.
    pub(crate) unsafe fn params_clear(self, ext: *const clap_host_params, id: clap_id, flags: u32) {
        if !self.raw.is_null() && !ext.is_null() {
            // SAFETY: caller guarantees host and extension are live;
            // `clear` is `[main-thread]` per `ext/params.h`.
            unsafe {
                if let Some(clear) = (*ext).clear {
                    clear(self.raw, id, flags);
                }
            }
        }
    }

    /// `[main-thread]` `gui.closed`, when the host exposes `clap.gui`.
    ///
    /// # Safety
    /// The underlying host must still be alive.
    pub(crate) unsafe fn gui_closed(self, was_destroyed: bool) {
        // SAFETY: caller guarantees host is live; `closed` is `[main-thread]`
        // per `ext/gui.h`.
        unsafe {
            if let Some(ext) = self.extension::<clap_host_gui>(CLAP_EXT_GUI)
                && let Some(closed) = ext.closed
            {
                closed(self.raw, was_destroyed);
            }
        }
    }

    /// `[main-thread]` `audio_ports.rescan`, when the host exposes it.
    ///
    /// # Safety
    /// The underlying host must still be alive.
    pub(crate) unsafe fn audio_ports_rescan(self, flags: u32) {
        // SAFETY: caller guarantees host is live; `rescan` is `[main-thread]`
        // per `ext/audio-ports.h`.
        unsafe {
            if let Some(ext) = self.extension::<clap_host_audio_ports>(CLAP_EXT_AUDIO_PORTS)
                && let Some(rescan) = ext.rescan
            {
                rescan(self.raw, flags);
            }
        }
    }

    /// `[main-thread]` `note_name.changed`, when the host exposes it.
    ///
    /// # Safety
    /// The underlying host must still be alive.
    pub(crate) unsafe fn note_name_changed(self) {
        // SAFETY: caller guarantees host is live; `changed` is `[main-thread]`
        // per `ext/note-name.h`.
        unsafe {
            if let Some(ext) = self.extension::<clap_host_note_name>(CLAP_EXT_NOTE_NAME)
                && let Some(changed) = ext.changed
            {
                changed(self.raw);
            }
        }
    }

    /// `[main-thread]` `voice_info.changed`, when the host exposes it.
    ///
    /// # Safety
    /// The underlying host must still be alive.
    pub(crate) unsafe fn voice_info_changed(self) {
        // SAFETY: caller guarantees host is live; `changed` is `[main-thread]`
        // per `ext/voice-info.h`.
        unsafe {
            if let Some(ext) = self.extension::<clap_host_voice_info>(CLAP_EXT_VOICE_INFO)
                && let Some(changed) = ext.changed
            {
                changed(self.raw);
            }
        }
    }

    /// `[main-thread]` `remote_controls.changed`, when the host exposes it.
    ///
    /// # Safety
    /// The underlying host must still be alive.
    pub(crate) unsafe fn remote_controls_changed(self) {
        // SAFETY: caller guarantees host is live; `changed` is `[main-thread]`
        // per `ext/remote-controls.h`.
        unsafe {
            if let Some(ext) = self.extension::<clap_host_remote_controls>(CLAP_EXT_REMOTE_CONTROLS)
                && let Some(changed) = ext.changed
            {
                changed(self.raw);
            }
        }
    }

    /// `[main-thread]` `preset_load.loaded`, when the host exposes it.
    ///
    /// # Safety
    /// The underlying host must still be alive and `location` / `load_key`
    /// must satisfy `clap_host_preset_load.loaded`'s C string contract.
    pub(crate) unsafe fn preset_loaded(
        self,
        location_kind: u32,
        location: *const c_char,
        load_key: *const c_char,
    ) {
        // SAFETY: caller guarantees host and string pointers are valid;
        // `loaded` is `[main-thread]` per `ext/preset-load.h`.
        unsafe {
            if let Some(ext) = self.extension::<clap_host_preset_load>(CLAP_EXT_PRESET_LOAD)
                && let Some(loaded) = ext.loaded
            {
                loaded(self.raw, location_kind, location, load_key);
            }
        }
    }

    /// `[main-thread]` `track_info.get`, when the host exposes it.
    ///
    /// # Safety
    /// The underlying host and `ext` (if non-null) must still be alive.
    #[must_use]
    pub(crate) unsafe fn track_info_get(
        self,
        ext: *const clap_host_track_info,
        out: &mut clap_track_info,
    ) -> bool {
        if self.raw.is_null() || ext.is_null() {
            return false;
        }
        // SAFETY: caller guarantees host and extension are live; `get` is
        // `[main-thread]` per `ext/track-info.h`.
        unsafe { (*ext).get.is_some_and(|get| get(self.raw, out)) }
    }
}

impl HostHandle<Audio> {
    /// Widen back to the thread-safe subset.
    #[must_use]
    pub(crate) fn as_any(self) -> HostHandle<Any> {
        HostHandle {
            raw: self.raw,
            _phase: PhantomData,
        }
    }

    /// `[audio-thread]` `tail.changed`, when the host exposes `clap.tail`.
    ///
    /// # Safety
    /// The underlying host and `ext` (if non-null) must still be alive.
    pub(crate) unsafe fn tail_changed(self, ext: *const clap_host_tail) {
        if !self.raw.is_null() && !ext.is_null() {
            // SAFETY: caller guarantees host and extension are live;
            // `changed` is `[audio-thread]` per `ext/tail.h`.
            unsafe {
                if let Some(changed) = (*ext).changed {
                    changed(self.raw);
                }
            }
        }
    }
}

/// Thread-safe handle to the host's `clap_host_params` extension pointer.
///
/// Only `[thread-safe]` calls (`request_flush`) are made through it off the
/// main thread.
#[derive(Clone, Copy)]
pub(crate) struct HostParamsHandle(*const clap_host_params);

// SAFETY: the host's `clap_host_params` struct is immutable for the lifetime
// of the host, and `HostHandle<Any>::request_flush` invokes only the
// `[thread-safe]` `request_flush` callback inside `HostQueue::with_open`.
unsafe impl Send for HostParamsHandle {}
unsafe impl Sync for HostParamsHandle {}

impl HostParamsHandle {
    /// # Safety
    /// `raw` is null or a live `clap_host_params` pointer that outlives use.
    #[must_use]
    pub(crate) unsafe fn new(raw: *const clap_host_params) -> Self {
        Self(raw)
    }

    #[must_use]
    pub(crate) fn as_ptr(self) -> *const clap_host_params {
        self.0
    }
}
