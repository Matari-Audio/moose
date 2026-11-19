//! Plugin-state save / restore helpers layered on the canonical wire
//! format in [`moose_utils::state`]. The wire functions are
//! re-exported here so format wrappers and plugin code keep a single
//! import path; the envelope itself lives in `moose-utils` so
//! `cargo-moose` can emit byte-identical blobs (factory preset files)
//! without inheriting `moose-core`'s runtime dependency chain.

pub use moose_utils::state_stream::{
    state_stream_size, write_state_stream, write_state_stream_with_persist,
};

pub use moose_utils::state::{
    DeserializedState, StateParse, deserialize_state, hash_plugin_id, parse_state,
    resolve_vst3_cid, serialize_state, vst3_cid,
};

/// The plugin format whose wrapper found a foreign state blob.
/// Carried in [`ForeignState::Raw`] so a `migrate_state`
/// implementation can branch per format when the old builds
/// serialized differently per format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PluginFormat {
    Clap,
    Vst3,
    Vst2,
    Au,
    Lv2,
    Aax,
}

/// What a format wrapper found where moose state should have been.
/// Handed to [`crate::plugin::PluginRuntime::migrate_state`] on the
/// host thread; the plugin decides whether it recognizes the bytes.
pub enum ForeignState<'a> {
    /// Bytes that aren't a moose envelope: a previous framework's
    /// state, exactly as the old build saved it.
    Raw {
        format: PluginFormat,
        /// The container key the bytes were found under, for keyed
        /// formats (AU dict key, LV2 property URI, AAX chunk id).
        /// `None` for stream formats (CLAP / VST3 / VST2).
        source_key: Option<&'a str>,
        bytes: &'a [u8],
    },
    /// A valid moose envelope whose `plugin_id_hash` doesn't match:
    /// the plugin was renamed / re-identified. Params, extra, and the
    /// `#[persist]` block are already decoded; the plugin only decides
    /// whether to accept them. Forward `persist` into the returned
    /// [`MigratedState`] to keep GUI layout / file paths / instance
    /// names across the rename.
    MismatchedEnvelope {
        plugin_id_hash: u64,
        params: &'a [(u32, f64)],
        extra: Option<&'a [u8]>,
        persist: &'a [u8],
    },
}

/// Moose-shaped state produced by a successful
/// [`crate::plugin::PluginRuntime::migrate_state`]. Rides the normal
/// restore pipeline as a synthetic [`DeserializedState`]; the next
/// save writes a regular envelope, so migration is a one-shot door,
/// not a permanent dual-format reader.
#[derive(Default)]
pub struct MigratedState {
    pub params: Vec<(u32, f64)>,
    pub extra: Option<Vec<u8>>,
    /// Serialized `#[persist]` block to restore. Leave empty when the
    /// source has no moose persist data (a foreign framework's blob);
    /// forward [`ForeignState::MismatchedEnvelope::persist`] to carry
    /// a renamed plugin's persisted fields across the migration.
    pub persist: Vec<u8>,
}

impl From<MigratedState> for DeserializedState {
    fn from(migrated: MigratedState) -> Self {
        Self {
            params: migrated.params,
            extra: migrated.extra,
            persist: migrated.persist,
        }
    }
}

