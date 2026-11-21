#![forbid(unsafe_code)]

mod catalog;
mod dynamic;
mod info;
mod param_text;
mod persist_write;
mod projection;
mod range;
pub mod sample;
mod smooth;
mod types;

pub use catalog::{CatalogChange, CatalogChanges, CatalogError, CatalogSignal, ParamCatalog};
pub use dynamic::{DynamicParams, Slot, SlotDef, SlotEdit, SlotRange};
pub use info::{
    MidiSource, ParamFlags, ParamFunction, ParamInfo, ParamUnit, ParamValueKind,
    map_source_to_param,
};
pub use param_text::parse_formatted_value;
pub use persist_write::{KeyedPersistWrite, PreparedPersistWrite, write_prepared_persist};
pub use projection::{ParamProjection, ProjectedValue, append_projected_infos};
pub use range::ParamRange;
pub use sample::{Float, Sample};
pub use smooth::{SmoothState, Smoother, SmoothingStyle};
pub use types::{
    BoolParam, EnumParam, FloatParam, FloatParamReadF32, FloatParamReadF64, IntParam, MeterSlot,
    ParamEnum, ParamSmoother, ValueListParam,
};
#[expect(deprecated, reason = "re-exported until the 8.0 removal")]
pub use types::{bump_edit_epoch, custom_edit_epoch, edit_epoch};

/// Deterministically hash a stable parameter string ID (e.g. `"osc1.cutoff"`)
/// into the host-safe 31-bit parameter ID domain (`0..=PARAM_ID_MAX`).
///
/// Uses FNV-1a-32 masked to 31 bits so every plugin format (CLAP, VST3, AU)
/// accepts the resulting ID.
///
/// Moose never uses this to assign host IDs: numeric IDs stay as declared
/// and string IDs ride beside them in [`ParamCatalog`] and the OAST v2 state
/// envelope (D9). Re-deriving a shipped plugin's host IDs from string IDs
/// would orphan every saved host automation lane. Use it only for a new
/// parameter set that has never shipped, and build its catalog with
/// [`ParamCatalog::try_new`] so a collision is an error, not a panic.
#[must_use]
pub const fn hash_param_id(string_id: &str) -> u32 {
    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let bytes = string_id.as_bytes();
    let mut h = FNV_OFFSET;
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u32;
        h = h.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    h & PARAM_ID_MAX
}

/// Largest host-visible parameter ID. The signed-positive 31-bit
/// domain is accepted by every supported format, including VST3.
#[doc(hidden)]
pub const PARAM_ID_MAX: u32 = 0x7fff_ffff;

/// Historical mask for auto-assigned and nested parameter IDs.
/// Keep this at 24 bits: changing it would remap existing automation
/// and serialized state even though explicit IDs use the wider domain.
#[doc(hidden)]
pub const AUTO_PARAM_ID_MASK: u32 = (1 << 24) - 1;

/// Preserve the historical nested-ID fold for the old 24-bit domain,
/// while treating newly accepted wider explicit IDs as absolute.
#[doc(hidden)]
#[must_use]
pub const fn rebase_nested_param_id(id: u32, base: u32) -> u32 {
    if id <= AUTO_PARAM_ID_MASK {
        id.wrapping_add(base) & AUTO_PARAM_ID_MASK
    } else {
        id
    }
}

/// Implementation detail - not part of the stable public API.
/// Internal meter IDs start immediately outside the host parameter
/// domain and are used only to index meter storage.
#[doc(hidden)]
pub const METER_ID_BASE: u32 = PARAM_ID_MAX + 1;

/// Sealing module: external crates cannot implement [`Params`] or
/// [`ParamEnum`] directly because they can't name `Sealed`. The
/// `#[derive(Params)]` and `#[derive(ParamEnum)]` macros emit the
/// `Sealed` impl alongside their trait impls, so derive users are
/// unaffected.
#[doc(hidden)]
pub mod __private {
    pub trait Sealed {}
}

