//! D6 dense index lookup: `*_by_index` agrees with the by-id path for every
//! catalog entry, and a stale, foreign or out-of-range index (a runtime-added
//! dynamic slot, a projected view, a cookie from an older catalog) falls back
//! to the by-id lookup instead of touching the wrong parameter.

use moose::params::{
    DynamicParams, ParamFlags, ParamInfo, ParamProjection, ParamRange, ParamUnit, ParamValueKind,
    ProjectedValue, SlotDef, SlotRange,
};
use moose::prelude::*;

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct Tree {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = 0.5)]
    gain: FloatParam,
    #[param(id = 1, name = "Steps", range = "discrete(0, 8)", default = 3)]
    steps: IntParam,
    #[param(id = 2, name = "Bank")]
    bank: ValueListParam,
    #[nested(base = 3)]
    slots: DynamicParams<3>,
}

fn assert_by_index_matches_by_id(p: &Tree) {
    let catalog = p.param_catalog();
    for info in p.param_infos() {
        let index = catalog.index_of(info.id).expect("catalog index");
        assert_eq!(
            p.get_plain_by_index(index, info.id),
            p.get_plain(info.id),
            "get id {}",
            info.id
        );
        let plain = p.get_plain(info.id).expect("value");
        let text = p.format_value(info.id, plain);
        assert_eq!(p.format_value_by_index(index, info.id, plain), text);
        if let Some(text) = text {
            assert_eq!(
                p.parse_value_by_index(index, info.id, &text),
                p.parse_value(info.id, &text),
                "parse id {}",
                info.id
            );
        }
    }
}

#[test]
fn every_declared_nested_and_value_list_entry_resolves_by_index() {
    let p = Tree::new();
    assert!(p.bank.set_labels(["Kit A", "Kit B", "Kit C"]));
    p.bank.set_index(2);
    p.steps.set_value(7);
    assert_by_index_matches_by_id(&p);

    let bank = p.param_catalog().index_of(p.bank.id()).expect("bank");
    assert_eq!(p.get_plain_by_index(bank, p.bank.id()), Some(2.0));
    assert_eq!(
        p.format_value_by_index(bank, p.bank.id(), 1.0).as_deref(),
        Some("Kit B")
    );
    p.set_plain_by_index(bank, p.bank.id(), 0.0);
    assert_eq!(p.bank.index(), 0);
}

#[test]
fn runtime_added_dynamic_entry_hits_the_id_fallback() {
    let p = Tree::new();
    // Bind a slot at runtime: an entry the derive could not see at compile time.
    let slot = p.slots.slot(1).expect("slot 1");
    p.slots.edit(|tx| {
        tx.bind(
            slot,
            SlotDef::new(
                "Mix",
                SlotRange::Linear {
                    min: 0.0,
                    max: 100.0,
                },
            )
            .with_default(40.0),
        );
    });
    let id = p.slots.id(slot);
    let catalog = p.param_catalog();
    let index = catalog
        .index_of(id)
        .expect("runtime slot is in the catalog");
    assert_eq!(index, 4, "3 own fields, then the pool's slots in order");
    assert_by_index_matches_by_id(&p);

    // The correct dense index reaches the slot through the nested recursion.
    assert_eq!(p.get_plain_by_index(index, id), Some(40.0));

    // A stale index pointing at a declared field (gain at 0) must not answer
    // for the slot: the id guard rejects it and the by-id path wins.
    assert_eq!(p.get_plain_by_index(0, id), Some(40.0));
    p.set_plain_by_index(0, id, 75.0);
    assert_eq!(p.get_plain(id), Some(75.0));
    assert!(
        (p.gain.raw_target() - 0.5).abs() < 1e-12,
        "gain untouched by a stale index"
    );

    // A stale index inside the pool (another slot) and an out-of-range one
    // (a cookie from a larger, older catalog) fall back the same way.
    assert_eq!(p.get_plain_by_index(3, id), Some(75.0));
    assert_eq!(p.get_plain_by_index(999, id), Some(75.0));
    p.set_plain_by_index(999, id, 10.0);
    assert_eq!(p.get_plain(id), Some(10.0));
    assert_eq!(
        p.format_value_by_index(0, id, 10.0),
        p.format_value(id, 10.0)
    );
    assert_eq!(p.parse_value_by_index(999, id, "12"), Some(12.0));

    // Unknown ids stay unknown whichever index the host hands in.
    assert_eq!(p.get_plain_by_index(0, 0x00ff_0000), None);
    assert_eq!(p.get_plain_by_index(999, 0x00ff_0000), None);
}