/// Reason a [`crate::PluginRuntime::load_state`] /
/// `moose_plugin::PluginLogic::load_state` implementation failed to
/// interpret the host-supplied extra-state blob. Format wrappers
/// receive this on the audio-thread apply path and log it; hosts
/// that surface a non-success code to the DAW (e.g. CLAP
/// `state_load` returning `false`) read the variant via that path.
///
/// `Malformed` is the typical case: the blob's framing or content
/// doesn't match what `save_state` would emit (version skew between
/// older session files and newer plugin builds is the canonical
/// example). `Other` carries a free-form message for plugin-specific
/// failures that don't fit the malformed-bytes shape.
#[derive(Debug)]
#[non_exhaustive]
pub enum StateLoadError {
    /// State blob is too short, mis-framed, or otherwise unparseable.
    Malformed(&'static str),
    /// Plugin-specific failure with a free-form message.
    Other(String),
}

impl std::fmt::Display for StateLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(s) => write!(f, "malformed state: {s}"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for StateLoadError {}

/// Apply a deserialized state to a plugin: write parameter values,
/// snap smoothers, then hand the optional extra blob to
/// [`crate::plugin::PluginRuntime::load_state`].
///
/// Format wrappers call this from the audio thread after popping a
/// pending load off their per-instance handoff queue. The reason it
/// must run on the audio thread (and not on the host's main thread,
/// where state-load callbacks are typically invoked): `load_state`
/// takes `&mut P`, which would alias the audio thread's `&mut P`
/// inside `process()` and produce a data race. The audio thread is
/// the single thread that already owns `&mut P` between blocks, so
/// running the load there sidesteps the race entirely.
///
/// `restore_values` and `snap_smoothers` go through the param
/// struct's interior atomics, so they don't strictly need to run on
/// the audio thread - but applying them here keeps the param values and
/// the user's extra-state blob coherent for any observer reading after
/// this returns.
///
/// `#[persist]` fields are **not** applied here - `load_persist` takes a
/// lock per field that a GUI reader can hold, so it would risk blocking
/// the audio thread. [`apply_params`] applies persist on the host thread;
/// the deferred wrappers call it before queueing, and a host-thread full
/// load (the standalone host, an inactive-plugin load) calls it alongside
/// this.
pub fn apply_state<P: crate::export::PluginExport>(plugin: &mut P, state: &DeserializedState) {
    if let Err(e) = try_apply_state(plugin, state) {
        // Debug-only breadcrumb: `eprintln!` locks stderr and allocates.
        // Wrappers that can report the error use `try_apply_state`.
        #[cfg(debug_assertions)]
        eprintln!("moose: load_state failed: {e}");
        #[cfg(not(debug_assertions))]
        let _ = e;
    }
}

/// [`apply_state`], returning the plugin's `load_state` error so a
/// wrapper that applies synchronously can report it to the host.
///
/// # Errors
/// Forwards the plugin's [`crate::plugin::PluginRuntime::load_state`] error.
pub fn try_apply_state<P: crate::export::PluginExport>(
    plugin: &mut P,
    state: &DeserializedState,
) -> Result<(), StateLoadError> {
    use moose_params::Params;
    plugin.params().restore_values(&state.params);
    plugin.params().snap_smoothers();
    match &state.extra {
        Some(extra) => plugin.load_state(extra),
        None => Ok(()),
    }
}

/// Parameter values and `#[persist]` as they were before a load, so a load
/// whose `load_state` fails is undone as a unit (params, persist and custom
/// state all stay at the previous state). Captured on the host thread.
///
/// ponytail: copies the whole `#[persist]` blob per load; a store with
/// large persist fields pays that copy. Snapshot by `Arc` if it shows up.
pub struct ParamsBackup {
    values: Vec<(u32, f64)>,
    persist: Vec<u8>,
}

impl ParamsBackup {
    /// Capture the store's current values and `#[persist]` blob.
    #[must_use]
    pub fn capture<P: moose_params::Params>(params: &P) -> Self {
        let (ids, values) = params.collect_values();
        Self {
            values: ids.into_iter().zip(values).collect(),
            persist: params.serialize_persist(),
        }
    }

    /// Put the captured values and `#[persist]` back.
    pub fn restore<P: moose_params::Params>(&self, params: &P) {
        params.restore_state(&self.values, &self.persist);
        params.snap_smoothers();
    }
}

/// Parse a host-supplied state blob and, when it isn't this plugin's
/// envelope, offer it to the plugin's
/// [`crate::plugin::PluginRuntime::migrate_state`] hook. One routing
/// point for every format wrapper's state callback:
///
/// - a matching envelope loads as always;
/// - foreign bytes ([`StateParse::NotAnEnvelope`]) and renamed-plugin
///   envelopes ([`StateParse::WrongPlugin`]) go to `migrate_state`;
/// - a future envelope version and a corrupt envelope fail the load
///   (never handed to the plugin), each with its own log line.
///
/// `None` means the load failed and the wrapper must report failure
/// to the host in its own idiom. Runs on the host thread - that's
/// where `migrate_state` is allowed to do allocator-heavy parsing.
#[must_use]
pub fn parse_or_migrate<P: PluginExport>(
    data: &[u8],
    expected_plugin_id: u64,
    format: PluginFormat,
    source_key: Option<&str>,
) -> Option<DeserializedState> {
    finish_parse_or_migrate::<P>(
        parse_state(data, expected_plugin_id),
        data,
        format,
        source_key,
    )
}

fn finish_parse_or_migrate<P: PluginExport>(
    parsed: StateParse,
    data: &[u8],
    format: PluginFormat,
    source_key: Option<&str>,
) -> Option<DeserializedState> {
    let state = match parsed {
        StateParse::Ok(state) => Some(state),
        StateParse::NotAnEnvelope => P::migrate_state(&ForeignState::Raw {
            format,
            source_key,
            bytes: data,
        })
        .map(Into::into),
        StateParse::WrongPlugin { found, state } => {
            P::migrate_state(&ForeignState::MismatchedEnvelope {
                plugin_id_hash: found,
                params: &state.params,
                extra: state.extra.as_deref(),
                persist: &state.persist,
            })
            .map(Into::into)
        }
        StateParse::UnknownVersion(version) => {
            // Same logging rationale as `apply_state`: one-shot event,
            // no `log` dep in the audio-runtime crate.
            eprintln!(
                "moose: state blob carries envelope version {version}; this build \
                 reads version 1 - load failed"
            );
            None
        }
        StateParse::Corrupt => {
            eprintln!("moose: state blob is a corrupt moose envelope - load failed");
            None
        }
    }?;
    // Reject before any wrapper touches the params: a blob the store
    // can't read must not leave it half-restored.
    if !<P::Params as moose_params::Params>::validate_persist(&state.persist) {
        eprintln!("moose: saved #[persist] state failed validation - load failed");
        return None;
    }
    Some(state)
}

/// Read canonical state incrementally into the final state lanes. Foreign
/// blobs alone retain the bounded whole-blob migration fallback.
pub fn read_or_migrate<P: PluginExport>(
    read: impl FnMut(&mut [u8]) -> i64,
    expected_plugin_id: u64,
    format: PluginFormat,
) -> Option<DeserializedState> {
    use moose_utils::state_stream::{StreamState, read_state_stream};
    match read_state_stream(read, P::max_host_state_bytes())? {
        StreamState::Foreign(bytes) => {
            parse_or_migrate::<P>(&bytes, expected_plugin_id, format, None)
        }
        StreamState::Envelope(StateParse::WrongPlugin { found, state })
            if found == expected_plugin_id =>
        {
            finish_parse_or_migrate::<P>(StateParse::Ok(state), &[], format, None)
        }
        StreamState::Envelope(parsed) => finish_parse_or_migrate::<P>(parsed, &[], format, None),
    }
}

/// Prepare large persistence fields directly from a bounded host stream, then
/// commit on the host thread only after framing, EOF and store validation pass.
/// Formats with native borrowed containers may pass that container's existing
/// bound; CLAP/VST3 pass the plugin's explicit stream policy.
pub fn read_and_apply_params<P: PluginExport>(
    read: impl FnMut(&mut [u8]) -> i64,
    expected_plugin_id: u64,
    format: PluginFormat,
    source_key: Option<&str>,
    params: &P::Params,
    limit: usize,
) -> Option<DeserializedState> {
    use moose_utils::state_stream::{StreamState, read_state_stream_prepared};
    let (stream, prepared) = read_state_stream_prepared(read, limit, |id, input, len| {
        if id == expected_plugin_id {
            <P::Params as Params>::prepare_persist_stream(input, len)
        } else {
            None
        }
    })?;
    let state = match stream {
        StreamState::Foreign(bytes) => {
            parse_or_migrate::<P>(&bytes, expected_plugin_id, format, source_key)
        }
        StreamState::Envelope(StateParse::WrongPlugin { found, state })
            if found == expected_plugin_id =>
        {
            finish_parse_or_migrate::<P>(StateParse::Ok(state), &[], format, source_key)
        }
        StreamState::Envelope(parsed) => {
            finish_parse_or_migrate::<P>(parsed, &[], format, source_key)
        }
    }?;
    if let Some(commit) = prepared {
        commit(params, &state.params, &state.persist);
        params.snap_smoothers();
    } else {
        apply_params(params, &state);
    }
    Some(state)
}

/// Apply just the parameter values from a deserialized state - the
/// host-thread-safe subset of [`apply_state`]. Format wrappers call
/// this from their state-load callback (host main thread) before
/// pushing the full state onto the audio-thread handoff queue, so
/// host-thread reads of `getParameter`/equivalents see the restored
/// values immediately. Validators (auval, pluginval, the VST2 binary
/// smoke) read parameters synchronously after `setChunk`/equivalents
/// without first running a render block, and would otherwise see the
/// pre-restore values until the audio thread caught up.
///
/// The extra blob still has to round-trip through the audio thread
/// because [`crate::plugin::PluginRuntime::load_state`] takes `&mut P`, which
/// would alias `process()`'s `&mut P` if called from the host thread.
/// `restore_values` and `snap_smoothers` go through atomic interior
/// mutability and are safe to call concurrently with `process()`.
///
/// `#[persist]` fields are applied here too, and deliberately *not* on the
/// audio thread: `load_persist` takes a `RwLock::write` / `Mutex::lock`
/// per persisted field - fields a GUI thread reads at frame rate - so
/// applying it inside `process()` would let the editor block the audio
/// thread (priority inversion). It also allocates (deserializing
/// `String` / `Vec`). Running it on the host thread keeps both off the
/// real-time path.
pub fn apply_params<P: moose_params::Params>(params: &P, state: &DeserializedState) {
    if !P::validate_persist(&state.persist) {
        return;
    }
    params.restore_state(&state.params, &state.persist);
    params.snap_smoothers();
}

// ---------------------------------------------------------------------------
// `snapshot_plugin` / `restore_plugin` - high-level helpers wrapping
// `serialize_state` + `deserialize_state` with the params-collect /
// restore + custom-state plumbing every host needs to do anyway.
// ---------------------------------------------------------------------------

use crate::export::{PluginExport, read_custom_state_offthread};
use moose_params::Params;

/// Errors `restore_plugin` can return.
///
/// `Invalid` covers envelope-level failures (missing / wrong magic,
/// version mismatch, plugin-ID mismatch, truncated body); `LoadState`
/// covers a successfully-parsed envelope whose extra-state blob the
/// plugin's [`crate::PluginRuntime::load_state`] rejected. The caller
/// typically prints a diagnostic and proceeds with default params.
#[derive(Debug)]
pub enum RestoreError {
    /// The bytes don't parse as a state envelope for this plugin.
    Invalid,
    /// Envelope parsed but the plugin couldn't interpret its extra
    /// bytes.
    LoadState(StateLoadError),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid => f.write_str("state envelope is invalid"),
            Self::LoadState(e) => write!(f, "plugin load_state failed: {e}"),
        }
    }
}