/// Format a plain parameter value as a display string based on the parameter's unit.
///
/// Used by the `#[derive(Params)]` macro for default `format_value` implementations
/// on `FloatParam` and `IntParam` fields. `IntParam` is identified by
/// `ParamValueKind::Int`, set by the derive from the field type - its
/// value is always integer-valued, so the fractional `{:.1}` / `{:.2}`
/// formats float-typed params use would render "0.0 st" / "0.00"
/// instead of "0 st" / "0".
#[must_use]
pub fn format_param_value(info: &ParamInfo, value: f64) -> String {
    let is_int = info.kind == ParamValueKind::Int;
    // Round to nearest integer before display so a smoothed IntParam
    // that's mid-transition doesn't briefly render the rounded-down
    // half-step (e.g. an `i32::from(value)` of -1 when value is -0.5
    // mid-snap). `IntParam::value_i32` rounds the same way at the
    // audio-thread read site.
    #[allow(clippy::cast_possible_truncation)]
    let int_value = value.round() as i64;
    match info.unit {
        ParamUnit::Db => {
            if is_int {
                format!("{int_value} dB")
            } else {
                format!("{value:.1} dB")
            }
        }
        ParamUnit::Hz => {
            if value >= 1000.0 {
                format!("{:.1} kHz", value / 1000.0)
            } else {
                format!("{value:.0} Hz")
            }
        }
        ParamUnit::Milliseconds => {
            if is_int {
                format!("{int_value} ms")
            } else {
                format!("{value:.1} ms")
            }
        }
        ParamUnit::Seconds => {
            if value >= 1.0 {
                format!("{value:.2} s")
            } else {
                format!("{:.0} ms", value * 1000.0)
            }
        }
        ParamUnit::Percent => format!("{:.0}%", value * 100.0),
        ParamUnit::Semitones => {
            if is_int {
                format!("{int_value} st")
            } else {
                format!("{value:.1} st")
            }
        }
        ParamUnit::Degrees => {
            if is_int {
                format!("{int_value}°")
            } else {
                format!("{value:.1}°")
            }
        }
        ParamUnit::Pan => {
            // Convention: pan params are normalized to [-1.0, 1.0]. Round
            // to nearest integer percent first so the dead-zone test and
            // L/R label agree (e.g. -0.004 → 0% → "C", -0.006 → -1% → "1L").
            // Result is bounded by `[-100, 100]` after clamp to `[-1, 1]`.
            #[allow(clippy::cast_possible_truncation)]
            let pct = (value * 100.0).round() as i32;
            match pct.cmp(&0) {
                std::cmp::Ordering::Equal => "C".to_string(),
                std::cmp::Ordering::Less => format!("{}L", -pct),
                std::cmp::Ordering::Greater => format!("{pct}R"),
            }
        }
        ParamUnit::Custom(unit) => {
            if is_int {
                format!("{int_value}{unit}")
            } else {
                format!("{value:.2}{unit}")
            }
        }
        ParamUnit::None => {
            if is_int {
                format!("{int_value}")
            } else {
                format!("{value:.2}")
            }
        }
    }
}

/// Host-facing label and visibility for a parameter whose ID, range and
/// backing storage stay fixed. Dynamic-list formats may omit unavailable
/// parameters. Returned by [`Params::parameter_presentation`]; format
/// wrappers rescan when [`Params::parameter_presentation_revision`] changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterPresentation {
    pub name: String,
    pub group: String,
    pub hidden: bool,
    /// Whether formats with a dynamic parameter list should enumerate it.
    /// The parameter ID and storage remain fixed for other formats.
    pub available: bool,
}

impl Default for ParameterPresentation {
    fn default() -> Self {
        Self {
            name: String::new(),
            group: String::new(),
            hidden: false,
            available: true,
        }
    }
}

/// Host-thread commit of a fully read and validated persistence document.
/// Preparation must not mutate plugin state. Large fields may be decoded into
/// their final immutable representation instead of a serialized staging buffer.
pub type PreparedPersist<P> = Box<dyn FnOnce(&P, &[(u32, f64)], &[u8]) + Send>;

/// A non-realtime completion wake. The wrapper closes this gate before
/// releasing host handles; workers retain only Weak handles to it. The wake
/// must schedule a host-thread callback, never synchronously run the drain.
pub struct PersistChangeSignal {
    wake: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}
impl PersistChangeSignal {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            wake: std::sync::Mutex::new(Some(Box::new(wake))),
        }
    }
    /// Background workers only. Not audio safe (may lock and wake the host).
    pub fn notify(&self) {
        let wake = self
            .wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(wake) = wake.as_ref() {
            wake();
        }
    }
    /// Waits for an in-flight request, then permanently disables host access.
    pub fn close(&self) {
        self.wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// A compact identity prepared by the owner of a persisted field.
///
/// `byte_len` describes the canonical input represented by `digest`, not a host
/// stream size limit. Producers must include every persisted value, preserve
/// float bits, and keep the identity stable for unchanged content. Immutable
/// payload owners can cache a 256-bit digest when preparing their data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PersistFingerprint {
    pub byte_len: u64,
    pub digest: [u8; 32],
}

