//! Runtime parameters on a fixed pool of host slots.
//!
//! A plugin that builds its parameter set at runtime (modular routing, macro
//! slots, per-instance targets) declares `#[nested] slots: DynamicParams<N>`.
//! The host sees `N` parameters with stable ids that are never reused. At
//! runtime each slot is bound to a [`SlotDef`] (name, group, range, default,
//! function) or left free. Wrappers map that onto what the format allows:
//! CLAP lists only bound slots and rescans after a restart, VST3 reloads
//! the component where the host supports it, and everything else keeps the
//! full slot list and hides free slots.
//!
//! Values are lock-free atomics on every thread. Slot metadata changes only
//! through [`DynamicParams::edit`] off the audio thread; the commit wakes the
//! wrapper through its [`CatalogSignal`] instead of being polled.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use crate::catalog::{CatalogChange, CatalogChanges, CatalogSignal};
use crate::{
    ParamFlags, ParamFunction, ParamInfo, ParamRange, ParamUnit, ParamValueKind,
    ParameterPresentation, Params,
};

/// Host-facing name of every slot; the bound name is the presentation name.
const SLOT_NAME: &str = "Slot";
/// Leading byte of the persisted binding table.
const PERSIST_VERSION: u8 = 1;

/// The value shape of a bound slot. Every variant has a well-defined
/// normalized mapping, so a slot can never hold an unencodable range.
#[derive(Clone, Debug, PartialEq)]
pub enum SlotRange {
    /// Linear `min..=max`.
    Linear { min: f64, max: f64 },
    /// Logarithmic `min..=max`; both must be positive.
    Log { min: f64, max: f64 },
    /// Whole numbers `min..=max`.
    Int { min: i32, max: i32 },
    /// Off / on.
    Toggle,
    /// One of the labelled choices (at least one).
    Choice(Vec<String>),
}

impl SlotRange {
    fn shape(&self) -> Shape {
        match *self {
            Self::Linear { min, max } => Shape {
                tag: Tag::Linear,
                a: min,
                b: max,
            },
            Self::Log { min, max } => Shape {
                tag: Tag::Log,
                a: min,
                b: max,
            },
            Self::Int { min, max } => Shape {
                tag: Tag::Int,
                a: f64::from(min),
                b: f64::from(max),
            },
            Self::Toggle => Shape {
                tag: Tag::Toggle,
                a: 0.0,
                b: 1.0,
            },
            Self::Choice(ref labels) => Shape {
                tag: Tag::Choice,
                a: 0.0,
                #[allow(clippy::cast_precision_loss, reason = "label counts are tiny")]
                b: labels.len().max(1) as f64,
            },
        }
    }
}

/// What a bound slot is. Build with [`SlotDef::new`] and the `with_*`
/// setters.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotDef {
    name: String,
    group: String,
    range: SlotRange,
    default: f64,
    function: Option<ParamFunction>,
}

impl SlotDef {
    /// A slot named `name` over `range`, defaulting to the range's minimum.
    #[must_use]
    pub fn new(name: impl Into<String>, range: SlotRange) -> Self {
        let default = range.shape().param_range().denormalize(0.0);
        Self {
            name: name.into(),
            group: String::new(),
            range,
            default,
            function: None,
        }
    }
    /// Host group (module path) for the slot.
    #[must_use]
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = group.into();
        self
    }
    /// Default plain value, clamped into the range.
    #[must_use]
    pub fn with_default(mut self, plain: f64) -> Self {
        let range = self.range.shape().param_range();
        self.default = range.denormalize(range.normalize(plain));
        self
    }
    /// The role hosts know this slot by.
    #[must_use]
    pub fn with_function(mut self, function: ParamFunction) -> Self {
        self.function = Some(function);
        self
    }
    /// The display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The value shape.
    #[must_use]
    pub fn range(&self) -> &SlotRange {
        &self.range
    }
}

/// A slot of one [`DynamicParams`] pool. Only the pool hands these out, so
/// an edit can never name a slot that doesn't exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Slot(usize);