impl std::error::Error for RestoreError {}

/// Serialize a plugin instance into the canonical state envelope -
/// parameter values + optional custom-state payload, with the magic /
/// version / plugin-ID header `serialize_state` writes.
///
/// Same shape every format wrapper produces, so a `.state` file
/// written by one host loads in any other (subject to the
/// plugin-ID match `deserialize_state` enforces).
pub fn snapshot_plugin<P: PluginExport>(plugin: &P) -> Vec<u8> {
    use moose_params::Params;
    let (ids, values) = plugin.params().collect_values();
    let extra = read_custom_state_offthread(plugin);
    let persist = plugin.params().serialize_persist();
    serialize_state(
        hash_plugin_id(P::info().clap_id),
        &ids,
        &values,
        &extra,
        &persist,
    )
}

/// Inverse of [`snapshot_plugin`]. Validates the envelope's magic,
/// version, and plugin-ID hash; on success restores parameter
/// values via `Params::restore_values` and forwards the optional
/// extra payload to `Plugin::load_state`.
///
/// # Errors
///
/// Returns [`RestoreError::Invalid`] if the magic / version /
/// plugin-ID hash check fails or the envelope is truncated. A
/// successful return guarantees the params and (optional) extra
/// payload were forwarded to the plugin.
pub fn restore_plugin<P: PluginExport>(plugin: &mut P, bytes: &[u8]) -> Result<(), RestoreError> {
    let id = hash_plugin_id(P::info().clap_id);
    let s = deserialize_state(bytes, id).ok_or(RestoreError::Invalid)?;
    if !<P::Params as Params>::validate_persist(&s.persist) {
        return Err(RestoreError::Invalid);
    }
    plugin.params().restore_state(&s.params, &s.persist);
    if let Some(extra) = s.extra {
        plugin.load_state(&extra).map_err(RestoreError::LoadState)?;
    }
    Ok(())
}

