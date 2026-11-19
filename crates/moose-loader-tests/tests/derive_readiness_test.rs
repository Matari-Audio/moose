//! Regression coverage for generated identifiers and validated defaults.
#![allow(non_camel_case_types, clippy::float_cmp)]
use moose::prelude::*;

// An input path may resolve differently at the call site. The emitted value
// must be the canonical constant the derive validated.
mod std {
    pub mod f64 {
        pub mod consts {
            pub const FRAC_1_SQRT_2: f64 = 2.0;
        }
    }
}

#[derive(Params)]
struct r#type {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", default = std::f64::consts::FRAC_1_SQRT_2)]
    gain: FloatParam,
    #[param(id = 1, name = "Self", default = 0.5)]
    self_: FloatParam,
    #[param(id = 2, name = "Band", default = 0.5)]
    _3band: FloatParam,
}

#[derive(Params)]
struct Child {
    #[param(id = 0, name = "Child", default = 0.5)]
    gain: FloatParam,
}

#[derive(Params)]
#[params(id_scheme = "ordinal")]
struct SparseParent {
    #[param(id = 1, name = "Parent", default = 0.5)]
    gain: FloatParam,
    #[nested(base = 2)]
    child: Child,
}

#[test]
fn identifiers_and_defaults_compile_and_agree_with_static_metadata() {
    let params = r#type::new();
    assert_eq!(typeParamId::Gain as u32, 0);
    assert_eq!(typeParamId::Self_ as u32, 1);
    assert_eq!(typeParamId::_3band as u32, 2);
    let infos = r#type::param_infos_static();
    assert_eq!(infos[0].default_plain, ::core::f64::consts::FRAC_1_SQRT_2);
    assert_eq!(
        params.get_plain(0).unwrap(),
        ::core::f64::consts::FRAC_1_SQRT_2
    );
    assert_eq!(std::f64::consts::FRAC_1_SQRT_2, 2.0);
}

#[test]
fn sparse_parent_with_explicit_nested_base_has_unique_ids() {
    let _params = SparseParent::new();
    let ids: Vec<_> = SparseParent::param_infos_static()
        .iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(ids, vec![1, 2]);
}
