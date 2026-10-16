//! Integration-test host for `moose-loader`.
//!
//! This crate has no library surface. It exists only so the tests
//! under `tests/` can depend on the umbrella `moose` crate (needed
//! by `#[derive(Params)]` / `#[derive(State)]` expansion, which
//! emits `::moose::params::*` and `::moose::core::*` paths) without
//! the back-edge into `moose-loader` becoming a `moose <->
//! moose-loader` cycle in cargo metadata.
//!
//! See `crates/moose-loader/Cargo.toml` for the rationale.

#[cfg(test)]
mod persist_fingerprint_tests;

#[cfg(test)]
mod instance_edit_epoch_tests {
    use moose::prelude::*;

    #[derive(Params)]
    struct Child {
        #[param(id = 0, name = "Value", range = "linear(0, 1)", default = 0.0)]
        value: FloatParam,
    }

    #[derive(Params)]
    struct Parent {
        #[param(id = 0, name = "Value", range = "linear(0, 1)", default = 0.0)]
        value: FloatParam,
        #[nested(base = 1)]
        first: Child,
        #[nested(base = 2)]
        second: Child,
    }

    #[test]
    fn nested_setters_only_advance_the_owning_instance() {
        let a = Parent::default();
        let b = Parent::default();
        assert_eq!(a.parameter_edit_epoch(), 0);
        a.value.set_value(0.5);
        a.first.value.set_value(0.25);
        a.set_normalized(2, 0.75);
        assert_eq!(a.parameter_edit_epoch(), 3);
        assert_eq!(a.first.parameter_edit_epoch(), 3);
        assert_eq!(a.second.parameter_edit_epoch(), 3);
        assert_eq!(b.parameter_edit_epoch(), 0);
        a.set_normalized(2, 0.75);
        assert_eq!(
            a.parameter_edit_epoch(),
            3,
            "an unchanged setter is not an edit"
        );
        b.first.value.set_value(0.5);
        assert_eq!(b.parameter_edit_epoch(), 1);
        assert_eq!(a.parameter_edit_epoch(), 3);
        let custom = moose::params::custom_edit_epoch();
        moose::params::bump_edit_epoch();
        assert!(moose::params::custom_edit_epoch() > custom);
        assert_eq!(
            a.parameter_edit_epoch(),
            3,
            "custom state has a separate counter"
        );
    }
}

#[cfg(test)]
mod parameter_projection_tests;
