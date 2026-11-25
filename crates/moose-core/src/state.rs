//! Plugin-state save / restore helpers layered on the canonical wire
//! format in [`moose_utils::state`]. The wire functions are
//! re-exported here so format wrappers and plugin code keep a single
//! import path; the envelope itself lives in `moose-utils` so
//! `cargo-moose` can emit byte-identical blobs (factory preset files)
//! without inheriting `moose-core`'s runtime dependency chain.

pub use moose_utils::state_stream::{
    state_stream_size, state_stream_size_v2, write_state_stream, write_state_stream_v2,
    write_state_stream_v2_with_persist, write_state_stream_with_persist,
};

pub use moose_utils::state::{
    DeserializedState, NamedParam, STATE_VERSION_V1, STATE_VERSION_V2, StateParse,
    deserialize_state, hash_plugin_id, parse_state, resolve_vst3_cid, serialize_state,
    serialize_state_for_schema, serialize_state_v2, vst3_cid,
};

/// Host-reported reason a plugin's state is being saved or loaded.
///
/// Maps CLAP `clap.state-context/2` (`CLAP_STATE_CONTEXT_FOR_*`), CLAP
/// `clap.preset-load`, and VST3 `IStreamAttributes`
/// (`PresetAttributes::kStateType` / `StateType::*`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StateContext {
    /// No context was reported by the host (plain `clap.state`, VST3 without
    /// `IStreamAttributes`, AU, AAX, standalone, or an unrecognized attribute).
    #[default]
    Unknown,
    /// Saving or loading a user/factory preset (CLAP
    /// `CLAP_STATE_CONTEXT_FOR_PRESET`, `clap.preset-load`, VST3
    /// `StateType::kTrackPreset` / `"Preset"`).
    Preset,
    /// Saving or loading a DAW project/session (CLAP
    /// `CLAP_STATE_CONTEXT_FOR_PROJECT`, VST3 `StateType::kProject`).
    Project,
    /// Duplicating an existing plugin instance (CLAP
    /// `CLAP_STATE_CONTEXT_FOR_DUPLICATE`).
    Duplicate,
    /// Copy/paste of plugin state between instances.
    CopyPaste,
    /// Saving or loading the plugin's default state (VST3 `StateType::kDefault`).
    Default,
}

impl StateContext {
    pub(crate) const COUNT: usize = 6;

    #[must_use]
    pub(crate) const fn to_u8(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Preset => 1,
            Self::Project => 2,
            Self::Duplicate => 3,
            Self::CopyPaste => 4,
            Self::Default => 5,
        }
    }
}

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
    ///
    /// An OAST v2 envelope carries its entries in `named_params` (stable
    /// string ID plus numeric fallback) and its plugin schema in
    /// `schema_version`; `params` holds only unnamed numeric entries (all
    /// of a v1 envelope's). Forward both into the [`MigratedState`] so the
    /// entries resolve by string ID and the schema migration chain starts
    /// from the saved schema, not from 0.
    MismatchedEnvelope {
        plugin_id_hash: u64,
        params: &'a [(u32, f64)],
        named_params: &'a [NamedParam],
        schema_version: u32,
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
    /// String-keyed entries, resolved against the parameter catalog by
    /// stable string ID (numeric fallback when the key no longer exists)
    /// like a native v2 load. Forward
    /// [`ForeignState::MismatchedEnvelope::named_params`] here, or build
    /// entries with [`NamedParam::new`]. Default: empty.
    pub named_params: Vec<NamedParam>,
    /// Plugin state schema of the returned entries; the plugin's
    /// [`StateMigrationChain`] upgrades from here. `0` (the default) is
    /// the pre-schema baseline - right for a foreign framework's blob.
    /// Forward [`ForeignState::MismatchedEnvelope::schema_version`] for a
    /// renamed plugin.
    pub schema_version: u32,
}

impl From<MigratedState> for DeserializedState {
    fn from(migrated: MigratedState) -> Self {
        Self {
            params: migrated.params,
            named_params: migrated.named_params,
            schema_version: migrated.schema_version,
            extra: migrated.extra,
            persist: migrated.persist,
        }
    }
}

/// One deterministic step `from_version -> from_version + 1` in a
/// [`StateMigrationChain`].
#[derive(Clone, Copy)]
pub struct StateMigrationStep {
    pub from_version: u32,
    pub to_version: u32,
    pub migrate: fn(&mut DeserializedState) -> Result<(), StateLoadError>,
}

/// Composable schema-version migration chain (`v -> v + 1`) executed on the
/// host thread when restoring an older state envelope.
#[derive(Clone, Default)]
pub struct StateMigrationChain {
    target_version: u32,
    steps: Vec<StateMigrationStep>,
}

impl StateMigrationChain {
    /// Start an empty migration chain targeting `target_version`.
    #[must_use]
    pub fn new(target_version: u32) -> Self {
        Self {
            target_version,
            steps: Vec::new(),
        }
    }

    /// Register a migration step from `from_version` to `from_version + 1`.
    ///
    /// # Panics
    /// Panics if `from_version == u32::MAX` or if a step for `from_version`
    /// is already registered.
    #[must_use]
    pub fn step(
        mut self,
        from_version: u32,
        migrate: fn(&mut DeserializedState) -> Result<(), StateLoadError>,
    ) -> Self {
        let to_version = from_version
            .checked_add(1)
            .expect("state migration from_version overflow");
        assert!(
            self.steps.iter().all(|s| s.from_version != from_version),
            "duplicate state migration step for version {from_version}"
        );
        self.steps.push(StateMigrationStep {
            from_version,
            to_version,
            migrate,
        });
        self
    }