impl Slot {
    /// Position in the pool, `0..N`.
    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Tag {
    Linear,
    Log,
    Int,
    Toggle,
    Choice,
}

impl Tag {
    fn from_u8(v: u8) -> Option<Self> {
        [
            Self::Linear,
            Self::Log,
            Self::Int,
            Self::Toggle,
            Self::Choice,
        ]
        .into_iter()
        .find(|t| *t as u8 == v)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Shape {
    tag: Tag,
    a: f64,
    b: f64,
}

impl Shape {
    const FREE: Self = Self {
        tag: Tag::Linear,
        a: 0.0,
        b: 1.0,
    };

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "bounds come from i32 / label counts"
    )]
    fn param_range(self) -> ParamRange {
        match self.tag {
            Tag::Linear => ParamRange::Linear {
                min: self.a,
                max: self.b,
            },
            Tag::Log => ParamRange::Logarithmic {
                min: self.a,
                max: self.b,
            },
            Tag::Int => ParamRange::Discrete {
                min: self.a as i64,
                max: self.b as i64,
            },
            Tag::Toggle => ParamRange::Discrete { min: 0, max: 1 },
            Tag::Choice => ParamRange::Enum {
                count: self.b as usize,
            },
        }
    }

    fn kind(self) -> ParamValueKind {
        match self.tag {
            Tag::Linear | Tag::Log => ParamValueKind::Float,
            Tag::Int => ParamValueKind::Int,
            Tag::Toggle => ParamValueKind::Bool,
            Tag::Choice => ParamValueKind::Enum,
        }
    }
}

/// A slot's shape, readable from the audio thread without a lock: a
/// sequence lock over plain atomics. The single writer is [`DynamicParams`]
/// under its layout mutex.
struct ShapeCell {
    seq: AtomicU32,
    tag: AtomicU8,
    a: AtomicU64,
    b: AtomicU64,
}

impl ShapeCell {
    fn new(shape: Shape) -> Self {
        Self {
            seq: AtomicU32::new(0),
            tag: AtomicU8::new(shape.tag as u8),
            a: AtomicU64::new(shape.a.to_bits()),
            b: AtomicU64::new(shape.b.to_bits()),
        }
    }

    fn load(&self) -> Shape {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 == 0 {
                let tag = self.tag.load(Ordering::Relaxed);
                let a = f64::from_bits(self.a.load(Ordering::Relaxed));
                let b = f64::from_bits(self.b.load(Ordering::Relaxed));
                fence(Ordering::Acquire);
                if self.seq.load(Ordering::Relaxed) == before {
                    return Shape {
                        tag: Tag::from_u8(tag).unwrap_or(Tag::Linear),
                        a,
                        b,
                    };
                }
            }
            std::hint::spin_loop();
        }
    }

    /// Caller holds the layout lock (one writer).
    fn store(&self, shape: Shape) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.tag.store(shape.tag as u8, Ordering::Relaxed);
        self.a.store(shape.a.to_bits(), Ordering::Relaxed);
        self.b.store(shape.b.to_bits(), Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }
}

struct SlotCell {
    id: u32,
    /// Plain value as f64 bits. Plain, so a value restored before its
    /// binding keeps its meaning once the binding arrives.
    value: AtomicU64,
    bound: AtomicBool,
    shape: ShapeCell,
}

#[derive(Default)]
struct Layout {
    defs: Vec<Option<SlotDef>>,
    /// Host ids removed or retyped since the wrapper last took changes.
    removed: Vec<u32>,
}

/// A pool of `N` runtime-defined parameters. See the module docs.
pub struct DynamicParams<const N: usize> {
    slots: Box<[SlotCell]>,
    layout: Mutex<Layout>,
    revision: AtomicU64,
    pending: AtomicU8,
    signal: Mutex<Weak<CatalogSignal>>,
    edit_epoch: Option<Arc<AtomicU64>>,
}

