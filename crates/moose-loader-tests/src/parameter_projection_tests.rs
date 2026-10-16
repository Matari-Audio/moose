use moose::params::{
    ParamFlags, ParamInfo, ParamProjection, ParamRange, ParamUnit, ParamValueKind, ProjectedValue,
};
use moose::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};

const VIEW: u32 = 0x0100_0000;
const RANGE: ParamRange = ParamRange::Discrete { min: 1, max: 64 };
static INFOS: [ParamInfo; 1] = [ParamInfo {
    id: VIEW,
    name: "Voices",
    short_name: "Voices",
    group: "Oscillator 1/Unison",
    range: RANGE,
    default_plain: 1.0,
    flags: ParamFlags::AUTOMATABLE,
    unit: ParamUnit::None,
    kind: ParamValueKind::Int,
    midi_map: None,
    midi_channel: None,
}];

#[derive(Params)]
#[params(pre_save = "prepare")]
struct Child {
    #[param(
        id = 0,
        name = "Legacy fraction",
        range = "linear(0, 1)",
        default = 0.25
    )]
    value: FloatParam,
    #[skip]
    prepares: AtomicUsize,
}
impl Child {
    fn prepare(&self) {
        self.prepares.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Params)]
#[params(projection = "Views", pre_save = "prepare")]
struct Store {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = 0.5)]
    gain: FloatParam,
    #[nested(base = 1)]
    child: Child,
    #[skip]
    prepares: AtomicUsize,
    #[skip]
    view_writes: AtomicUsize,
}
impl Store {
    fn prepare(&self) {
        self.prepares.fetch_add(1, Ordering::Relaxed);
    }
}
struct Views;
impl ParamProjection<Store> for Views {
    fn param_infos() -> &'static [ParamInfo] {
        &INFOS
    }
    fn get_plain(params: &Store, id: u32) -> Option<f64> {
        (id == VIEW).then(|| RANGE.denormalize(params.child.value.raw_target()))
    }
    fn get_normalized(params: &Store, id: u32) -> Option<f64> {
        Self::get_plain(params, id).map(|plain| RANGE.normalize(plain))
    }
    fn set_normalized(params: &Store, id: u32, value: f64) -> Option<ProjectedValue> {
        if id != VIEW || !value.is_finite() {
            return None;
        }
        let plain = RANGE.denormalize(value.clamp(0.0, 1.0));
        let normalized = RANGE.normalize(plain);
        params.view_writes.fetch_add(1, Ordering::Relaxed);
        params.child.value.set_value(normalized);
        Some(ProjectedValue { plain, normalized })
    }
    fn set_plain(params: &Store, id: u32, value: f64) -> Option<ProjectedValue> {
        if !value.is_finite() {
            return None;
        }
        Self::set_normalized(params, id, RANGE.normalize(value))
    }
    fn format_value(_: &Store, id: u32, value: f64) -> Option<String> {
        (id == VIEW).then(|| format!("{} voices", value.round()))
    }
    fn parse_value(_: &Store, id: u32, text: &str) -> Option<f64> {
        if id != VIEW {
            return None;
        }
        text.strip_suffix(" voices")?.parse().ok()
    }
}

#[test]
fn metadata_appends_after_the_complete_declared_and_nested_prefix() {
    let params = Store::default();
    let dynamic = params.param_infos();
    let registered = Store::param_infos_static();
    assert_eq!(
        dynamic.iter().map(|p| p.id).collect::<Vec<_>>(),
        [0, 1, VIEW]
    );
    assert_eq!(
        registered.iter().map(|p| p.id).collect::<Vec<_>>(),
        [0, 1, VIEW]
    );
    assert_eq!(params.count(), 3);
    let mut appended = vec![];
    params.append_param_infos(&mut appended);
    assert_eq!(
        appended.iter().map(|p| p.id).collect::<Vec<_>>(),
        [0, 1, VIEW]
    );
    assert_eq!(dynamic[2].kind, ParamValueKind::Int);
    assert_eq!(dynamic[2].range.step_count_usize(), 63);
    assert_eq!(dynamic[2].group, "Oscillator 1/Unison");
}