    #[must_use]
    pub fn target_version(&self) -> u32 {
        self.target_version
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Upgrade `state` step-by-step from `state.schema_version` to
    /// `self.target_version`.
    ///
    /// # Errors
    /// Returns [`StateLoadError::Malformed`] if `state.schema_version` is
    /// newer than `target_version` or a required step is missing, or forwards
    /// any error returned by a step.
    pub fn run(&self, state: &mut DeserializedState) -> Result<(), StateLoadError> {
        if state.schema_version > self.target_version {
            return Err(StateLoadError::Malformed(
                "state schema version is newer than this plugin",
            ));
        }
        while state.schema_version < self.target_version {
            let current = state.schema_version;
            let Some(step) = self.steps.iter().find(|s| s.from_version == current) else {
                return Err(StateLoadError::Malformed("missing state migration step"));
            };
            (step.migrate)(state)?;
            state.schema_version = step.to_version;
        }
        Ok(())
    }
}

/// Resolve v2 string-keyed parameter entries against `catalog`, updating
/// `state.params` so string-matched entries win over stale numeric IDs while
/// preserving fallback numeric entries only when their string key did not
/// resolve. Never leaks a resolved record's old fallback ID onto another
/// parameter that later reused that numeric ID.
pub fn resolve_named_params(state: &mut DeserializedState, catalog: &moose_params::ParamCatalog) {
    if state.named_params.is_empty() {
        return;
    }
    let mut resolved: Vec<(u32, f64)> =
        Vec::with_capacity(state.params.len() + state.named_params.len());
    let mut claimed_ids = std::collections::HashSet::with_capacity(state.named_params.len());
    let mut unresolved_fallbacks: Vec<(u32, f64)> = Vec::new();
    // Pass 1: authoritative string-ID matches. Once a record matches by string
    // key, its old fallback numeric ID is discarded so it cannot overwrite a
    // different parameter that reused that numeric ID.
    for entry in &state.named_params {
        if let Some(id) = catalog.id_by_string(&entry.key) {
            if claimed_ids.insert(id) {
                resolved.push((id, entry.value));
            }
        } else if let Some(fallback_id) = entry.fallback_id {
            unresolved_fallbacks.push((fallback_id, entry.value));
        }
    }
    // Pass 2: numeric fallback for named records whose string key did not match.
    for (id, value) in unresolved_fallbacks {
        if catalog.by_id(id).is_some() && claimed_ids.insert(id) {
            resolved.push((id, value));
        }
    }
    // Pass 3: unnamed (v1 or empty-key v2) numeric entries.
    for &(id, value) in &state.params {
        if catalog.by_id(id).is_some() && claimed_ids.insert(id) {
            resolved.push((id, value));
        }
    }
    state.params = resolved;
    state.named_params.clear();
}

/// Reason a [`crate::PluginRuntime::load_state`] /
/// `moose_plugin::PluginLogicCore::load_state` implementation failed to
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

/// Type-erased custom state decoded off the audio thread for zero-allocation,
/// lock-free swap on the audio thread.
///
/// Plugins opt in by returning `Ok(Some(DecodedState::new(state)))` from
/// `decode_state` (host thread) and calling [`Self::swap_with`] inside
/// `apply_decoded_state` (audio thread). The displaced old state remains
/// inside this wrapper and is retired and dropped off the audio thread.
pub struct DecodedState(Box<dyn std::any::Any + Send>);

impl std::fmt::Debug for DecodedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DecodedState(..)")
    }
}

impl DecodedState {
    /// Wrap a newly decoded state value constructed off the audio thread.
    #[must_use]
    pub fn new<T: Send + 'static>(value: T) -> Self {
        Self(Box::new(value))
    }

    /// Zero-allocation empty placeholder (`Box<()>`).
    #[must_use]
    pub fn empty() -> Self {
        Self::new(())
    }

    /// Borrow the decoded state if its concrete type is `T`.
    #[must_use]
    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }

    /// Mutably borrow the decoded state if its concrete type is `T`.
    #[must_use]
    pub fn downcast_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.0.downcast_mut::<T>()
    }

    /// Swap the decoded `T` with `target` in place.
    ///
    /// Leaves the previous `target` value inside `self` so the wrapper retires
    /// and drops it off the audio thread. Real-time safe: zero allocations,
    /// zero deallocations, and zero locks.
    pub fn swap_with<T: Send + 'static>(&mut self, target: &mut T) -> bool {
        if let Some(inner) = self.0.downcast_mut::<T>() {
            std::mem::swap(inner, target);
            true
        } else {
            false
        }
    }
}

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
    try_apply_state_with_context(plugin, state, StateContext::Unknown)
}

/// [`try_apply_state`] carrying the host-reported [`StateContext`].
///
/// # Errors
/// Forwards the plugin's
/// [`crate::plugin::PluginRuntime::load_state_with_context`] error.
pub fn try_apply_state_with_context<P: crate::export::PluginExport>(
    plugin: &mut P,
    state: &DeserializedState,
    context: StateContext,
) -> Result<(), StateLoadError> {
    use moose_params::Params;
    plugin.params().restore_values(&state.params);
    plugin.params().snap_smoothers();
    plugin.load_state_with_context(state.extra.as_deref().unwrap_or(&[]), context)
}

/// Parameter values and `#[persist]` as they were before a load, so a load
/// whose `load_state` fails is undone as a unit (params, persist and custom
/// state all stay at the previous state). Captured on the host thread.
///
/// ponytail: copies the whole `#[persist]` blob per load; a store with
/// large persist fields pays that copy. Snapshot by `Arc` if it shows up.
#[derive(Clone)]
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

/// RAII rollback guard that restores a [`ParamsBackup`] on drop unless
/// [`Self::commit`] is called — keeping pre-applied parameter mutations atomic
/// across both `Err` returns and caught panics inside plugin state hooks.
pub struct ParamsRollbackGuard<'a, P: moose_params::Params> {
    backup: &'a ParamsBackup,
    params: &'a P,
    committed: bool,
}

impl<'a, P: moose_params::Params> ParamsRollbackGuard<'a, P> {
    /// Arm rollback restoration of `backup` onto `params` until [`Self::commit`].
    #[must_use]
    pub fn new(backup: &'a ParamsBackup, params: &'a P) -> Self {
        Self {
            backup,
            params,
            committed: false,
        }
    }