/// Visits a derived persistence tree without constructing its complete blob.
///
/// Each field is bracketed by `begin_field`/`end_field`; nested parameter stores
/// recurse inside their own bracket. A leaf supplies either its cached identity
/// or its ordinary serialized bytes. Consumers own their hashing policy and
/// must frame names and lengths to distinguish nested/adjacent fields.
pub trait PersistFingerprintVisitor {
    fn begin_field(&mut self, key: &str);
    fn end_field(&mut self);
    fn fingerprint(&mut self, value: PersistFingerprint);
    fn serialized(&mut self, bytes: &[u8]);
}

/// Trait implemented by #[derive(Params)] on a struct.
/// Format wrappers use this to enumerate, read, and write parameters.
///
/// Stays dyn-compatible (every method dispatches through `&self`) so
/// editors can pass `Arc<dyn Params>` into the screenshot pipeline
/// without naming the concrete type. Generic code that needs to
/// *construct* a fresh `Params` value should add a `Default` bound
/// rather than expecting one on the trait - `#[derive(Params)]` emits
/// `impl Default` alongside the trait impl, so that bound is free for
/// derive users.
pub trait Params: __private::Sealed + Send + Sync + 'static {
    /// Optional incremental persistence preparation. Return `None` without
    /// reading to keep the ordinary buffered path. `Some(Err)` rejects before
    /// any parameter changes; `Some(Ok)` must consume exactly `len` bytes and
    /// return small validation bytes plus a host-thread commit. The wrapper
    /// validates framing, EOF and `validate_persist` before invoking the commit.
    fn prepare_persist_stream(
        _read: &mut dyn std::io::Read,
        _len: usize,
    ) -> Option<std::io::Result<(Vec<u8>, PreparedPersist<Self>)>>
    where
        Self: Sized,
    {
        None
    }

    /// Runtime name / group / visibility / availability for `id`, read on the
    /// host main thread. `None` keeps the static [`ParamInfo`] label.
    /// Derive: `#[params(presentation = "method")]`.
    fn parameter_presentation(&self, _id: u32) -> Option<ParameterPresentation> {
        None
    }

    /// Bump whenever any [`Self::parameter_presentation`] result changes;
    /// wrappers read it on the host main thread and ask the host to rescan.
    /// It may refresh derived metadata; it is never an audio-thread poll.
    /// Derive: `#[params(presentation_revision = "method")]`.
    fn parameter_presentation_revision(&self) -> u64 {
        0
    }

    /// The host-known role of `id`, if any (VST3 `IParameterFunctionName`).
    /// Main thread. Derive: `#[params(function = "method")]`, else the first
    /// `#[nested]` group that answers.
    fn parameter_function(&self, _id: u32) -> Option<ParamFunction> {
        None
    }

    /// Bound once by the format wrapper. A runtime parameter set (such as
    /// [`DynamicParams`]) wakes it after every committed change.
    fn set_catalog_signal(&self, _signal: std::sync::Weak<CatalogSignal>) {}

    /// Main thread, after a catalog wake: what changed since the last call.
    /// Taking clears it.
    fn take_catalog_changes(&self) -> CatalogChanges {
        CatalogChanges::default()
    }

    /// Whether a write to this ID can change another parameter's name, group
    /// or visibility. Called only for changed-event IDs, including on audio
    /// threads: do not allocate, lock, or rebuild presentation here. Wrappers
    /// coalesce a main-thread callback; the revision getter may then refresh
    /// derived metadata off audio. Ordinary numeric IDs should return false.
    /// Derive: `#[params(presentation_change = "method")]`.
    fn parameter_affects_presentation(&self, _id: u32) -> bool {
        false
    }

    /// Stored parameter changes in this instance, including nested groups.
    /// Equal reads mean no setter changed a value. The counter is per
    /// instance; custom state keeps its own generation. The default (a
    /// hand-written impl with no counter) never changes.
    fn parameter_edit_epoch(&self) -> u64 {
        0
    }

    /// Attach the counter of a containing parameter tree during construction.
    #[doc(hidden)]
    fn set_edit_epoch(&mut self, _epoch: std::sync::Arc<std::sync::atomic::AtomicU64>) {}

    /// All parameter infos, in declaration order.
    fn param_infos(&self) -> Vec<ParamInfo>;

    /// Stable string IDs for parameters in this store (`(numeric_id, "path")`),
    /// used by OAST v2 state serialization and [`ParamCatalog::by_string_id`].
    /// `#[derive(Params)]` populates this automatically from field names and
    /// nested paths (e.g. `"osc1.cutoff"`).
    fn param_string_ids(&self) -> Vec<(u32, String)> {
        Vec::new()
    }

    /// Labelled choices for discrete list parameters (`ValueListParam`,
    /// `EnumParam`, `DynamicParams` `SlotRange::Choice`), used by host value
    /// string queries and UI dropdowns.
    fn value_list_labels(&self, _id: u32) -> Option<Vec<String>> {
        None
    }

    /// Live [`ParamRange`] for `id` when its bounds can change at runtime
    /// (such as [`ValueListParam`] after [`ValueListParam::set_labels`] or
    /// [`DynamicParams`] slots). Must be lock-free so format wrappers can call
    /// it from audio-thread automation normalization/denormalization.
    /// Returns `None` for parameters whose range is fixed at registration.
    fn live_param_range(&self, _id: u32) -> Option<ParamRange> {
        None
    }

    /// Cached immutable metadata for format adapters. The default builds the
    /// instance's current table with its stable string IDs attached. Root
    /// projection stores use one process-shared catalog because their complete
    /// ID list and ranges are immutable.
    fn param_catalog(&self) -> std::sync::Arc<ParamCatalog> {
        // Runtime tables (dynamic slots, runtime string IDs) can be wrong
        // in ways the derive's compile-time checks cannot see; degrade the
        // string-ID side map rather than panic inside a host callback.
        // On a valid table this is identical to `try_with_string_ids`.
        std::sync::Arc::new(ParamCatalog::with_string_ids_lossy(
            self.param_infos(),
            self.param_string_ids(),
        ))
    }

    /// Append parameter infos onto an existing buffer. Default impl
    /// delegates to [`Self::param_infos`] and `extend`s; the derive
    /// macro overrides for nested structs so deep trees don't pay
    /// O(depth) intermediate `Vec` allocations per outer call.
    fn append_param_infos(&self, into: &mut Vec<ParamInfo>) {
        into.extend(self.param_infos());
    }

    /// Static parameter metadata, available without an instance.
    ///
    /// Format wrappers' `register_*` paths call this to learn the
    /// parameter set without constructing a full plugin. The
    /// instance-based alternative would pay for any allocation the
    /// constructor does (DSP buffers, FFT plans, image atlases, etc.)
    /// at static-init time, which is fragile under AAX's `Describe`
    /// running before main. The derive macro overrides this with a
    /// `LazyLock`-cached `Vec<ParamInfo>` built from the same
    /// compile-time metadata it uses for [`Self::param_infos`], so
    /// registration becomes allocation-free after the first call.
    ///
    /// Default impl returns an empty vec - hand-written `Params` impls
    /// that don't override fall through to the runtime path inside
    /// `PluginExport::param_infos_static`. Gated by `Self: Sized` so
    /// adding the method preserves dyn-compatibility for the existing
    /// `&self`-method shape (`&dyn Params` skips this slot).
    #[must_use]
    fn param_infos_static() -> Vec<ParamInfo>
    where
        Self: Sized,
    {
        Vec::new()
    }

    /// Number of parameters.
    fn count(&self) -> usize;

    /// IDs of every `#[meter]` slot declared on the params struct
    /// (including nested subtrees), in declaration order. Default impl
    /// returns empty - only structs that declare meters need to
    /// override. The derive macro implements it automatically.
    ///
    /// Format wrappers that expose DSP-side meters back to the UI
    /// (LV2's output control ports, for instance) use this to know
    /// which IDs to poll each `process()`.
    fn meter_ids(&self) -> Vec<u32> {
        Vec::new()
    }

    /// Get normalized value (0.0–1.0) by ID.
    fn get_normalized(&self, id: u32) -> Option<f64>;

    /// Set normalized value (0.0–1.0) by ID.
    ///
    /// Takes `&self`, not `&mut self` - the per-param storage in
    /// `FloatParam` / `BoolParam` / `IntParam` / `EnumParam` is built
    /// on `AtomicU32` / `AtomicU64`, so writes go through interior
    /// mutability. Format wrappers, GUI editors, and the audio thread
    /// all hold `&Params` (or `Arc<Params>`) concurrently and write
    /// without coordination - every implementation must be sound under
    /// concurrent `&self` writes from multiple threads.
    fn set_normalized(&self, id: u32, value: f64);

    /// Set normalized value and read back the resulting plain value in
    /// one call. CLAP / AU forward the plain value to the host's
    /// automation channel after a GUI write. The default impl is the
    /// obvious `set_normalized` then `get_plain`; concrete `Params`
    /// implementations that can compute both in one trait dispatch
    /// (e.g. the `#[derive(Params)]` output) should override for a
    /// single match-arm walk.
    fn set_normalized_returning_plain(&self, id: u32, value: f64) -> f64 {
        self.set_normalized(id, value);
        self.get_plain(id).unwrap_or(0.0)
    }

    /// Set normalized value and read back the (post-clamp / post-step)
    /// normalized value in one call. VST3 / VST2 / AAX forward
    /// normalized values to the host's automation channel. Same
    /// override-for-single-dispatch contract as
    /// [`Self::set_normalized_returning_plain`].
    fn set_normalized_returning_normalized(&self, id: u32, value: f64) -> f64 {
        self.set_normalized(id, value);
        self.get_normalized(id).unwrap_or(0.0)
    }

    /// Get plain value by ID.
    fn get_plain(&self, id: u32) -> Option<f64>;

    /// Visual/bipolar origin value of `id` in plain units (for example `0.0`
    /// for a `-24..+24 dB` gain or `-1..+1` pan), used by hosts that support
    /// CLAP `clap.params-origin/1`. Defaults to `None` (no distinct origin).
    fn param_origin(&self, _id: u32) -> Option<f64> {
        None
    }

    /// Get plain value by dense declaration-order index, validated against `id`.
    ///
    /// Format wrappers that already resolved `id` to its dense catalog index
    /// (via a validated CLAP cookie or VST3 sorted index) call this to avoid
    /// a second `O(n)` ID-to-field scan.
    fn get_plain_by_index(&self, index: usize, id: u32) -> Option<f64> {
        let _ = index;
        self.get_plain(id)
    }

    /// Set plain value by ID.
    ///
    /// Same `&self` interior-mutability contract as
    /// [`Self::set_normalized`].
    fn set_plain(&self, id: u32, value: f64);

    /// Set plain value by dense declaration-order index, validated against `id`.
    ///
    /// Same `&self` interior-mutability contract as [`Self::set_plain`].
    fn set_plain_by_index(&self, index: usize, id: u32, value: f64) {
        let _ = index;
        self.set_plain(id, value);
    }

    /// Set the mono modulation offset of `id` in plain units (CLAP
    /// `CLAP_EVENT_PARAM_MOD` with no note). Non-destructive: the stored
    /// value, the edit counter and saved state never move; only
    /// `FloatParam::modulated_value` (and `DynamicParams::modulated_plain`)
    /// read it. Non-finite amounts are dropped. The chunker calls this at
    /// the event's sub-block boundary on the audio thread, so it must not
    /// allocate, lock or block. Unknown ids and parameters that can't be
    /// modulated (bool, int, enum, list, projected views) ignore it.
    /// Default: no-op (`#[derive(Params)]` implements it).
    fn set_modulation(&self, _id: u32, _amount: f64) {}

    /// Zero every modulation offset in this store, including nested
    /// groups. Same real-time contract as [`Self::set_modulation`];
    /// wrappers call it on reset / deactivate, where the host's modulation
    /// stream restarts. Default: no-op.
    fn clear_modulations(&self) {}

    /// Format a value in the parameter's declared plain/storage range.
    /// `#[params(value_format = "method")]` first calls an instance method
    /// `fn(&self, id: u32, value: f64) -> Option<String>`. Returning `None`
    /// keeps the generated field/nested formatter. This can give a fixed
    /// automation slot its current target's musical display without changing
    /// its range, stored values or automation transport.
    fn format_value(&self, id: u32, value: f64) -> Option<String>;

    /// Format a value by dense declaration-order index, validated against `id`.
    fn format_value_by_index(&self, index: usize, id: u32, value: f64) -> Option<String> {
        let _ = index;
        self.format_value(id, value)
    }

    /// Parse display text into the parameter's declared plain/storage range.
    /// `#[params(value_parse = "method")]` first calls an instance method
    /// `fn(&self, id: u32, text: &str) -> Result<Option<f64>, ()>`.
    /// `Ok(Some(value))` handles the input (non-finite values are rejected),
    /// `Ok(None)` uses generated field/nested parsing, and `Err(())` rejects
    /// input without falling back to a potentially different numeric meaning.
    fn parse_value(&self, id: u32, text: &str) -> Option<f64>;

    /// Parse display text by dense declaration-order index, validated against `id`.
    fn parse_value_by_index(&self, index: usize, id: u32, text: &str) -> Option<f64> {
        let _ = index;
        self.parse_value(id, text)
    }

    /// Reset all smoothers to current values.
    fn snap_smoothers(&self);

    /// Update smoother sample rates.
    fn set_sample_rate(&self, sample_rate: f64);

    /// Collect all parameter IDs and their current plain values.
    fn collect_values(&self) -> (Vec<u32>, Vec<f64>);

    /// Restore parameter values from a list of (id, value) pairs.
    fn restore_values(&self, values: &[(u32, f64)]);

    /// Restore a saved state's parameter values and `#[persist]` blob as
    /// one operation. Every state-load path in `moose-core` calls this
    /// (after [`Self::validate_persist`] accepted the blob), so a store
    /// that must reconcile the two representations overrides it
    /// (derive: `#[params(restore_state = "method")]`).
    fn restore_state(&self, values: &[(u32, f64)], persist: &[u8]) {
        self.restore_values(values);
        self.load_persist(persist);
    }

    /// Check a saved `#[persist]` blob before anything is restored.
    /// Returning `false` rejects the whole state load, so the store is
    /// never left half-restored from a document it can't read. Runs on
    /// the host thread. Default: accept everything (derive:
    /// `#[params(validate_persist = "function")]`).
    #[must_use]
    fn validate_persist(_data: &[u8]) -> bool
    where
        Self: Sized,
    {
        true
    }

    /// Serialize this store's `#[persist]` fields into a keyed blob the
    /// host saves alongside the parameter values. Default: empty (no
    /// persist fields). The `#[derive(Params)]` macro overrides it when
    /// any field carries `#[persist]`.
    #[must_use]
    fn serialize_persist(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Freeze persisted controls and payload ownership for incremental output.
    /// The default retains existing serialization behavior; the derive prepares
    /// keyed fields individually so large immutable fields can avoid staging.
    /// Never call this or the returned writer from an audio callback.
    ///
    /// # Errors
    /// A custom snapshot may reject an unrepresentable or unavailable document.
    fn prepare_persist_write(&self) -> std::io::Result<Box<dyn PreparedPersistWrite>> {
        Ok(Box::new(self.serialize_persist()))
    }

    /// Opt-in asynchronous persistent-document completion. No polling is
    /// needed: a field wakes its wrapper through the weak signal below.
    fn wants_persist_notifications(&self) -> bool {
        false
    }

    /// Bound once by the format wrapper, independently of editor lifetime.
    fn set_persist_change_signal(&self, _signal: std::sync::Weak<PersistChangeSignal>) {}

    /// Host/UI thread only, after a completion wake. Return a small parameter
    /// beacon only for a real, not-yet-announced persistent change. The wrapper
    /// delivers the normal begin/value/end gesture; return None for no-ops.
    /// `#[params(persist_changed = "method")]` supplies this optional hook.
    fn take_persist_change(&self) -> Option<(u32, f64)> {
        None
    }

    /// Visit every persisted field, including nested stores and pre-save hooks.
    /// The derive emits this from the same field inventory as serialization.
    ///
    /// This is non-realtime: a field without a cached identity uses its existing
    /// serializer and may allocate or lock. Large fields should provide a cheap
    /// identity prepared at mutation/publication time. The default preserves
    /// manual implementations by exposing their complete serialized state.
    fn visit_persist_fingerprints(&self, visitor: &mut dyn PersistFingerprintVisitor) {
        visitor.serialized(&self.serialize_persist());
    }

    /// Restore this store's `#[persist]` fields from a blob produced by
    /// [`Self::serialize_persist`]. Unknown / missing keys are skipped,
    /// leaving those fields at their current value. Default: no-op.
    fn load_persist(&self, data: &[u8]) {
        let _ = data;
    }

    /// Walk every parameter and meter ID reachable from `self`
    /// (including nested `#[nested]` substructs) and panic on the
    /// first duplicate.
    ///
    /// Why this isn't just a compile-time check: the
    /// `#[derive(Params)]` collision check at expansion time only
    /// sees IDs declared in the *current* struct. A parent param
    /// `id = 5` and a nested-substruct param `id = 5` both compile,
    /// because the parent derive doesn't see into the nested type.
    /// At runtime, the `set_plain` / `get_plain` dispatcher matches
    /// at the outer level first and silently never reaches the
    /// nested one - preset round-trips would corrupt the nested
    /// value. This method makes that bug surface as a panic at
    /// plugin construction instead of as quiet state loss.
    ///
    /// Called automatically by the derive-generated `Self::new()`.
    /// Plugin code shouldn't need to invoke it directly.
    fn assert_no_id_collisions(&self) {
        // Construction-time check of the declared tree, so it is strict;
        // runtime catalogs go through the non-panicking `param_catalog`.
        let catalog =
            match ParamCatalog::try_with_string_ids(self.param_infos(), self.param_string_ids()) {
                Ok(catalog) => catalog,
                Err(err) => panic!("{err}"),
            };
        let mut seen_meters = std::collections::HashSet::new();
        for meter_id in self.meter_ids() {
            if let Some(previous) = catalog.by_id(meter_id) {
                panic!(
                    "meter ID {meter_id} collides with parameter ID for '{}'",
                    previous.name
                );
            }
            assert!(
                seen_meters.insert(meter_id),
                "duplicate meter ID {meter_id} (two #[nested] structs each declare a meter; nested meters aren't supported - keep meters in a single Params struct)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::ParamRange;

    fn pan_info() -> ParamInfo {
        ParamInfo {
            id: 0,
            name: "Pan",
            short_name: "Pan",
            group: "",
            range: ParamRange::Linear {
                min: -1.0,
                max: 1.0,
            },
            default_plain: 0.0,
            flags: ParamFlags::empty(),
            unit: ParamUnit::Pan,
            kind: ParamValueKind::Float,
            midi_map: None,
            midi_channel: None,
        }
    }

    #[test]
    fn pan_centre() {
        let info = pan_info();
        assert_eq!(format_param_value(&info, 0.0), "C");
        assert_eq!(format_param_value(&info, 0.004), "C");
        assert_eq!(format_param_value(&info, -0.004), "C");
    }

    #[test]
    fn pan_left() {
        let info = pan_info();
        assert_eq!(format_param_value(&info, -0.5), "50L");
        assert_eq!(format_param_value(&info, -1.0), "100L");
        assert_eq!(format_param_value(&info, -0.006), "1L");
    }

    #[test]
    fn pan_right() {
        let info = pan_info();
        assert_eq!(format_param_value(&info, 0.5), "50R");
        assert_eq!(format_param_value(&info, 1.0), "100R");
        assert_eq!(format_param_value(&info, 0.006), "1R");
    }

    fn int_info(unit: ParamUnit) -> ParamInfo {
        ParamInfo {
            id: 0,
            name: "n",
            short_name: "n",
            group: "",
            range: ParamRange::Discrete { min: -12, max: 12 },
            default_plain: 0.0,
            flags: ParamFlags::empty(),
            unit,
            kind: ParamValueKind::Int,
            midi_map: None,
            midi_channel: None,
        }
    }

    #[test]
    fn int_param_no_fractional_zero() {
        // IntParam values must render with no decimal places.
        // A hard-coded `{:.1}` formatter (regardless of param kind)
        // would render "0.0 st" / "-5.0 st" for semitone values.
        assert_eq!(
            format_param_value(&int_info(ParamUnit::Semitones), 0.0),
            "0 st"
        );
        assert_eq!(
            format_param_value(&int_info(ParamUnit::Semitones), -5.0),
            "-5 st"
        );
        assert_eq!(format_param_value(&int_info(ParamUnit::None), 0.0), "0");
        assert_eq!(format_param_value(&int_info(ParamUnit::Db), 6.0), "6 dB");
        assert_eq!(
            format_param_value(&int_info(ParamUnit::Milliseconds), 50.0),
            "50 ms"
        );
    }
}
