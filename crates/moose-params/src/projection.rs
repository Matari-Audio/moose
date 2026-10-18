//! Immutable host views backed by a plugin's existing canonical storage.
use crate::{AUTO_PARAM_ID_MASK, PARAM_ID_MAX, ParamInfo};

/// Both results of one committed write, after clamping and stepping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProjectedValue {
    pub plain: f64,
    pub normalized: f64,
}

/// Opt-in appended host parameters: `#[params(projection = "MyViews")]`.
///
/// Metadata is process-shared and immutable. Every ID must be an absolute ID
/// above the historical 24-bit nested-ID domain and at most `PARAM_ID_MAX`.
/// Descriptors are appended after all declared/nested parameters; structural
/// edits change presentation, never this list. Use distinct IDs when a range,
/// unit or stepped/enum meaning differs. Reusing a provider in two nested
/// children is a duplicate-ID error, not an implicit retargeting operation.
///
/// Reads/writes must be bounded, allocation-free and safe under concurrent
/// calls. Return `None` for unknown IDs. Writes use canonical setters (including
/// their edit epochs), and return both post-quantization values without another
/// dispatch. A hidden/incompatible view may ignore a write and return its current
/// value. Text methods run off audio and may allocate.
///
/// Views are deliberately omitted from `collect_values`: aliases must not round
/// or overwrite a legacy canonical value when old projects are recalled. Store
/// any additional canonical arena through the owner's ordinary persisted fields.
/// Attach a provider to the outermost store to preserve the complete existing
/// host-list prefix. A provider on a child appends inside that child's metadata
/// span and therefore must not be retrofitted into an ordinal nested layout.
/// Nested/pre-save/state migration behavior remains owned by `Params`.
pub trait ParamProjection<P: ?Sized> {
    fn param_infos() -> &'static [ParamInfo];
    fn get_normalized(params: &P, id: u32) -> Option<f64>;
    fn get_plain(params: &P, id: u32) -> Option<f64>;
    fn set_normalized(params: &P, id: u32, value: f64) -> Option<ProjectedValue>;
    fn set_plain(params: &P, id: u32, value: f64) -> Option<ProjectedValue>;
    fn format_value(_params: &P, _id: u32, _value: f64) -> Option<String> {
        None
    }
    fn parse_value(_params: &P, _id: u32, _text: &str) -> Option<f64> {
        None
    }
}

/// Derive support; descriptor enumeration is host-thread work.
#[doc(hidden)]
pub fn append_projected_infos<P: ?Sized, V: ParamProjection<P>>(into: &mut Vec<ParamInfo>) {
    let infos = V::param_infos();
    for info in infos {
        assert!(
            (AUTO_PARAM_ID_MASK + 1..=PARAM_ID_MAX).contains(&info.id),
            "projected parameter ID {} must be in the absolute host-ID domain",
            info.id
        );
    }
    into.extend_from_slice(infos);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParamFlags, ParamRange, ParamUnit, ParamValueKind};
    struct Invalid;
    impl ParamProjection<()> for Invalid {
        fn param_infos() -> &'static [ParamInfo] {
            static INFOS: [ParamInfo; 1] = [ParamInfo {
                id: 1,
                name: "Invalid view",
                short_name: "Invalid",
                group: "",
                range: ParamRange::Linear { min: 0.0, max: 1.0 },
                default_plain: 0.0,
                flags: ParamFlags::AUTOMATABLE,
                unit: ParamUnit::None,
                kind: ParamValueKind::Float,
                midi_map: None,
                midi_channel: None,
            }];
            &INFOS
        }
        fn get_plain(_: &(), _: u32) -> Option<f64> {
            None
        }
        fn get_normalized(_: &(), _: u32) -> Option<f64> {
            None
        }
        fn set_plain(_: &(), _: u32, _: f64) -> Option<ProjectedValue> {
            None
        }
        fn set_normalized(_: &(), _: u32, _: f64) -> Option<ProjectedValue> {
            None
        }
    }
    #[test]
    #[should_panic(expected = "must be in the absolute host-ID domain")]
    fn rejects_rebasable_projection_ids() {
        append_projected_infos::<(), Invalid>(&mut vec![]);
    }
}
