//! A runtime slot pool nested in a derived parameter tree: ids rebase,
//! presentation and function come from the pool, catalog changes reach the
//! root's signal, and the bindings ride along in the persisted state.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moose_derive::Params;
use moose_params::{
    CatalogChange, CatalogSignal, DynamicParams, FloatParam, ParamFunction, Params, SlotDef,
    SlotRange,
};

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct Modular {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = 0.5)]
    gain: FloatParam,
    #[nested(base = 1)]
    slots: DynamicParams<4>,
}

#[test]
fn nested_pool_is_a_live_part_of_the_tree() {
    let p = Modular::new();
    assert_eq!(p.count(), 5);
    let ids: Vec<u32> = p.param_infos().iter().map(|i| i.id).collect();
    assert_eq!(ids, [0, 1, 2, 3, 4]);

    let wakes = Arc::new(AtomicUsize::new(0));
    let signal = Arc::new(CatalogSignal::new({
        let wakes = Arc::clone(&wakes);
        move || {
            wakes.fetch_add(1, Ordering::Relaxed);
        }
    }));
    p.set_catalog_signal(Arc::downgrade(&signal));

    let mix = p.slots.slot(2).expect("slot 2");
    p.slots.edit(|tx| {
        tx.bind(
            mix,
            SlotDef::new(
                "Mix",
                SlotRange::Linear {
                    min: 0.0,
                    max: 100.0,
                },
            )
            .with_default(100.0)
            .with_function(ParamFunction::DryWetMix),
        );
    });
    assert_eq!(wakes.load(Ordering::Relaxed), 1, "one wake per transaction");
    let id = p.slots.id(mix);
    assert_eq!(id, 3);
    assert!(
        p.take_catalog_changes()
            .flags
            .contains(CatalogChange::STRUCTURE)
    );
    assert_eq!(p.parameter_presentation(id).expect("bound").name, "Mix");
    assert_eq!(p.parameter_function(id), Some(ParamFunction::DryWetMix));
    assert_eq!(p.parameter_function(0), None);
    assert_eq!(p.get_plain(id), Some(100.0));

    p.set_plain(id, 25.0);
    let (ids, values) = p.collect_values();
    let values: Vec<(u32, f64)> = ids.into_iter().zip(values).collect();
    let persist = p.serialize_persist();

    let q = Modular::new();
    q.restore_state(&values, &persist);
    assert_eq!(q.parameter_presentation(id).expect("restored").name, "Mix");
    assert_eq!(q.get_plain(id), Some(25.0));
}