#[test]
fn typed_writes_share_the_canonical_epoch_and_fused_setters_commit_once() {
    let params = Store::default();
    let other = Store::default();
    let plain = params.set_normalized_returning_plain(VIEW, 0.5);
    assert_eq!(plain, RANGE.denormalize(0.5));
    assert_eq!(params.view_writes.load(Ordering::Relaxed), 1);
    assert_eq!(params.child.value.raw_target(), RANGE.normalize(plain));
    assert_eq!(params.parameter_edit_epoch(), 1);
    assert_eq!(other.parameter_edit_epoch(), 0);
    let normalized = params.set_normalized_returning_normalized(VIEW, 0.9);
    assert_eq!(normalized, RANGE.normalize(RANGE.denormalize(0.9)));
    assert_eq!(params.view_writes.load(Ordering::Relaxed), 2);
    params.set_plain(VIEW, 64.0);
    assert_eq!(params.get_plain(VIEW), Some(64.0));
    assert_eq!(params.get_normalized(VIEW), Some(1.0));
    params.set_normalized(0, 0.75);
    assert_eq!(params.get_plain(0), Some(0.75));
    assert_eq!(params.view_writes.load(Ordering::Relaxed), 3);
}

#[test]
fn old_fractional_state_remains_exact_and_aliases_are_not_serialized_twice() {
    let params = Store::default();
    let fraction = 0.251_234_567_89;
    params.child.value.set_value(fraction);
    let (ids, values) = params.collect_values();
    assert_eq!(ids, [0, 1]);
    assert_eq!(values, [0.5, fraction]);
    assert_eq!(params.prepares.load(Ordering::Relaxed), 1);
    assert_eq!(params.child.prepares.load(Ordering::Relaxed), 1);
    let restored = Store::default();
    restored.restore_values(&ids.into_iter().zip(values).collect::<Vec<_>>());
    assert_eq!(
        restored.child.value.raw_target().to_bits(),
        fraction.to_bits()
    );
    assert_eq!(restored.view_writes.load(Ordering::Relaxed), 0);
    assert_eq!(restored.get_plain(VIEW), Some(RANGE.denormalize(fraction)));
}

#[test]
fn projection_text_and_unknown_ids_do_not_intercept_legacy_parameters() {
    let params = Store::default();
    assert_eq!(
        params.format_value(VIEW, 17.0).as_deref(),
        Some("17 voices")
    );
    assert_eq!(params.parse_value(VIEW, "17 voices"), Some(17.0));
    assert_eq!(params.parse_value(VIEW, "NaN voices"), None);
    assert_eq!(params.get_plain(VIEW + 1), None);
    params.set_plain(VIEW + 1, 1.0);
    params.set_normalized(VIEW, f64::NAN);
    assert_eq!(params.view_writes.load(Ordering::Relaxed), 0);
    assert!(params.format_value(0, 0.5).is_some());
    assert_eq!(params.parse_value(0, "0.5"), Some(0.5));
}

// Wider IDs stay absolute under nested layouts. A projection must not alias a
// declared endpoint even when the alias is invisible in the initial patch.
struct CollisionViews;
#[derive(Params)]
#[params(projection = "CollisionViews")]
struct CollisionStore {
    #[param(
        id = 16777216,
        name = "Existing endpoint",
        range = "linear(0, 1)",
        default = 0.0
    )]
    value: FloatParam,
}
impl ParamProjection<CollisionStore> for CollisionViews {
    fn param_infos() -> &'static [ParamInfo] {
        &INFOS
    }
    fn get_plain(_: &CollisionStore, _: u32) -> Option<f64> {
        None
    }
    fn get_normalized(_: &CollisionStore, _: u32) -> Option<f64> {
        None
    }
    fn set_plain(_: &CollisionStore, _: u32, _: f64) -> Option<ProjectedValue> {
        None
    }
    fn set_normalized(_: &CollisionStore, _: u32, _: f64) -> Option<ProjectedValue> {
        None
    }
}

#[test]
#[should_panic(expected = "duplicate parameter ID 16777216")]
fn projection_ids_cannot_replace_declared_endpoints() {
    let _ = CollisionStore::default();
}