impl<const N: usize> Default for DynamicParams<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> std::fmt::Debug for DynamicParams<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DynamicParams")
            .field("slots", &N)
            .field("bound", &self.bound().len())
            .finish()
    }
}

impl<const N: usize> DynamicParams<N> {
    /// `N` free slots with ids `0..N` (rebased when nested).
    ///
    /// # Panics
    /// If `N` doesn't fit the parameter id range.
    #[must_use]
    pub fn new() -> Self {
        let slots = (0..N)
            .map(|i| SlotCell {
                id: u32::try_from(i).expect("slot count fits a parameter id"),
                value: AtomicU64::new(0f64.to_bits()),
                bound: AtomicBool::new(false),
                shape: ShapeCell::new(Shape::FREE),
            })
            .collect();
        Self {
            slots,
            layout: Mutex::new(Layout {
                defs: vec![None; N],
                ..Layout::default()
            }),
            revision: AtomicU64::new(0),
            pending: AtomicU8::new(0),
            signal: Mutex::new(Weak::new()),
            edit_epoch: None,
        }
    }

    /// Every slot, in order.
    pub fn slots(&self) -> impl Iterator<Item = Slot> {
        (0..N).map(Slot)
    }

    /// The slot at `index`, if the pool has one.
    #[must_use]
    pub fn slot(&self, index: usize) -> Option<Slot> {
        (index < N).then_some(Slot(index))
    }

    /// The slot behind a host parameter id.
    #[must_use]
    pub fn slot_for_id(&self, id: u32) -> Option<Slot> {
        let first = self.slots.first()?.id;
        let index = usize::try_from(id.wrapping_sub(first) & crate::AUTO_PARAM_ID_MASK).ok()?;
        (self.slots.get(index)?.id == id).then_some(Slot(index))
    }

    /// The host parameter id of `slot`.
    #[must_use]
    pub fn id(&self, slot: Slot) -> u32 {
        self.slots[slot.0].id
    }

    /// Normalized value, lock-free.
    #[must_use]
    pub fn normalized(&self, slot: Slot) -> f64 {
        let cell = &self.slots[slot.0];
        cell.shape
            .load()
            .param_range()
            .normalize(f64::from_bits(cell.value.load(Ordering::Relaxed)))
    }

    /// Plain value in the slot's current range, lock-free.
    #[must_use]
    pub fn plain(&self, slot: Slot) -> f64 {
        f64::from_bits(self.slots[slot.0].value.load(Ordering::Relaxed))
    }

