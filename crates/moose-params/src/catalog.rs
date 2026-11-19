//! Immutable metadata and ID lookup shared by format adapters, and the
//! change notices a runtime parameter set sends them.
use crate::ParamInfo;
use std::{collections::HashMap, ops::Deref};

/// Wakes a wrapper after a parameter-set change. Its wake must only
/// schedule a host main-thread callback (CLAP `request_callback`, the VST3
/// UI-thread wake); the callback then calls
/// [`crate::Params::take_catalog_changes`].
pub type CatalogSignal = crate::PersistChangeSignal;

bitflags::bitflags! {
    /// What changed in a parameter set since the wrapper last asked.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct CatalogChange: u8 {
        /// Values changed outside host automation (CLAP `RESCAN_VALUES`,
        /// VST3 `kParamValuesChanged`).
        const VALUES = 0b001;
        /// Names, groups or display text changed (CLAP `RESCAN_INFO |
        /// RESCAN_TEXT`, VST3 `kParamTitlesChanged`).
        const INFO = 0b010;
        /// Parameters appeared, disappeared or changed range, kind or
        /// function (CLAP restart + `RESCAN_ALL`, VST3 `kReloadComponent`).
        const STRUCTURE = 0b100;
    }
}

/// The changes taken from a parameter set in one main-thread callback.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[must_use]
pub struct CatalogChanges {
    pub flags: CatalogChange,
    /// Host ids that were removed or retyped. CLAP clears their automation
    /// and modulation (`params.clear(id, CLAP_PARAM_CLEAR_ALL)`).
    pub removed: Vec<u32>,
}

impl CatalogChanges {
    /// Fold `other` into `self`.
    pub fn merge(&mut self, other: Self) {
        self.flags |= other.flags;
        self.removed.extend(other.removed);
    }
}

/// Declaration-order metadata plus an indexed ID lookup. Construct off audio;
/// lookups neither allocate nor walk unrelated descriptors.
pub struct ParamCatalog {
    infos: Box<[ParamInfo]>,
    indices: HashMap<u32, usize>,
}

impl ParamCatalog {
    /// Build an immutable parameter catalog.
    ///
    /// # Panics
    /// Panics if two descriptors have the same parameter ID.
    #[must_use]
    pub fn new(infos: Vec<ParamInfo>) -> Self {
        let mut indices = HashMap::with_capacity(infos.len());
        for (index, info) in infos.iter().enumerate() {
            if let Some(previous) = indices.insert(info.id, index) {
                panic!(
                    "duplicate parameter ID {}: '{}' and '{}'",
                    info.id, infos[previous].name, info.name
                );
            }
        }
        Self {
            infos: infos.into_boxed_slice(),
            indices,
        }
    }

    #[must_use]
    pub fn by_id(&self, id: u32) -> Option<&ParamInfo> {
        self.indices.get(&id).map(|&index| &self.infos[index])
    }
}

impl Deref for ParamCatalog {
    type Target = [ParamInfo];
    fn deref(&self) -> &Self::Target {
        &self.infos
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParamFlags, ParamRange, ParamUnit, ParamValueKind};
    fn info(id: u32) -> ParamInfo {
        ParamInfo {
            id,
            name: "Value",
            short_name: "Value",
            group: "Module",
            range: ParamRange::Linear { min: 0.0, max: 1.0 },
            default_plain: 0.0,
            flags: ParamFlags::AUTOMATABLE,
            unit: ParamUnit::None,
            kind: ParamValueKind::Float,
            midi_map: None,
            midi_channel: None,
        }
    }
    #[test]
    fn lookup_preserves_unsorted_declaration_order_and_wide_ids() {
        let catalog = ParamCatalog::new(vec![info(97), info(0x0100_0000), info(3)]);
        assert_eq!(
            catalog.iter().map(|info| info.id).collect::<Vec<_>>(),
            [97, 0x0100_0000, 3]
        );
        for id in [3, 97, 0x0100_0000] {
            assert_eq!(catalog.by_id(id).unwrap().id, id);
        }
        assert!(catalog.by_id(u32::MAX).is_none());
    }
    #[test]
    #[should_panic(expected = "duplicate parameter ID 97")]
    fn duplicate_metadata_cannot_enter_the_index() {
        let _ = ParamCatalog::new(vec![info(97), info(97)]);
    }
}