    /// Disarm rollback once the state load has been accepted.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl<P: moose_params::Params> Drop for ParamsRollbackGuard<'_, P> {
    fn drop(&mut self) {
        if !self.committed {
            self.backup.restore(self.params);
        }
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
    let mut state = route_parsed_state(parsed, data, format, source_key, P::migrate_state)?;
    if let Err(err) = migrate_to_current::<P>(&mut state) {
        eprintln!("moose: {err} - load failed");
        return None;
    }
    Some(state)
}

/// Turn a parse outcome into state to restore, offering foreign bytes and
/// renamed-plugin envelopes to `migrate` (the plugin's `migrate_state`).
fn route_parsed_state(
    parsed: StateParse,
    data: &[u8],
    format: PluginFormat,
    source_key: Option<&str>,
    migrate: impl FnOnce(&ForeignState<'_>) -> Option<MigratedState>,
) -> Option<DeserializedState> {
    match parsed {
        StateParse::Ok(state) => Some(state),
        StateParse::NotAnEnvelope => migrate(&ForeignState::Raw {
            format,
            source_key,
            bytes: data,
        })
        .map(Into::into),
        StateParse::WrongPlugin { found, state } => migrate(&ForeignState::MismatchedEnvelope {
            plugin_id_hash: found,
            params: &state.params,
            named_params: &state.named_params,
            schema_version: state.schema_version,
            extra: state.extra.as_deref(),
            persist: &state.persist,
        })
        .map(Into::into),
        StateParse::UnknownVersion(version) => {
            // Same logging rationale as `apply_state`: one-shot event,
            // no `log` dep in the audio-runtime crate.
            eprintln!(
                "moose: state blob carries envelope version {version}; this build \
                 reads versions 1..=2 - load failed"
            );
            None
        }
        StateParse::Corrupt => {
            eprintln!("moose: state blob is a corrupt moose envelope - load failed");
            None
        }
    }
}

/// Bring a decoded envelope up to this plugin's state schema: run
/// [`crate::plugin::PluginRuntime::state_migrations`] (`v -> v + 1`) when
/// the saved schema differs, then validate the `#[persist]` block. The one
/// migration gate shared by host state loads, [`restore_plugin`] and preset
/// loads. Runs on the host thread, before anything touches live parameters,
/// so a blob the store can't read never leaves it half-restored.
///
/// # Errors
/// The failing migration step's error, or [`StateLoadError::Malformed`] when
/// the saved schema is newer than the plugin, a step is missing, or the
/// persist block fails validation.
pub fn migrate_to_current<P: PluginExport>(
    state: &mut DeserializedState,
) -> Result<(), StateLoadError> {
    let chain = P::state_migrations();
    if state.schema_version != chain.target_version() {
        chain.run(state)?;
    }
    if !<P::Params as moose_params::Params>::validate_persist(&state.persist) {
        return Err(StateLoadError::Malformed(
            "saved #[persist] state failed validation",
        ));
    }
    Ok(())
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
    let expected_schema = P::state_schema_version();
    // Only use zero-copy prepared persistence streaming when the incoming
    // envelope already matches the plugin's current schema version; older
    // schemas keep their raw persist bytes so `StateMigrationChain` can
    // migrate persisted fields before `apply_params` commits them.
    let (stream, prepared) = read_state_stream_prepared(read, limit, |id, schema, input, len| {
        if id == expected_plugin_id && schema == expected_schema {
            <P::Params as Params>::prepare_persist_stream(input, len)
        } else {
            None
        }
    })?;
    let mut state = match stream {
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
    if !state.named_params.is_empty() {
        resolve_named_params(&mut state, &params.param_catalog());
    }
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
    if state.named_params.is_empty() {
        params.restore_state(&state.params, &state.persist);
    } else {
        let mut resolved = state.clone();
        resolve_named_params(&mut resolved, &params.param_catalog());
        params.restore_state(&resolved.params, &resolved.persist);
    }
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

/// Compute the exact serialized byte size for a plugin's state stream (v1 when
/// `P::state_schema_version() == 0`, v2 otherwise).
#[must_use]
pub fn plugin_state_stream_size<P: PluginExport>(
    params: &P::Params,
    ids: &[u32],
    values: &[f64],
    extra: &[u8],
    persist_len: u64,
    limit: usize,
) -> Option<usize> {
    if ids.len() != values.len() {
        return None;
    }
    if P::state_schema_version() == 0 {
        state_stream_size(ids, values, extra, persist_len, limit)
    } else {
        let catalog = params.param_catalog();
        let entries = state_entries(&catalog, ids, values);
        state_stream_size_v2(&entries, extra, persist_len, limit)
    }
}

/// Stream a plugin's state envelope (v1 when `P::state_schema_version() == 0`,
/// v2 carrying schema version + stable parameter string IDs otherwise).
#[allow(clippy::too_many_arguments)]
pub fn write_plugin_state_stream<P: PluginExport>(
    plugin_id: u64,
    params: &P::Params,
    ids: &[u32],
    values: &[f64],
    extra: &[u8],
    persist_len: u64,
    limit: usize,
    persist: impl FnOnce(&mut dyn std::io::Write) -> std::io::Result<()>,
    write: impl FnMut(&[u8]) -> i64,
) -> bool {
    if ids.len() != values.len() {
        return false;
    }
    let schema_version = P::state_schema_version();
    if schema_version == 0 {
        write_state_stream_with_persist(
            plugin_id,
            ids,
            values,
            extra,
            persist_len,
            limit,
            persist,
            write,
        )
    } else {
        let catalog = params.param_catalog();
        let entries = state_entries(&catalog, ids, values);
        write_state_stream_v2_with_persist(
            plugin_id,
            schema_version,
            &entries,
            extra,
            persist_len,
            limit,
            persist,
            write,
        )
    }
}

/// Serialize a plugin instance into the canonical state envelope -
/// parameter values + optional custom-state payload, with the magic /
/// version / plugin-ID header `serialize_state` writes.
///
/// Same shape every format wrapper produces, so a `.state` file
/// written by one host loads in any other (subject to the
/// plugin-ID match `deserialize_state` enforces).
pub fn snapshot_plugin<P: PluginExport>(plugin: &P) -> Vec<u8> {
    if P::state_schema_version() > 0 {
        return snapshot_plugin_v2(plugin);
    }
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

/// Serialize a plugin instance into an OAST v2 envelope carrying its schema
/// version and stable parameter string IDs alongside fallback numeric IDs.
pub fn snapshot_plugin_v2<P: PluginExport>(plugin: &P) -> Vec<u8> {
    let catalog = plugin.params().param_catalog();
    let (ids, values) = plugin.params().collect_values();
    let entries = state_entries(&catalog, &ids, &values);
    let extra = read_custom_state_offthread(plugin);
    let persist = plugin.params().serialize_persist();
    serialize_state_v2(
        hash_plugin_id(P::info().clap_id),
        P::state_schema_version(),
        &entries,
        &extra,
        &persist,
    )
}

/// Inverse of [`snapshot_plugin`]. Validates the envelope's magic,
/// version, and plugin-ID hash; on success runs
/// [`crate::plugin::PluginRuntime::state_migrations`], resolves v2
/// string-keyed parameter IDs, restores parameter values via
/// `Params::restore_state`, and forwards the optional extra payload to
/// `Plugin::load_state`.
///
/// # Errors
///
/// Returns [`RestoreError::Invalid`] if the magic / version /
/// plugin-ID hash check fails or the envelope is truncated, and
/// [`RestoreError::LoadState`] when a schema migration or `#[persist]`
/// validation fails (see [`migrate_to_current`]). A successful return guarantees the params and (optional) extra
/// payload were forwarded to the plugin.
pub fn restore_plugin<P: PluginExport>(plugin: &mut P, bytes: &[u8]) -> Result<(), RestoreError> {
    let id = hash_plugin_id(P::info().clap_id);
    let mut s = deserialize_state(bytes, id).ok_or(RestoreError::Invalid)?;
    migrate_to_current::<P>(&mut s).map_err(RestoreError::LoadState)?;
    if !s.named_params.is_empty() {
        resolve_named_params(&mut s, &plugin.params().param_catalog());
    }
    plugin.params().restore_state(&s.params, &s.persist);
    if let Some(mut decoded_persist) =
        P::decode_persist_state(plugin.params(), &s.persist, StateContext::Unknown)
            .map_err(RestoreError::LoadState)?
    {
        plugin.apply_decoded_persist(&mut decoded_persist, StateContext::Unknown);
    }
    if let Some(extra) = s.extra {
        plugin.load_state(&extra).map_err(RestoreError::LoadState)?;
    }
    Ok(())
}

/// `(string_id, numeric_id, value)` entries for a v2 envelope. A parameter
/// without a string ID gets `""` and round-trips by numeric ID alone.
fn state_entries<'a>(
    catalog: &'a moose_params::ParamCatalog,
    ids: &[u32],
    values: &[f64],
) -> Vec<(&'a str, u32, f64)> {
    ids.iter()
        .zip(values)
        .map(|(&id, &value)| (catalog.string_id(id).unwrap_or(""), id, value))
        .collect()
}

// ---------------------------------------------------------------------------
// Presets: one schema-aware writer and one migration-aware reader shared by
// every wrapper (standalone, AU factory presets, editor preset menus).
// ---------------------------------------------------------------------------

/// Serialize `params`' current values plus the custom-state `extra` blob as a
/// preset envelope: OAST v1 when `P::state_schema_version() == 0` (so presets
/// from plugins that never opted into schemas stay readable by older builds),
/// OAST v2 with stable string IDs otherwise. Presets carry no `#[persist]`
/// block - that is session state (window size, paths), not part of a sound.
#[must_use]
pub fn serialize_preset<P: PluginExport>(params: &P::Params, extra: &[u8]) -> Vec<u8> {
    let catalog = params.param_catalog();
    let (ids, values) = params.collect_values();
    serialize_state_for_schema(
        hash_plugin_id(P::info().clap_id),
        P::state_schema_version(),
        &state_entries(&catalog, &ids, &values),
        extra,
        &[],
    )
}

/// Save `params`' current values plus `extra` to `store`'s user scope in the
/// envelope [`serialize_preset`] would write.
///
/// # Errors
/// As [`crate::presets::PresetStore::save`].
pub fn save_user_preset<P: PluginExport>(
    store: &crate::presets::PresetStore,
    meta: crate::presets::PresetMeta,
    params: &P::Params,
    extra: &[u8],
) -> Result<crate::presets::PresetRef, crate::presets::PresetError> {
    let catalog = params.param_catalog();
    let (ids, values) = params.collect_values();
    store.save_for_schema(
        meta,
        P::state_schema_version(),
        &state_entries(&catalog, &ids, &values),
        extra,
    )
}

/// Bring a decoded preset up to the current plugin: run
/// [`migrate_to_current`] (schema migrations + `#[persist]` validation),
/// then resolve v2 string-keyed entries against `params`' catalog so a
/// renumbered parameter still receives its saved value. The returned state
/// carries resolved numeric IDs only, ready for [`apply_params`] /
/// [`apply_state`].
///
/// # Errors
/// As [`migrate_to_current`].
pub fn prepare_preset<P: PluginExport>(
    params: &P::Params,
    mut state: DeserializedState,
) -> Result<DeserializedState, StateLoadError> {
    migrate_to_current::<P>(&mut state)?;
    if !state.named_params.is_empty() {
        resolve_named_params(&mut state, &params.param_catalog());
        state.named_params.clear();
    }
    Ok(state)
}

/// Read a preset file for this plugin and [`prepare_preset`] it. `None` for
/// an unreadable or foreign file, or one whose migration fails (logged).
#[must_use]
pub fn load_preset_file<P: PluginExport>(
    path: &std::path::Path,
    plugin_id_hash: u64,
    params: &P::Params,
) -> Option<DeserializedState> {
    let state = crate::presets::load_preset_file(path, plugin_id_hash)?;
    prepare_preset::<P>(params, state)
        .inspect_err(|err| eprintln!("moose: preset {}: {err}", path.display()))
        .ok()
}

/// Load a preset from `store` by uri / uuid / name and [`prepare_preset`] it.
///
/// # Errors
/// As [`crate::presets::PresetStore::load`]; a failed migration is
/// [`crate::presets::PresetError::InvalidState`].
pub fn load_preset<P: PluginExport>(
    store: &crate::presets::PresetStore,
    params: &P::Params,
    uri_or_uuid: &str,
) -> Result<DeserializedState, crate::presets::PresetError> {
    let state = store.load(uri_or_uuid)?;
    prepare_preset::<P>(params, state).map_err(|err| {
        eprintln!("moose: preset {uri_or_uuid}: {err}");
        crate::presets::PresetError::InvalidState
    })
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
            ..MigratedState::default()
        };
        let restored: DeserializedState = migrated.into();
        assert_eq!(restored.persist, vec![1, 2, 3]);
        assert_eq!(restored.extra.as_deref(), Some(&[9, 9][..]));
        assert_eq!(restored.params, vec![(0, 1.0)]);
    }

    /// A renamed schema-v2 plugin sees its old envelope's string-keyed
    /// entries and schema, and what it returns rides into the restore as
    /// named entries at that schema (PR #76 review).
    #[test]
    fn renamed_v2_envelope_offers_and_returns_named_params() {
        use super::{
            ForeignState, NamedParam, PluginFormat, parse_state, route_parsed_state,
            serialize_state_v2,
        };
        let blob = serialize_state_v2(
            0xA11CE,
            3,
            &[("osc1.cutoff", 10, 0.75), ("mix", 11, 0.5)],
            b"extra",
            b"persist",
        );
        let parsed = parse_state(&blob, 0xB0B);
        let mut offered = false;
        let state = route_parsed_state(parsed, &blob, PluginFormat::Clap, None, |foreign| {
            let ForeignState::MismatchedEnvelope {
                plugin_id_hash,
                params,
                named_params,
                schema_version,
                extra,
                persist,
            } = foreign
            else {
                panic!("a renamed envelope must arrive as MismatchedEnvelope");
            };
            offered = true;
            assert_eq!(*plugin_id_hash, 0xA11CE);
            assert!(params.is_empty(), "v2 entries are all named");
            assert_eq!(*schema_version, 3);
            assert_eq!(*extra, Some(&b"extra"[..]));
            assert_eq!(*persist, b"persist");
            assert_eq!(named_params.len(), 2);
            assert_eq!(named_params[0].key, "osc1.cutoff");
            assert_eq!(named_params[0].fallback_id, Some(10));
            // Keep every entry and add one under a renamed key.
            let mut kept = named_params.to_vec();
            kept.push(NamedParam::new("filter.drive", 0.25));
            Some(MigratedState {
                named_params: kept,
                schema_version: *schema_version,
                persist: persist.to_vec(),
                ..MigratedState::default()
            })
        })
        .expect("the plugin accepted the envelope");
        assert!(offered);
        assert_eq!(state.schema_version, 3);
        assert_eq!(state.persist, b"persist");
        assert_eq!(state.params, []);
        let keys: Vec<&str> = state.named_params.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(keys, ["osc1.cutoff", "mix", "filter.drive"]);
    }

    #[test]
    fn migrated_state_default_has_empty_persist() {
        let restored: DeserializedState = MigratedState::default().into();
        assert_eq!(restored.persist, []);
        assert!(restored.extra.is_none());
        assert_eq!(restored.params, []);
        assert_eq!(restored.named_params, []);
        assert_eq!(restored.schema_version, 0);
    }
}

/// Default minimum window during which a recent audio-block heartbeat counts
/// the audio thread as actively streaming. Expanded automatically by
/// [`AudioHeartbeat::set_block_interval`] when a host negotiates a larger block
/// period (e.g. 8,192 frames at 44.1 kHz ≈ 186 ms).
pub(crate) const AUDIO_ACTIVE_WINDOW: std::time::Duration = std::time::Duration::from_millis(150);

/// Maximum heartbeat activity window cap so pathological prepare parameters
/// cannot make a stalled host look active indefinitely.
const AUDIO_ACTIVE_WINDOW_MAX_US: u64 = 2_000_000;

/// Extra margin above one negotiated block interval to absorb audio-thread
/// scheduling jitter and the host handshake wait.
const AUDIO_ACTIVE_INTERVAL_MARGIN_US: u64 = 50_000;

/// Host-side poll interval while waiting for an audio-thread atomic completion.
/// Keeps the audio thread 100% free of mutexes and `Thread::unpark` syscalls
/// while waking the host well within a millisecond of block completion.
pub(crate) const WAIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_micros(250);

static MONOTONIC_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Initialize the process-wide monotonic epoch off the audio thread so audio
/// heartbeats never trigger `OnceLock` initialization inside `process`.
pub(crate) fn warm_monotonic_epoch() {
    let _ = MONOTONIC_EPOCH.get_or_init(std::time::Instant::now);
}

#[cfg(test)]
thread_local! { static TEST_TIME: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
pub(crate) fn with_test_time(us: u64, run: impl FnOnce()) {
    let old = TEST_TIME.replace(Some(us));
    run();
    TEST_TIME.set(old);
}

#[inline]
fn monotonic_us() -> u64 {
    #[cfg(test)]
    if let Some(us) = TEST_TIME.get() {
        return us;
    }
    let Some(epoch) = MONOTONIC_EPOCH.get() else {
        return 0;
    };
    let micros = epoch.elapsed().as_micros();
    #[allow(clippy::cast_possible_truncation)]
    let clamped = micros.min(u128::from(u64::MAX)) as u64;
    // Reserve 0 for "never ran / marked inactive".
    clamped.max(1)
}

/// Lock-free, allocation-free audio-thread activity tracker shared by
/// [`StateLoadQueue`] and [`crate::snapshot::SnapshotSlot`].
pub(crate) struct AudioHeartbeat {
    /// Microseconds since `MONOTONIC_EPOCH` of the last audio block, or `0`
    /// when no block has run since construction or `mark_inactive`.
    last_block_us: std::sync::atomic::AtomicU64,
    /// Per-thread token of the last audio block caller (0 = none).
    last_thread: std::sync::atomic::AtomicU64,
    /// Activity window in microseconds, adjusted for the negotiated block period.
    active_window_us: std::sync::atomic::AtomicU64,
    /// Set by `mark_inactive()` and cleared by `record_block()`.
    stopped: std::sync::atomic::AtomicBool,
    /// Monotonic stop-transition counter incremented by `mark_inactive()`.
    inactive_gen: std::sync::atomic::AtomicU64,
}

impl AudioHeartbeat {
    pub(crate) fn new() -> Self {
        warm_monotonic_epoch();
        #[allow(clippy::cast_possible_truncation)]
        let default_window_us = AUDIO_ACTIVE_WINDOW.as_micros() as u64;
        Self {
            last_block_us: std::sync::atomic::AtomicU64::new(0),
            last_thread: std::sync::atomic::AtomicU64::new(0),
            active_window_us: std::sync::atomic::AtomicU64::new(default_window_us),
            stopped: std::sync::atomic::AtomicBool::new(false),
            inactive_gen: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Host thread (on prepare/activate): expand the activity window to cover
    /// the host's negotiated block interval plus scheduling margin.
    pub(crate) fn set_block_interval(&self, sample_rate: f64, max_block_size: usize) {
        use std::sync::atomic::Ordering;
        #[allow(clippy::cast_possible_truncation)]
        let default_window_us = AUDIO_ACTIVE_WINDOW.as_micros() as u64;
        let window_us = if sample_rate.is_finite() && sample_rate > 0.0 && max_block_size > 0 {
            #[allow(clippy::cast_precision_loss)]
            let block_us_f = (max_block_size as f64 / sample_rate) * 1_000_000.0;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let block_us = if block_us_f.is_finite() && block_us_f > 0.0 {
                block_us_f.ceil().min(2_000_000.0) as u64
            } else {
                0
            };
            block_us
                .saturating_add(AUDIO_ACTIVE_INTERVAL_MARGIN_US)
                .clamp(default_window_us, AUDIO_ACTIVE_WINDOW_MAX_US)
        } else {
            default_window_us
        };
        self.active_window_us.store(window_us, Ordering::Release);
    }

    /// Audio thread: stamp a live block. Wait-free, zero allocations, zero syscalls.
    #[inline]
    pub(crate) fn record_block(&self) {
        use std::sync::atomic::Ordering;
        self.stopped.store(false, Ordering::Relaxed);
        self.last_thread
            .store(crate::wrapper::current_thread_token(), Ordering::Relaxed);
        self.last_block_us.store(monotonic_us(), Ordering::Release);
    }

    /// Host thread (on stop/deactivate): clear activity immediately and advance
    /// the stop counter so any in-flight wait loop exits without stalling.
    #[inline]
    pub(crate) fn mark_inactive(&self) {
        use std::sync::atomic::Ordering;
        self.last_block_us.store(0, Ordering::Release);
        self.last_thread.store(0, Ordering::Relaxed);
        self.stopped.store(true, Ordering::Release);
        self.inactive_gen.fetch_add(1, Ordering::Release);
    }

    #[inline]
    pub(crate) fn inactive_gen(&self) -> u64 {
        self.inactive_gen.load(std::sync::atomic::Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn was_stopped_since(&self, start_gen: u64) -> bool {
        use std::sync::atomic::Ordering;
        self.stopped.load(Ordering::Acquire)
            || self.inactive_gen.load(Ordering::Acquire) != start_gen
    }

    /// Host thread: bounded wait timeout scaled to the negotiated audio block
    /// interval (at least `floor`) so large blocks (e.g. 8,192 frames at
    /// 44.1 kHz) do not time out before one block completes.
    #[must_use]
    pub(crate) fn handshake_timeout(&self, floor: std::time::Duration) -> std::time::Duration {
        let window_us = self
            .active_window_us
            .load(std::sync::atomic::Ordering::Acquire);
        std::time::Duration::from_micros(window_us).max(floor)
    }

    /// Host thread: true only when another thread ran an audio block within
    /// the active window (at least [`AUDIO_ACTIVE_WINDOW`], expanded for the
    /// negotiated block interval). Returns `false` for stopped, sleeping,
    /// bypassed, suspended, or single-threaded callers.
    #[must_use]
    pub(crate) fn is_active_on_other_thread(&self) -> bool {
        use std::sync::atomic::Ordering;
        let last_us = self.last_block_us.load(Ordering::Acquire);
        if last_us == 0 {
            return false;
        }
        let last_tid = self.last_thread.load(Ordering::Relaxed);
        if last_tid == 0 || last_tid == crate::wrapper::current_thread_token() {
            return false;
        }
        let now_us = monotonic_us();
        let window_us = self.active_window_us.load(Ordering::Acquire);
        now_us.saturating_sub(last_us) <= window_us
    }
}

/// Capacity of the lock-free retirement queue for swapped-out [`DecodedState`]
/// values. Because `ready` has capacity 1 and every host push/wait drains
/// `retired`, at most 1–2 entries sit here in practice; 8 provides ample slack
/// so the audio thread never deallocates.
const RETIRED_STATE_CAP: usize = 8;

pub(crate) struct QueuedLoad {
    pub(crate) state: std::sync::Arc<DeserializedState>,
    pub(crate) backup: Option<std::sync::Arc<ParamsBackup>>,
    pub(crate) decoded: Option<DecodedState>,
    pub(crate) decoded_persist: Option<DecodedState>,
    pub(crate) context: StateContext,
}

type RetainedOwner = (
    std::sync::Arc<DeserializedState>,
    Option<std::sync::Arc<ParamsBackup>>,
);

/// Host-produced, single audio-consumer state handoff. The host retains an Arc
/// until it observes that neither the ready slot nor audio owns it. Audio only
/// decrements an Arc; it can never perform the final free of a recalled state.
/// Swapped-out [`DecodedState`] values return through the lock-free `retired`
/// queue and are dropped on the host thread, so audio never deallocates them
/// either. At most one consumed state and one queued state survive a producer
/// call. Persist lanes have already been applied on the host and are released
/// there.
pub struct StateLoadQueue {
    ready: crossbeam_queue::ArrayQueue<QueuedLoad>,
    retired: crossbeam_queue::ArrayQueue<DecodedState>,
    owners: std::sync::Mutex<Vec<RetainedOwner>>,
    /// Address of the last state whose apply finished (0 = none), so the
    /// host can tell whether its own push has landed. The host keeps an
    /// `Arc` clone while it compares, so the address can't be reused.
    applied: std::sync::atomic::AtomicUsize,
    /// Last accepted queued custom load. Its host-owned Arc supplies save bytes
    /// until a current snapshot becomes available. Never lock from apply.
    accepted_custom: std::sync::atomic::AtomicUsize,
    /// Whether that apply's `load_state` succeeded.
    applied_ok: std::sync::atomic::AtomicBool,
    /// Audio-thread block heartbeat for sleeping-host detection.
    heartbeat: AudioHeartbeat,
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
            retired: crossbeam_queue::ArrayQueue::new(RETIRED_STATE_CAP),
            owners: std::sync::Mutex::new(Vec::with_capacity(3)),
            applied: std::sync::atomic::AtomicUsize::new(0),
            accepted_custom: std::sync::atomic::AtomicUsize::new(0),
            applied_ok: std::sync::atomic::AtomicBool::new(true),
            heartbeat: AudioHeartbeat::new(),
        }
    }
    /// Host thread (on prepare/activate): configure the negotiated block
    /// interval for sleeping-host heartbeat detection.
    pub fn set_block_interval(&self, sample_rate: f64, max_block_size: usize) {
        self.heartbeat
            .set_block_interval(sample_rate, max_block_size);
    }
    /// Audio thread: record that an audio block is running. Wait-free, zero allocs.
    #[inline]
    pub fn record_audio_block(&self) {
        self.heartbeat.record_block();
    }
    /// Host thread: mark audio inactive on stop/deactivate so any waiting host
    /// returns promptly without stalling for the handshake timeout.
    #[inline]
    pub fn mark_audio_inactive(&self) {
        self.heartbeat.mark_inactive();
    }
    /// Host thread: true when another thread recently ran an audio block.
    #[must_use]
    pub fn is_audio_active_on_other_thread(&self) -> bool {
        self.heartbeat.is_active_on_other_thread()
    }
    /// Host thread only. All persist fields must already have been applied.
    pub fn force_push(&self, state: DeserializedState) {
        drop(self.push_tracked(state));
    }
    /// [`Self::force_push`], returning a handle for [`Self::wait_applied`].
    pub fn push_tracked(&self, state: DeserializedState) -> std::sync::Arc<DeserializedState> {
        self.push_with_backup_and_decoded(state, None, None)
    }
    pub(crate) fn push_with_backup_and_decoded(
        &self,
        state: DeserializedState,
        backup: Option<ParamsBackup>,
        decoded: Option<DecodedState>,
    ) -> std::sync::Arc<DeserializedState> {
        self.push_with_backup_decoded_and_context(state, backup, decoded, StateContext::Unknown)
    }
    pub(crate) fn push_with_backup_decoded_and_context(
        &self,
        state: DeserializedState,
        backup: Option<ParamsBackup>,
        decoded: Option<DecodedState>,
        context: StateContext,
    ) -> std::sync::Arc<DeserializedState> {
        self.push_with_backup_decoded_persist_and_context(state, backup, decoded, None, context)
    }
    pub(crate) fn push_with_backup_decoded_persist_and_context(
        &self,
        mut state: DeserializedState,
        backup: Option<ParamsBackup>,
        decoded: Option<DecodedState>,
        decoded_persist: Option<DecodedState>,
        context: StateContext,
    ) -> std::sync::Arc<DeserializedState> {
        self.reclaim_retired();
        let backup = backup.map(std::sync::Arc::new);
        state.persist = Vec::new();
        let state = std::sync::Arc::new(state);
        let mut owners = self
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        owners.push((std::sync::Arc::clone(&state), backup.clone()));
        drop(self.ready.force_push(QueuedLoad {
            state: std::sync::Arc::clone(&state),
            backup,
            decoded,
            decoded_persist,
            context,
        }));
        // `> 2`: the ready slot or audio, plus the handle returned below.
        owners.retain(|(s, _)| {
            std::sync::Arc::strong_count(s) > 1 + usize::from(std::sync::Arc::ptr_eq(s, &state))
                || self
                    .accepted_custom
                    .load(std::sync::atomic::Ordering::Acquire)
                    == std::sync::Arc::as_ptr(s) as usize
        });
        debug_assert!(owners.len() <= 3, "single audio consumer contract");
        state
    }
    /// Audio thread: park a swapped-out [`DecodedState`] for off-thread drop.
    /// Wait-free, zero allocations, zero deallocations while capacity holds.
    pub(crate) fn retire_decoded(&self, old: DecodedState) {
        let _ = self.retired.push(old);
    }
    /// Host thread: drop any retired [`DecodedState`] values swapped out by the
    /// audio thread.
    pub fn reclaim_retired(&self) {
        while let Some(old) = self.retired.pop() {
            drop(old);
        }
    }
    pub fn pop(&self) -> Option<std::sync::Arc<DeserializedState>> {
        self.pop_queued().map(|q| q.state)
    }
    pub(crate) fn pop_queued(&self) -> Option<QueuedLoad> {
        self.ready.pop()
    }
    pub(crate) fn set_accepted_custom(&self, state_ptr: usize) {
        self.accepted_custom
            .store(state_ptr, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn accepted_extra(&self) -> Option<Vec<u8>> {
        let owners = self
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = self
            .accepted_custom
            .load(std::sync::atomic::Ordering::Acquire);
        owners
            .iter()
            .find(|(s, _)| std::sync::Arc::as_ptr(s) as usize == current)
            .and_then(|(s, _)| s.extra.clone())
    }
    /// Record that `state` (from [`Self::pop`]) has been applied.
    /// Strictly lock-free and syscall-free: safe inside `RtSection` on the
    /// audio thread (publishes completion with atomics only).
    #[inline]
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
    /// block ran in time (or audio stopped) and the state stays queued.
    #[must_use]
    pub fn wait_applied(
        &self,
        state: &std::sync::Arc<DeserializedState>,
        timeout: std::time::Duration,
    ) -> Option<bool> {
        self.wait_applied_poll(state, timeout, || {})
    }

    pub(crate) fn wait_applied_poll(
        &self,
        state: &std::sync::Arc<DeserializedState>,
        timeout: std::time::Duration,
        mut on_poll: impl FnMut(),
    ) -> Option<bool> {
        use std::sync::atomic::Ordering;
        let start_gen = self.heartbeat.inactive_gen();
        if self.is_applied(state) {
            return Some(self.applied_ok.load(Ordering::Relaxed));
        }
        if timeout.is_zero() || self.heartbeat.was_stopped_since(start_gen) {
            return None;
        }
        let deadline = std::time::Instant::now() + timeout;
        let mut spins = 0u8;
        loop {
            on_poll();
            if self.is_applied(state) {
                return Some(self.applied_ok.load(Ordering::Relaxed));
            }
            if self.heartbeat.was_stopped_since(start_gen) {
                return None;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            if spins < 4 {
                spins += 1;
                std::thread::yield_now();
            } else {
                std::thread::sleep((deadline - now).min(WAIT_POLL_INTERVAL));
            }
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
            .map(|(state, _)| state)
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
            named_params: vec![],
            schema_version: 0,
            extra: Some(vec![value; 1024]),
            persist: vec![value; 1024],
        }
    }

    #[test]
    fn migration_chain_upgrades_step_by_step_and_rejects_future_or_gapped_versions() {
        #[allow(clippy::unnecessary_wraps)]
        fn v0_to_v1(s: &mut DeserializedState) -> Result<(), StateLoadError> {
            s.named_params.push(NamedParam::new("osc1.cutoff", 500.0));
            Ok(())
        }
        #[allow(clippy::unnecessary_wraps)]
        fn v1_to_v2(s: &mut DeserializedState) -> Result<(), StateLoadError> {
            for entry in &mut s.named_params {
                if entry.key == "osc1.cutoff" {
                    entry.value *= 2.0;
                }
            }
            Ok(())
        }
        let chain = StateMigrationChain::new(2)
            .step(0, v0_to_v1)
            .step(1, v1_to_v2);

        let mut old = DeserializedState::default();
        chain.run(&mut old).expect("0 -> 1 -> 2 succeeds");
        assert_eq!(old.schema_version, 2);
        assert_eq!(
            old.named_params,
            vec![NamedParam::new("osc1.cutoff", 1000.0)]
        );

        let mut future = DeserializedState {
            schema_version: 3,
            ..DeserializedState::default()
        };
        assert!(chain.run(&mut future).is_err());

        let gapped = StateMigrationChain::new(2).step(0, v0_to_v1);
        let mut s = DeserializedState::default();
        assert!(gapped.run(&mut s).is_err());
    }

    #[test]
    fn resolve_named_params_remaps_by_string_id_and_never_leaks_fallback_into_reused_id() {
        use moose_params::{
            ParamCatalog, ParamFlags, ParamInfo, ParamRange, ParamUnit, ParamValueKind,
        };
        let mk = |id: u32, name: &'static str| ParamInfo {
            id,
            name,
            short_name: name,
            group: "",
            range: ParamRange::Linear { min: 0.0, max: 1.0 },
            default_plain: 0.0,
            flags: ParamFlags::AUTOMATABLE,
            unit: ParamUnit::None,
            kind: ParamValueKind::Float,
            midi_map: None,
            midi_channel: None,
        };
        // Current catalog: `osc1.cutoff` moved from numeric ID 10 -> 100, and
        // `mix` now reuses numeric ID 10.
        let catalog = ParamCatalog::with_string_ids(
            vec![mk(100, "Cutoff"), mk(10, "Mix")],
            vec![(100, "osc1.cutoff".into()), (10, "mix".into())],
        );
        // Saved record for `osc1.cutoff` carried old numeric ID 10 as fallback.
        // Resolving `osc1.cutoff` -> 100 must NOT also restore 0.75 onto `mix` (ID 10).
        let mut s = DeserializedState {
            params: Vec::new(),
            named_params: vec![NamedParam {
                key: "osc1.cutoff".into(),
                fallback_id: Some(10),
                value: 0.75,
            }],
            schema_version: 1,
            extra: None,
            persist: Vec::new(),
        };
        resolve_named_params(&mut s, &catalog);
        assert_eq!(s.params, vec![(100, 0.75)]);
        assert_eq!(s.named_params, Vec::new());

        // When a string key does NOT resolve, its numeric fallback still applies.
        let mut fallback = DeserializedState {
            params: Vec::new(),
            named_params: vec![NamedParam {
                key: "removed.key".into(),
                fallback_id: Some(10),
                value: 0.4,
            }],
            schema_version: 1,
            extra: None,
            persist: Vec::new(),
        };
        resolve_named_params(&mut fallback, &catalog);
        assert_eq!(fallback.params, vec![(10, 0.4)]);
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

    /// The host pushes while an audio thread pops and applies, as in a live
    /// session. Under Miri this checks the handoff for data races and that
    /// every recalled state is freed (leak check) once both sides finish.
    #[test]
    fn concurrent_push_and_apply_frees_every_state() {
        let queue = std::sync::Arc::new(StateLoadQueue::new(1));
        let audio = {
            let queue = std::sync::Arc::clone(&queue);
            std::thread::spawn(move || {
                let mut applied = 0;
                while applied < 3 {
                    if let Some(state) = queue.pop() {
                        assert!(state.persist.is_empty(), "persist stays on the host");
                        queue.mark_applied(&state, true);
                        applied = state.extra.as_ref().map_or(0, |e| e[0]);
                    }
                    std::thread::yield_now();
                }
            })
        };
        for value in 1..3 {
            queue.force_push(state(value));
        }
        let last = queue.push_tracked(state(3));
        audio.join().unwrap();
        assert_eq!(
            queue.wait_applied(&last, std::time::Duration::ZERO),
            Some(true)
        );
        assert!(queue.unapplied().is_none());
    }
}