    /// Bound slots and their definitions. Not for the audio thread.
    #[must_use]
    pub fn bound(&self) -> Vec<(Slot, SlotDef)> {
        self.lock()
            .defs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| Some((Slot(i), d.clone()?)))
            .collect()
    }

    /// The definition bound to `slot`. Not for the audio thread.
    #[must_use]
    pub fn def(&self, slot: Slot) -> Option<SlotDef> {
        self.lock().defs[slot.0].clone()
    }

    /// Change bindings in one transaction and tell the wrapper once.
    /// Not for the audio thread: it locks and may wake the host.
    pub fn edit<R>(&self, f: impl FnOnce(&mut SlotEdit<'_, N>) -> R) -> R {
        let mut layout = self.lock();
        let before = layout.defs.clone();
        let result = f(&mut SlotEdit {
            defs: &mut layout.defs,
        });
        let change = self.commit(&mut layout, &before, true);
        drop(layout);
        self.publish(change);
        result
    }

    fn lock(&self) -> MutexGuard<'_, Layout> {
        self.layout.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reshape changed slots and record what the host must hear. With
    /// `reset_values`, a newly bound or retyped slot starts at its default.
    fn commit(
        &self,
        layout: &mut Layout,
        before: &[Option<SlotDef>],
        reset_values: bool,
    ) -> CatalogChange {
        let mut change = CatalogChange::empty();
        for (i, (old, new)) in before.iter().zip(layout.defs.clone()).enumerate() {
            if *old == new {
                continue;
            }
            let cell = &self.slots[i];
            let old_shape = old.as_ref().map_or(Shape::FREE, |d| d.range.shape());
            let new_shape = new.as_ref().map_or(Shape::FREE, |d| d.range.shape());
            let structural = old.is_some() != new.is_some()
                || old_shape != new_shape
                || old.as_ref().map(|d| d.function) != new.as_ref().map(|d| d.function);
            if structural {
                change |= CatalogChange::STRUCTURE;
                if old.is_some() {
                    layout.removed.push(cell.id);
                }
                cell.shape.store(new_shape);
                cell.bound.store(new.is_some(), Ordering::Relaxed);
                let range = new_shape.param_range();
                let plain = match &new {
                    Some(def) if reset_values => def.default,
                    _ => f64::from_bits(cell.value.load(Ordering::Relaxed)),
                };
                let plain = if new.is_some() {
                    range.denormalize(range.normalize(plain))
                } else {
                    plain
                };
                if cell.value.swap(plain.to_bits(), Ordering::Relaxed) != plain.to_bits() {
                    change |= CatalogChange::VALUES;
                }
            } else {
                change |= CatalogChange::INFO;
            }
        }
        change
    }

    fn publish(&self, change: CatalogChange) {
        if change.is_empty() {
            return;
        }
        self.revision.fetch_add(1, Ordering::Release);
        self.note_edit();
        self.pending.fetch_or(change.bits(), Ordering::AcqRel);
        let signal = self
            .signal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .upgrade();
        if let Some(signal) = signal {
            signal.notify();
        }
    }

    fn note_edit(&self) {
        if let Some(epoch) = &self.edit_epoch {
            epoch.fetch_add(1, Ordering::Release);
        }
    }

    /// Store a plain value: clamped into a bound slot's range, kept as is
    /// on a free slot (state may restore it before the binding).
    fn store_plain(&self, slot: Slot, plain: f64) {
        if !plain.is_finite() {
            return;
        }
        let cell = &self.slots[slot.0];
        let plain = if cell.bound.load(Ordering::Relaxed) {
            let range = cell.shape.load().param_range();
            range.denormalize(range.normalize(plain))
        } else {
            plain
        };
        if cell.value.swap(plain.to_bits(), Ordering::Relaxed) != plain.to_bits() {
            self.note_edit();
        }
    }

    fn info_at(&self, i: usize, shape: Shape, def: Option<&SlotDef>) -> ParamInfo {
        let range = shape.param_range();
        let mut flags = ParamFlags::AUTOMATABLE | ParamFlags::CHUNKED;
        if def.is_none() {
            flags |= ParamFlags::HIDDEN;
        }
        ParamInfo {
            id: self.slots[i].id,
            name: SLOT_NAME,
            short_name: SLOT_NAME,
            group: "",
            range,
            default_plain: def.map_or(0.0, |d| d.default),
            flags,
            unit: ParamUnit::None,
            kind: shape.kind(),
            midi_map: None,
            midi_channel: None,
        }
    }

    /// Fold a nested group's base into every slot id (derive support).
    #[doc(hidden)]
    pub fn offset_ids(&mut self, id_base: u32) {
        for slot in &mut self.slots {
            slot.id = crate::rebase_nested_param_id(slot.id, id_base);
        }
    }

    /// Bind a containing tree's edit counter (derive support).
    #[doc(hidden)]
    pub fn set_edit_epoch(&mut self, epoch: Arc<AtomicU64>) {
        self.edit_epoch = Some(epoch);
    }
}

/// One [`DynamicParams::edit`] transaction.
pub struct SlotEdit<'a, const N: usize> {
    defs: &'a mut Vec<Option<SlotDef>>,
}

