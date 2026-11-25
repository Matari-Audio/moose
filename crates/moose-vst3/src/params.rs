//! Parameter accessors, presentation, formatting and parsing on [`Vst3Instance`].

use crate::backend::ParamPresentation;
use crate::instance::Vst3Instance;
use crate::midi_proxy::allocated_midi_proxy_index;

use moose_core::export::PluginExport;
use moose_core::wrapper::run_extern_callback_with;
use moose_params::{ParamFlags, Params};
use std::sync::atomic::Ordering;

/// Parse host text entry for a MIDI-CC proxy param: a plain number,
/// clamped to the proxy's normalized `0..=1` domain.
pub(crate) fn parse_midi_proxy_text(text: &str) -> Option<f64> {
    let v = text.trim().parse::<f64>().ok()?;
    v.is_finite().then(|| v.clamp(0.0, 1.0))
}

impl<P: PluginExport> Vst3Instance<P> {
    pub(crate) fn param_get_value_impl(&self, id: u32) -> f64 {
        if let Some(index) = allocated_midi_proxy_index(&self.midi_proxy_ids, id)
            && let Some(slot) = self.midi_proxy_values.get(index)
        {
            return f64::from_bits(slot.load(Ordering::Relaxed));
        }
        match self.param_index_of(id) {
            Some(index) => self.params_arc.get_plain_by_index(index, id),
            None => self.params_arc.get_plain(id),
        }
        .unwrap_or(0.0)
    }

    pub(crate) fn param_set_value_impl(&self, id: u32, value: f64) {
        if let Some(index) = allocated_midi_proxy_index(&self.midi_proxy_ids, id)
            && let Some(slot) = self.midi_proxy_values.get(index)
        {
            slot.store(value.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
            return;
        }
        match self.param_index_of(id) {
            Some(index) => self.params_arc.set_plain_by_index(index, id, value),
            None => self.params_arc.set_plain(id, value),
        }
    }

    pub(crate) fn param_presentation_impl(&self, id: u32) -> Option<ParamPresentation> {
        run_extern_callback_with::<P, Option<ParamPresentation>>(
            "vst3",
            "parameter_presentation",
            None,
            || {
                let p = self.params_arc.parameter_presentation(id)?;
                Some(ParamPresentation {
                    name: p.name,
                    hidden: p.hidden,
                    unavailable: !p.available,
                })
            },
        )
    }

    pub(crate) fn param_presentation_revision_impl(&self) -> u64 {
        run_extern_callback_with::<P, u64>("vst3", "parameter_presentation_revision", 0, || {
            self.params_arc.parameter_presentation_revision()
        })
    }

    pub(crate) fn param_is_chunked_impl(&self, id: u32) -> bool {
        let info = self
            .param_index_of(id)
            .and_then(|idx| self.param_infos.by_index(idx))
            .or_else(|| self.param_infos.by_id(id));
        info.is_some_and(|info| info.flags.contains(ParamFlags::CHUNKED))
    }

    pub(crate) fn param_normalize_impl(&self, id: u32, plain: f64) -> f64 {
        if let Some(range) = self.params_arc.live_param_range(id) {
            return range.normalize(plain);
        }
        match self.param_ranges.binary_search_by_key(&id, |(i, _)| *i) {
            Ok(idx) => self.param_ranges[idx].1.normalize(plain),
            Err(_) => plain,
        }
    }

    pub(crate) fn param_denormalize_impl(&self, id: u32, normalized: f64) -> f64 {
        if let Some(range) = self.params_arc.live_param_range(id) {
            return range.denormalize(normalized);
        }
        match self.param_ranges.binary_search_by_key(&id, |(i, _)| *i) {
            Ok(idx) => self.param_ranges[idx].1.denormalize(normalized),
            Err(_) => normalized,
        }
    }

    #[allow(clippy::cast_possible_wrap)]
    pub(crate) fn param_step_count_impl(&self, id: u32) -> Option<(i32, Option<f64>)> {
        let range = self.params_arc.live_param_range(id)?;
        let default_norm = self
            .param_infos
            .by_id(id)
            .map(|info| range.normalize(info.default_plain));
        Some((
            range.step_count().map_or(0, |n| n.get() as i32),
            default_norm,
        ))
    }

    pub(crate) fn param_format_impl(&self, id: u32, value: f64) -> Option<String> {
        run_extern_callback_with::<P, Option<String>>("vst3", "format_value", None, || {
            match self.param_index_of(id) {
                Some(index) => self.params_arc.format_value_by_index(index, id, value),
                None => self.params_arc.format_value(id, value),
            }
        })
    }

    pub(crate) fn param_parse_impl(&self, id: u32, text: &str) -> Option<f64> {
        run_extern_callback_with::<P, Option<f64>>("vst3", "parse_value", None, || {
            if allocated_midi_proxy_index(&self.midi_proxy_ids, id).is_some() {
                return parse_midi_proxy_text(text);
            }
            match self.param_index_of(id) {
                Some(index) => self.params_arc.parse_value_by_index(index, id, text),
                None => self.params_arc.parse_value(id, text),
            }
        })
    }
}

#[cfg(test)]
mod dense_index_tests {
    use super::*;
    use moose_test_fixtures::host_value_fixture::TestPlugin;

    #[test]
    fn vst3_callbacks_resolve_params_via_sorted_dense_index_and_fallback() {
        let inst =
            Vst3Instance::<TestPlugin>::create(crate::lifecycle::vst3_test_host()).expect("create");
        let param_id = inst.param_infos[0].id;
        assert_eq!(inst.param_index_of(param_id), Some(0));
        assert_eq!(inst.param_index_of(999_999), None);

        inst.param_set_value_impl(param_id, 0.5);
        let got = inst.param_get_value_impl(param_id);
        assert!((got - 0.5).abs() < 1e-6, "got {got}");
        // Unknown ID degrades cleanly without panicking.
        assert!(inst.param_get_value_impl(999_999).abs() < 1e-12);
        let formatted = inst
            .param_format_impl(param_id, 0.5)
            .expect("formatted text");
        assert_ne!(formatted, "");
        let parsed = inst
            .param_parse_impl(param_id, &formatted)
            .expect("parsed text");
        assert!((parsed - 0.5).abs() < 1e-2, "parsed {parsed}");
    }
}