const VIEW: u32 = 0x0100_0000;
const VIEW_RANGE: ParamRange = ParamRange::Discrete { min: 1, max: 64 };
static VIEW_INFOS: [ParamInfo; 1] = [ParamInfo {
    id: VIEW,
    name: "Voices",
    short_name: "Voices",
    group: "",
    range: VIEW_RANGE,
    default_plain: 1.0,
    flags: ParamFlags::AUTOMATABLE,
    unit: ParamUnit::None,
    kind: ParamValueKind::Int,
    midi_map: None,
    midi_channel: None,
}];

#[derive(Params)]
#[params(projection = "Views")]
struct Projected {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = 0.5)]
    gain: FloatParam,
    #[param(id = 1, name = "Spread", range = "linear(0, 1)", default = 0.0)]
    spread: FloatParam,
}

struct Views;
impl ParamProjection<Projected> for Views {
    fn param_infos() -> &'static [ParamInfo] {
        &VIEW_INFOS
    }
    fn get_plain(params: &Projected, id: u32) -> Option<f64> {
        (id == VIEW).then(|| VIEW_RANGE.denormalize(params.spread.raw_target()))
    }
    fn get_normalized(params: &Projected, id: u32) -> Option<f64> {
        Self::get_plain(params, id).map(|plain| VIEW_RANGE.normalize(plain))
    }
    fn set_normalized(params: &Projected, id: u32, value: f64) -> Option<ProjectedValue> {
        if id != VIEW || !value.is_finite() {
            return None;
        }
        let plain = VIEW_RANGE.denormalize(value.clamp(0.0, 1.0));
        let normalized = VIEW_RANGE.normalize(plain);
        params.spread.set_value(normalized);
        Some(ProjectedValue { plain, normalized })
    }
    fn set_plain(params: &Projected, id: u32, value: f64) -> Option<ProjectedValue> {
        Self::set_normalized(params, id, VIEW_RANGE.normalize(value))
    }
    fn format_value(_: &Projected, id: u32, value: f64) -> Option<String> {
        (id == VIEW).then(|| format!("{} voices", value.round()))
    }
    fn parse_value(_: &Projected, id: u32, text: &str) -> Option<f64> {
        if id != VIEW {
            return None;
        }
        text.strip_suffix(" voices")?.parse().ok()
    }
}

#[test]
fn projected_entry_resolves_by_index_and_never_aliases_a_declared_field() {
    let p = Projected::default();
    let catalog = p.param_catalog();
    let index = catalog.index_of(VIEW).expect("projected entry indexed");
    assert_eq!(index, 2, "projections append after the declared prefix");

    p.set_plain_by_index(index, VIEW, 33.0);
    assert_eq!(p.get_plain_by_index(index, VIEW), Some(33.0));
    assert_eq!(p.get_plain(VIEW), Some(33.0));
    assert_eq!(
        p.format_value_by_index(index, VIEW, 33.0).as_deref(),
        Some("33 voices")
    );
    assert_eq!(p.parse_value_by_index(index, VIEW, "8 voices"), Some(8.0));

    // A stale index naming the gain slot reaches the projection, not gain.
    p.set_plain_by_index(0, VIEW, 9.0);
    assert_eq!(p.get_plain_by_index(0, VIEW), Some(9.0));
    assert!((p.gain.raw_target() - 0.5).abs() < 1e-12);
}