impl<const N: usize> SlotEdit<'_, N> {
    /// Bind `slot` to `def`, replacing any earlier binding.
    pub fn bind(&mut self, slot: Slot, def: SlotDef) {
        self.defs[slot.0] = Some(def);
    }
    /// Free `slot`; the host stops seeing it.
    pub fn unbind(&mut self, slot: Slot) {
        self.defs[slot.0] = None;
    }
    /// Rename a bound slot (no-op on a free one).
    pub fn rename(&mut self, slot: Slot, name: impl Into<String>) {
        if let Some(def) = &mut self.defs[slot.0] {
            def.name = name.into();
        }
    }
    /// The first free slot.
    #[must_use]
    pub fn first_free(&self) -> Option<Slot> {
        self.defs.iter().position(Option::is_none).map(Slot)
    }
    /// What `slot` is bound to in this transaction.
    #[must_use]
    pub fn def(&self, slot: Slot) -> Option<&SlotDef> {
        self.defs[slot.0].as_ref()
    }
}

impl<const N: usize> crate::__private::Sealed for DynamicParams<N> {}

impl<const N: usize> Params for DynamicParams<N> {
    fn parameter_presentation(&self, id: u32) -> Option<ParameterPresentation> {
        let slot = self.slot_for_id(id)?;
        Some(match self.def(slot) {
            Some(def) => ParameterPresentation {
                name: def.name,
                group: def.group,
                hidden: false,
                available: true,
            },
            None => ParameterPresentation {
                name: format!("{SLOT_NAME} {}", slot.0 + 1),
                group: String::new(),
                hidden: true,
                available: false,
            },
        })
    }

    fn parameter_presentation_revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    fn parameter_function(&self, id: u32) -> Option<ParamFunction> {
        self.def(self.slot_for_id(id)?)?.function
    }

    fn set_catalog_signal(&self, signal: Weak<CatalogSignal>) {
        *self.signal.lock().unwrap_or_else(PoisonError::into_inner) = signal;
    }

    fn take_catalog_changes(&self) -> CatalogChanges {
        let flags = CatalogChange::from_bits_truncate(self.pending.swap(0, Ordering::AcqRel));
        let removed = if flags.contains(CatalogChange::STRUCTURE) {
            std::mem::take(&mut self.lock().removed)
        } else {
            Vec::new()
        };
        CatalogChanges { flags, removed }
    }

    fn parameter_edit_epoch(&self) -> u64 {
        self.edit_epoch
            .as_ref()
            .map_or_else(crate::edit_epoch, |e| e.load(Ordering::Acquire))
    }

    fn set_edit_epoch(&mut self, epoch: Arc<AtomicU64>) {
        Self::set_edit_epoch(self, epoch);
    }

    fn param_infos(&self) -> Vec<ParamInfo> {
        let layout = self.lock();
        (0..N)
            .map(|i| self.info_at(i, self.slots[i].shape.load(), layout.defs[i].as_ref()))
            .collect()
    }

    fn param_infos_static() -> Vec<ParamInfo>
    where
        Self: Sized,
    {
        Self::new().param_infos()
    }

    fn count(&self) -> usize {
        N
    }

    fn get_normalized(&self, id: u32) -> Option<f64> {
        Some(self.normalized(self.slot_for_id(id)?))
    }

    fn set_normalized(&self, id: u32, value: f64) {
        if let Some(slot) = self.slot_for_id(id)
            && value.is_finite()
        {
            let range = self.slots[slot.0].shape.load().param_range();
            self.store_plain(slot, range.denormalize(value.clamp(0.0, 1.0)));
        }
    }

    fn get_plain(&self, id: u32) -> Option<f64> {
        Some(self.plain(self.slot_for_id(id)?))
    }

    fn set_plain(&self, id: u32, value: f64) {
        if let Some(slot) = self.slot_for_id(id) {
            self.store_plain(slot, value);
        }
    }