/// Resolve the state-envelope hash every format wrapper stamps into
/// the saved blob. Today this is just `hash_plugin_id(info.clap_id)`,
/// which means the same plugin built as CLAP / VST3 / AU / AAX / VST2
/// / LV2 produces a single state space - saving in one host and
/// loading in another will round-trip parameter values (provided the
/// `Plugin::save_state` / `load_state` extra payload is also
/// format-agnostic).
///
/// **Trade-off:** because the input is the CLAP ID, renaming
/// `info.clap_id` invalidates **every** saved session across **every**
/// format. Callers that want format-pinned state (e.g. an AU build
/// that shouldn't share state with the same plugin's CLAP build)
/// should add a per-format ID field to [`crate::PluginInfo`] and
/// route through it instead.
#[must_use]
pub fn shared_plugin_state_hash(info: &crate::PluginInfo) -> u64 {
    hash_plugin_id(info.clap_id)
}

#[cfg(test)]
mod tests {
    use super::{DeserializedState, MigratedState};

    // A renamed plugin that forwards its old envelope's `#[persist]`
    // bytes must keep them: the `From` bridge used to hardcode
    // `persist: Vec::new()`, silently reverting persisted fields to
    // default on every migrated load.
    #[test]
    fn migrated_state_forwards_persist() {
        let migrated = MigratedState {
            params: vec![(0, 1.0)],
            extra: Some(vec![9, 9]),
            persist: vec![1, 2, 3],
        };
        let restored: DeserializedState = migrated.into();
        assert_eq!(restored.persist, vec![1, 2, 3]);
        assert_eq!(restored.extra.as_deref(), Some(&[9, 9][..]));
        assert_eq!(restored.params, vec![(0, 1.0)]);
    }

