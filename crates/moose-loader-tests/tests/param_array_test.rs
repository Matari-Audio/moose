//! Behavioral tests for param arrays: a `[FloatParam; N]` (or any param
//! type) field is N host parameters, each with its own id and name.
//!
//! - An explicit `id = N` is element 0's; element `i` takes `N + i`.
//! - Without one, each element's id is the hash of `field[i]` (default
//!   scheme) or the next ordinal (legacy scheme).
//! - `name` / `short_name` are templates: `{i}` is the index from 0,
//!   `{n}` the number from 1, `{n:03}` zero-padded.

#![allow(clippy::float_cmp)]

use moose_derive::Params;
use moose_params::{BoolParam, FloatParam, FloatParamReadF32, Params};

#[derive(Params)]
struct Automation {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = 0.5)]
    gain: FloatParam,
    #[param(
        id = 100,
        name = "#{i:03}",
        short_name = "A{n}",
        range = "linear(0, 1)",
        default = 0.25
    )]
    slots: [FloatParam; 4],
    #[param(id = 200, name = "Switch {n}")]
    switches: [BoolParam; 2],
}

#[test]
fn an_array_is_one_param_per_element_with_its_own_id_and_name() {
    let a = Automation::new();
    assert_eq!(a.count(), 1 + 4 + 2);
    let infos = a.param_infos();
    let shown: Vec<(u32, &str, &str)> =
        infos.iter().map(|p| (p.id, p.name, p.short_name)).collect();
    assert_eq!(
        shown,
        vec![
            (0, "Gain", "Gain"),
            (100, "#000", "A1"),
            (101, "#001", "A2"),
            (102, "#002", "A3"),
            (103, "#003", "A4"),
            (200, "Switch 1", "Switch 1"),
            (201, "Switch 2", "Switch 2"),
        ]
    );
    // Registration reads the same table without an instance.
    let statics: Vec<(u32, &str)> = Automation::param_infos_static()
        .iter()
        .map(|p| (p.id, p.name))
        .collect();
    let runtime: Vec<(u32, &str)> = infos.iter().map(|p| (p.id, p.name)).collect();
    assert_eq!(statics, runtime);
}

#[test]
fn each_element_holds_its_own_value() {
    let a = Automation::new();
    assert_eq!(a.get_plain(102), Some(0.25));
    a.set_plain(102, 0.75);
    a.set_normalized(103, 1.0);
    assert_eq!(a.slots[2].value(), 0.75);
    assert_eq!(a.slots[3].value(), 1.0);
    assert_eq!(a.get_plain(101), Some(0.25), "its neighbours keep theirs");
    a.set_plain(201, 1.0);
    assert!(a.switches[1].value() && !a.switches[0].value());
    assert!(a.format_value(100, 0.5).is_some());
}

#[test]
fn an_arrays_values_round_trip_through_the_saved_values() {
    let a = Automation::new();
    a.set_plain(101, 0.9);
    let (ids, values) = a.collect_values();
    assert_eq!(ids.len(), 7);
    let b = Automation::new();
    let pairs: Vec<(u32, f64)> = ids.into_iter().zip(values).collect();
    b.restore_values(&pairs);
    assert!((b.slots[1].value() - 0.9).abs() < 1e-6);
}

#[test]
fn the_id_enum_names_each_element() {
    assert_eq!(AutomationParamId::Slots0.as_u32(), 100);
    assert_eq!(AutomationParamId::Slots3.as_u32(), 103);
    assert_eq!(
        AutomationParamId::from_u32(201),
        Some(AutomationParamId::Switches1)
    );
}

// Unpinned: each element's id hashes `field[i]`, so they are apart and
// stay put when the array grows.
#[derive(Params)]
struct Hashed {
    #[param(name = "Macro {n}", range = "linear(0, 1)")]
    macros: [FloatParam; 3],
}

#[derive(Params)]
struct HashedLonger {
    #[param(name = "Macro {n}", range = "linear(0, 1)")]
    macros: [FloatParam; 5],
}

#[test]
fn unpinned_elements_hash_their_index_and_keep_it_as_the_array_grows() {
    let ids = |infos: Vec<moose_params::ParamInfo>| infos.iter().map(|p| p.id).collect::<Vec<_>>();
    let three = ids(Hashed::new().param_infos());
    let five = ids(HashedLonger::new().param_infos());
    assert_eq!(three.len(), 3);
    assert_eq!(&five[..3], &three[..]);
    let mut apart = five.clone();
    apart.sort_unstable();
    apart.dedup();
    assert_eq!(apart.len(), 5);
}

// An array inside a nested group rebases with it.
#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct Bank {
    #[param(name = "Knob {n}", range = "linear(0, 10)", default = 1.0)]
    knobs: [FloatParam; 3],
}

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct Rack {
    #[param(id = 0, name = "Level", range = "linear(0, 1)")]
    level: FloatParam,
    #[nested(base = 10)]
    bank: Bank,
}

#[test]
fn an_array_in_a_nested_group_rebases_with_it() {
    let r = Rack::new();
    let ids: Vec<u32> = r.param_infos().iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![0, 10, 11, 12]);
    r.set_plain(12, 7.0);
    assert_eq!(r.bank.knobs[2].value(), 7.0);
    let statics: Vec<u32> = Rack::param_infos_static().iter().map(|p| p.id).collect();
    assert_eq!(statics, ids);
}