    fn format_value(&self, id: u32, value: f64) -> Option<String> {
        let slot = self.slot_for_id(id)?;
        let def = self.def(slot);
        if let Some(SlotDef {
            range: SlotRange::Choice(labels),
            ..
        }) = &def
        {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "rounded, clamped index"
            )]
            let index = (value.round().max(0.0) as usize).min(labels.len().saturating_sub(1));
            return labels.get(index).cloned();
        }
        let shape = self.slots[slot.0].shape.load();
        Some(crate::format_param_value(
            &self.info_at(slot.0, shape, def.as_ref()),
            value,
        ))
    }

    fn parse_value(&self, id: u32, text: &str) -> Option<f64> {
        let slot = self.slot_for_id(id)?;
        let text = text.trim();
        if let Some(SlotDef {
            range: SlotRange::Choice(labels),
            ..
        }) = self.def(slot)
        {
            #[allow(clippy::cast_precision_loss, reason = "label counts are tiny")]
            return labels
                .iter()
                .position(|l| l.eq_ignore_ascii_case(text))
                .map(|i| i as f64);
        }
        let value: f64 = text.split_whitespace().next()?.parse().ok()?;
        value.is_finite().then_some(value)
    }

    fn snap_smoothers(&self) {}

    fn set_sample_rate(&self, _sample_rate: f64) {}

    fn collect_values(&self) -> (Vec<u32>, Vec<f64>) {
        self.slots().map(|s| (self.id(s), self.plain(s))).unzip()
    }

    fn restore_values(&self, values: &[(u32, f64)]) {
        for &(id, value) in values {
            self.set_plain(id, value);
        }
    }

    fn serialize_persist(&self) -> Vec<u8> {
        persist::encode(&self.lock().defs)
    }

    fn load_persist(&self, data: &[u8]) {
        let Some(defs) = persist::decode(data, N) else {
            return;
        };
        let mut layout = self.lock();
        let before = std::mem::replace(&mut layout.defs, defs);
        let change = self.commit(&mut layout, &before, false);
        drop(layout);
        self.publish(change | CatalogChange::VALUES);
    }
}

/// The binding table as bytes: version, then per slot a presence byte and
/// the definition. Little-endian, length-prefixed strings.
mod persist {
    use super::{PERSIST_VERSION, SlotDef, SlotRange};
    use crate::ParamFunction;

    const FUNCTIONS: [ParamFunction; 7] = [
        ParamFunction::DryWetMix,
        ParamFunction::LowLatencyMode,
        ParamFunction::Randomize,
        ParamFunction::GainReduction,
        ParamFunction::GainReductionMax,
        ParamFunction::GainReductionPeakHold,
        ParamFunction::ResetGainReductionMax,
    ];

    pub(super) fn encode(defs: &[Option<SlotDef>]) -> Vec<u8> {
        let mut out = vec![PERSIST_VERSION];
        put_u32(&mut out, u32::try_from(defs.len()).unwrap_or(u32::MAX));
        for def in defs {
            let Some(def) = def else {
                out.push(0);
                continue;
            };
            out.push(1);
            put_str(&mut out, &def.name);
            put_str(&mut out, &def.group);
            out.extend(def.default.to_le_bytes());
            out.push(
                def.function
                    .and_then(|f| FUNCTIONS.iter().position(|g| *g == f))
                    .map_or(0, |i| u8::try_from(i + 1).unwrap_or(0)),
            );
            match &def.range {
                SlotRange::Linear { min, max } => put_pair(&mut out, 0, *min, *max),
                SlotRange::Log { min, max } => put_pair(&mut out, 1, *min, *max),
                SlotRange::Int { min, max } => {
                    put_pair(&mut out, 2, f64::from(*min), f64::from(*max));
                }
                SlotRange::Toggle => out.push(3),
                SlotRange::Choice(labels) => {
                    out.push(4);
                    put_u32(&mut out, u32::try_from(labels.len()).unwrap_or(u32::MAX));
                    for label in labels {
                        put_str(&mut out, label);
                    }
                }
            }
        }
        out
    }