    #[test]
    fn migrated_state_default_has_empty_persist() {
        let restored: DeserializedState = MigratedState::default().into();
        assert_eq!(restored.persist, []);
        assert!(restored.extra.is_none());
        assert_eq!(restored.params, []);
    }
}

/// Host-produced, single audio-consumer state handoff. The host retains an Arc
/// until it observes that neither the ready slot nor audio owns it. Audio only
/// decrements an Arc; it can never perform the final free of a recalled state.
/// At most one consumed state and one queued state survive a producer call.
/// Persist lanes have already been applied on the host and are released there.
pub struct StateLoadQueue {
    ready: crossbeam_queue::ArrayQueue<std::sync::Arc<DeserializedState>>,
    owners: std::sync::Mutex<Vec<std::sync::Arc<DeserializedState>>>,
    /// Address of the last state whose apply finished (0 = none), so the
    /// host can tell whether its own push has landed. The host keeps an
    /// `Arc` clone while it compares, so the address can't be reused.
    applied: std::sync::atomic::AtomicUsize,
    /// Whether that apply's `load_state` succeeded.
    applied_ok: std::sync::atomic::AtomicBool,
}
impl StateLoadQueue {
    /// Create a single-slot state handoff.
    ///
    /// # Panics
    /// Panics if `capacity` is not one.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert_eq!(capacity, 1, "state loads coalesce in one slot");
        Self {
            ready: crossbeam_queue::ArrayQueue::new(1),
            owners: std::sync::Mutex::new(Vec::with_capacity(3)),
            applied: std::sync::atomic::AtomicUsize::new(0),
            applied_ok: std::sync::atomic::AtomicBool::new(true),
        }
    }
    /// Host thread only. All persist fields must already have been applied.
    pub fn force_push(&self, state: DeserializedState) {
        drop(self.push_tracked(state));
    }
    /// [`Self::force_push`], returning a handle for [`Self::wait_applied`].
    pub fn push_tracked(&self, mut state: DeserializedState) -> std::sync::Arc<DeserializedState> {
        state.persist = Vec::new();
        let state = std::sync::Arc::new(state);
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        owners.push(std::sync::Arc::clone(&state));
        drop(self.ready.force_push(std::sync::Arc::clone(&state)));
        // `> 2`: the ready slot or audio, plus the handle returned below.
        owners.retain(|s| {
            std::sync::Arc::strong_count(s) > 1 + usize::from(std::sync::Arc::ptr_eq(s, &state))
        });
        debug_assert!(owners.len() <= 2, "single audio consumer contract");
        state
    }
    pub fn pop(&self) -> Option<std::sync::Arc<DeserializedState>> {
        self.ready.pop()
    }
    /// Record that `state` (from [`Self::pop`]) has been applied. Two
    /// atomic stores; safe on the audio thread.
    pub fn mark_applied(&self, state: &std::sync::Arc<DeserializedState>, ok: bool) {
        use std::sync::atomic::Ordering;
        self.applied_ok.store(ok, Ordering::Relaxed);
        self.applied
            .store(std::sync::Arc::as_ptr(state) as usize, Ordering::Release);
    }
    fn is_applied(&self, state: &std::sync::Arc<DeserializedState>) -> bool {
        self.applied.load(std::sync::atomic::Ordering::Acquire)
            == std::sync::Arc::as_ptr(state) as usize
    }
    /// Host thread: wait up to `timeout` for the audio thread to apply
    /// `state`. `Some(ok)` is the `load_state` result; `None` means no
    /// block ran in time and the state is still queued.
    #[must_use]
    pub fn wait_applied(
        &self,
        state: &std::sync::Arc<DeserializedState>,
        timeout: std::time::Duration,
    ) -> Option<bool> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.is_applied(state) {
                return Some(self.applied_ok.load(std::sync::atomic::Ordering::Relaxed));
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }
    /// Host thread: the newest pushed state if it has not been applied yet.
    /// A save taken before the audio thread drains the queue reports this
    /// state's custom bytes, since they are what the plugin will hold.
    #[must_use]
    pub fn unapplied(&self) -> Option<std::sync::Arc<DeserializedState>> {
        let owners = self
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        owners
            .last()
            .filter(|state| !self.is_applied(state))
            .cloned()
    }
}

#[cfg(test)]
mod state_handoff_tests {
    use super::*;
    fn state(value: u8) -> DeserializedState {
        DeserializedState {
            params: vec![],
            extra: Some(vec![value; 1024]),
            persist: vec![value; 1024],
        }
    }
    #[test]
    fn audio_cannot_drop_the_last_owner_and_retention_is_bounded() {
        let queue = StateLoadQueue::new(1);
        queue.force_push(state(1));
        let audio = queue.pop().unwrap();
        assert_eq!(audio.persist.len(), 0);
        let weak = std::sync::Arc::downgrade(&audio);
        for _ in 0..100 {
            queue.force_push(state(2));
        }
        assert_eq!(queue.owners.lock().unwrap().len(), 2);
        drop(audio); // Audio never releases the last owner.
        assert!(weak.upgrade().is_some());
        queue.force_push(state(3)); // Host reclaims the retired owner.
        assert!(weak.upgrade().is_none());
        assert_eq!(queue.owners.lock().unwrap().len(), 1);
        let audio = queue.pop().unwrap();
        drop(audio);
        assert_eq!(queue.owners.lock().unwrap().len(), 1);
    }
}
