//! Immutable metadata and ID lookup shared by format adapters.
use crate::ParamInfo;
use std::{collections::HashMap, ops::Deref};

/// Declaration-order metadata plus an indexed ID lookup. Construct off audio;
/// lookups neither allocate nor walk unrelated descriptors.
pub struct ParamCatalog {
    infos: Box<[ParamInfo]>,
    indices: HashMap<u32, usize>,
}

impl ParamCatalog {
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
        let catalog = ParamCatalog::new(vec![info(97), info(0x1000000), info(3)]);
        assert_eq!(
            catalog.iter().map(|info| info.id).collect::<Vec<_>>(),
            [97, 0x1000000, 3]
        );
        for id in [3, 97, 0x1000000] {
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