    /// `None` for anything that isn't a table of exactly `slots` entries.
    pub(super) fn decode(data: &[u8], slots: usize) -> Option<Vec<Option<SlotDef>>> {
        let mut r = Reader(data);
        if r.u8()? != PERSIST_VERSION || usize::try_from(r.u32()?).ok()? != slots {
            return None;
        }
        let mut defs = Vec::with_capacity(slots);
        for _ in 0..slots {
            if r.u8()? == 0 {
                defs.push(None);
                continue;
            }
            let name = r.str()?;
            let group = r.str()?;
            let default = r.f64()?;
            let function = match r.u8()? {
                0 => None,
                i => Some(*FUNCTIONS.get(usize::from(i) - 1)?),
            };
            let range = match r.u8()? {
                0 => SlotRange::Linear {
                    min: r.f64()?,
                    max: r.f64()?,
                },
                1 => SlotRange::Log {
                    min: r.f64()?,
                    max: r.f64()?,
                },
                #[allow(clippy::cast_possible_truncation, reason = "written from i32")]
                2 => SlotRange::Int {
                    min: r.f64()? as i32,
                    max: r.f64()? as i32,
                },
                3 => SlotRange::Toggle,
                4 => SlotRange::Choice((0..r.u32()?).map(|_| r.str()).collect::<Option<_>>()?),
                _ => return None,
            };
            let mut def = SlotDef::new(name, range)
                .with_group(group)
                .with_default(default);
            def.function = function;
            defs.push(Some(def));
        }
        r.0.is_empty().then_some(defs)
    }

    fn put_u32(out: &mut Vec<u8>, v: u32) {
        out.extend(v.to_le_bytes());
    }
    fn put_str(out: &mut Vec<u8>, s: &str) {
        put_u32(out, u32::try_from(s.len()).unwrap_or(u32::MAX));
        out.extend(s.as_bytes());
    }
    fn put_pair(out: &mut Vec<u8>, tag: u8, a: f64, b: f64) {
        out.push(tag);
        out.extend(a.to_le_bytes());
        out.extend(b.to_le_bytes());
    }

    struct Reader<'a>(&'a [u8]);
    impl Reader<'_> {
        fn take<const K: usize>(&mut self) -> Option<[u8; K]> {
            let (head, rest) = self.0.split_first_chunk::<K>()?;
            self.0 = rest;
            Some(*head)
        }
        fn u8(&mut self) -> Option<u8> {
            self.take::<1>().map(|[b]| b)
        }
        fn u32(&mut self) -> Option<u32> {
            self.take().map(u32::from_le_bytes)
        }
        fn f64(&mut self) -> Option<f64> {
            self.take()
                .map(f64::from_le_bytes)
                .filter(|v| v.is_finite())
        }
        fn str(&mut self) -> Option<String> {
            let len = usize::try_from(self.u32()?).ok()?;
            let (head, rest) = (self.0.get(..len)?, self.0.get(len..)?);
            self.0 = rest;
            String::from_utf8(head.to_vec()).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn cutoff() -> SlotDef {
        SlotDef::new(
            "Cutoff",
            SlotRange::Log {
                min: 20.0,
                max: 20_000.0,
            },
        )
        .with_group("Osc 1")
        .with_default(1000.0)
    }

    #[test]
    fn bind_publishes_structure_and_wakes_once() {
        let params = DynamicParams::<4>::new();
        let wakes = Arc::new(AtomicUsize::new(0));
        let signal = Arc::new(CatalogSignal::new({
            let wakes = Arc::clone(&wakes);
            move || {
                wakes.fetch_add(1, Ordering::Relaxed);
            }
        }));
        params.set_catalog_signal(Arc::downgrade(&signal));
        let slot = params.edit(|tx| {
            let slot = tx.first_free().expect("free slot");
            tx.bind(slot, cutoff());
            slot
        });
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        let changes = params.take_catalog_changes();
        assert!(
            changes
                .flags
                .contains(CatalogChange::STRUCTURE | CatalogChange::VALUES),
            "{changes:?}"
        );
        assert_eq!(changes.removed, Vec::<u32>::new());
        assert!(
            (params.plain(slot) - 1000.0).abs() < 1e-6,
            "starts at its default"
        );
        assert!(params.take_catalog_changes().flags.is_empty(), "taken once");

        params.edit(|tx| tx.rename(slot, "Freq"));
        assert_eq!(params.take_catalog_changes().flags, CatalogChange::INFO);
        assert_eq!(
            params
                .parameter_presentation(params.id(slot))
                .expect("bound")
                .name,
            "Freq"
        );

        params.edit(|tx| tx.unbind(slot));
        let changes = params.take_catalog_changes();
        assert_eq!(changes.removed, [params.id(slot)]);
        assert!(
            !params
                .parameter_presentation(params.id(slot))
                .expect("slot")
                .available
        );
    }

    #[test]
    fn values_are_atomic_and_follow_the_slot_range() {
        let params = DynamicParams::<2>::new();
        let slot = params.slot(1).expect("slot 1");
        params.edit(|tx| {
            tx.bind(
                slot,
                SlotDef::new("Steps", SlotRange::Int { min: -4, max: 4 }),
            );
        });
        params.set_plain(params.id(slot), 2.0);
        assert!((params.normalized(slot) - 0.75).abs() < 1e-9);
        assert_eq!(params.get_plain(params.id(slot)), Some(2.0));
        assert_eq!(params.param_infos()[1].kind, ParamValueKind::Int);
        assert!(
            params.param_infos()[0].flags.contains(ParamFlags::HIDDEN),
            "free slot hidden"
        );
    }

    #[test]
    fn nested_ids_rebase_and_resolve() {
        let mut params = DynamicParams::<3>::new();
        params.offset_ids(100);
        let ids: Vec<u32> = params.slots().map(|s| params.id(s)).collect();
        assert_eq!(ids, [100, 101, 102]);
        assert_eq!(params.slot_for_id(102), params.slot(2));
        assert_eq!(params.slot_for_id(99), None);
        assert_eq!(params.slot_for_id(103), None);
    }

    #[test]
    fn state_round_trips_bindings_and_values() {
        let a = DynamicParams::<3>::new();
        let choice = SlotDef::new("Mode", SlotRange::Choice(vec!["Up".into(), "Down".into()]))
            .with_function(ParamFunction::Randomize);
        a.edit(|tx| {
            tx.bind(Slot(0), cutoff());
            tx.bind(Slot(2), choice.clone());
        });
        a.set_plain(a.id(Slot(0)), 440.0);
        a.set_plain(a.id(Slot(2)), 1.0);
        let (ids, values) = a.collect_values();
        let values: Vec<(u32, f64)> = ids.into_iter().zip(values).collect();
        let blob = a.serialize_persist();

        let b = DynamicParams::<3>::new();
        b.restore_state(&values, &blob);
        assert_eq!(b.def(Slot(2)), Some(choice));
        assert!(
            (b.plain(Slot(0)) - 440.0).abs() < 1e-6,
            "value restored in the restored range"
        );
        assert_eq!(b.format_value(b.id(Slot(2)), 1.0).as_deref(), Some("Down"));
        assert_eq!(b.parse_value(b.id(Slot(2)), "down"), Some(1.0));
        assert_eq!(
            b.parameter_function(b.id(Slot(2))),
            Some(ParamFunction::Randomize)
        );

        assert!(
            persist::decode(&blob, 4).is_none(),
            "wrong slot count is rejected"
        );
        assert!(
            persist::decode(&blob[..blob.len() - 1], 3).is_none(),
            "truncated is rejected"
        );
    }

    #[test]
    fn shape_reads_never_tear() {
        let cell = Arc::new(ShapeCell::new(Shape {
            tag: Tag::Linear,
            a: 0.0,
            b: 0.0,
        }));
        let writer = {
            let cell = Arc::clone(&cell);
            std::thread::spawn(move || {
                for i in 0..20_000u32 {
                    let v = f64::from(i);
                    cell.store(Shape {
                        tag: Tag::Linear,
                        a: v,
                        b: v,
                    });
                }
            })
        };
        for _ in 0..20_000 {
            let s = cell.load();
            assert_eq!(s.a.to_bits(), s.b.to_bits(), "torn read {s:?}");
        }
        writer.join().expect("writer");
    }
}
