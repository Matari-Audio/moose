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

/// Why a parameter catalog could not be built.
///
/// Runtime catalogs (dynamic slot pools, projected views, string IDs read
/// back from a host or a file) go through [`ParamCatalog::try_new`] /
/// [`ParamCatalog::try_with_string_ids`] so a bad table is an error the
/// wrapper can report, never a panic inside a host callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// Two descriptors share one numeric host ID.
    DuplicateId {
        id: u32,
        first: &'static str,
        second: &'static str,
    },
    /// Two numeric IDs claim the same stable string ID.
    DuplicateStringId {
        string_id: String,
        first: u32,
        second: u32,
    },
    /// One numeric ID was given two different stable string IDs.
    ConflictingStringIds {
        id: u32,
        first: String,
        second: String,
    },
    /// A string ID names a numeric ID that has no descriptor (the string-ID
    /// list and the descriptor list disagree).
    UnknownId { id: u32, string_id: String },
    /// A string ID exceeds the OAST v2 wire limit (`u16::MAX` UTF-8 bytes).
    StringIdTooLong { id: u32, len: usize },
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateId { id, first, second } => {
                write!(f, "duplicate parameter ID {id}: '{first}' and '{second}'")
            }
            Self::DuplicateStringId {
                string_id,
                first,
                second,
            } => write!(
                f,
                "duplicate parameter string ID '{string_id}': IDs {first} and {second}"
            ),
            Self::ConflictingStringIds { id, first, second } => write!(
                f,
                "parameter ID {id} has two string IDs: '{first}' and '{second}'"
            ),
            Self::UnknownId { id, string_id } => write!(
                f,
                "string ID '{string_id}' names parameter ID {id}, which has no descriptor"
            ),
            Self::StringIdTooLong { id, len } => write!(
                f,
                "parameter ID {id} string ID is {len} bytes, exceeding the {} byte limit",
                u16::MAX
            ),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Declaration-order metadata plus indexed numeric-ID and stable-string-ID
/// lookup. Construct off audio; lookups neither allocate nor walk unrelated
/// descriptors.
///
/// String IDs are a side map: they never change or derive the numeric host
/// IDs, which stay exactly as declared so saved host automation keeps
/// resolving.
pub struct ParamCatalog {
    infos: Box<[ParamInfo]>,
    indices: HashMap<u32, usize>,
    string_ids: Box<[Option<String>]>,
    by_string: HashMap<String, usize>,
}

impl ParamCatalog {
    /// Build an immutable parameter catalog from a table known to be valid
    /// (the derive checks declared IDs at compile time).
    ///
    /// # Panics
    /// Panics if two descriptors have the same parameter ID. Use
    /// [`Self::try_new`] for tables assembled at runtime.
    #[must_use]
    pub fn new(infos: Vec<ParamInfo>) -> Self {
        Self::try_new(infos).unwrap_or_else(|err| panic!("{err}"))
    }

    /// Fallible [`Self::new`].
    ///
    /// # Errors
    /// [`CatalogError::DuplicateId`] if two descriptors share an ID.
    pub fn try_new(infos: Vec<ParamInfo>) -> Result<Self, CatalogError> {
        Self::try_with_string_ids(infos, Vec::new())
    }

    /// Build an immutable parameter catalog with stable string IDs attached
    /// to numeric host IDs, from a table known to be valid.
    ///
    /// # Panics
    /// Panics on any [`CatalogError`]. Use [`Self::try_with_string_ids`]
    /// for runtime or dynamic catalogs.
    #[must_use]
    pub fn with_string_ids(infos: Vec<ParamInfo>, string_ids: Vec<(u32, String)>) -> Self {
        Self::try_with_string_ids(infos, string_ids).unwrap_or_else(|err| panic!("{err}"))
    }

    /// Build a catalog with stable string IDs, rejecting a bad table instead
    /// of panicking. Empty string IDs are ignored; repeating an identical
    /// `(id, string_id)` pair is allowed.
    ///
    /// # Errors
    /// Returns the first [`CatalogError`] found: a duplicate numeric ID, a
    /// string ID claimed by two numeric IDs, a numeric ID given two string
    /// IDs, or a string ID naming an ID with no descriptor.
    pub fn try_with_string_ids(
        infos: Vec<ParamInfo>,
        string_ids_in: Vec<(u32, String)>,
    ) -> Result<Self, CatalogError> {
        Self::build(infos, string_ids_in, true)
    }

    /// Build a catalog that never fails: on a duplicate numeric ID the first
    /// descriptor keeps the ID lookup, and a conflicting or orphan string ID
    /// is dropped (the first claim wins). [`crate::Params::param_catalog`]
    /// falls back to this so a malformed runtime table degrades string-ID
    /// lookup instead of panicking a host.
    #[must_use]
    pub fn with_string_ids_lossy(infos: Vec<ParamInfo>, string_ids: Vec<(u32, String)>) -> Self {
        match Self::build(infos, string_ids, false) {
            Ok(catalog) => catalog,
            Err(_) => unreachable!("lossy catalog construction never fails"),
        }
    }

    fn build(
        infos: Vec<ParamInfo>,
        string_ids_in: Vec<(u32, String)>,
        strict: bool,
    ) -> Result<Self, CatalogError> {
        let mut indices: HashMap<u32, usize> = HashMap::with_capacity(infos.len());
        for (index, info) in infos.iter().enumerate() {
            if let Some(&previous) = indices.get(&info.id) {
                if strict {
                    return Err(CatalogError::DuplicateId {
                        id: info.id,
                        first: infos[previous].name,
                        second: info.name,
                    });
                }
                continue;
            }
            indices.insert(info.id, index);
        }
        let mut string_ids: Vec<Option<String>> = vec![None; infos.len()];
        let mut by_string: HashMap<String, usize> = HashMap::with_capacity(string_ids_in.len());
        for (id, key) in string_ids_in {
            if key.is_empty() {
                continue;
            }
            if key.len() > usize::from(u16::MAX) {
                if strict {
                    return Err(CatalogError::StringIdTooLong { id, len: key.len() });
                }
                continue;
            }
            let Some(&index) = indices.get(&id) else {
                if strict {
                    return Err(CatalogError::UnknownId { id, string_id: key });
                }
                continue;
            };
            if let Some(&previous) = by_string.get(&key) {
                if previous == index {
                    continue;
                }
                if strict {
                    return Err(CatalogError::DuplicateStringId {
                        string_id: key,
                        first: infos[previous].id,
                        second: id,
                    });
                }
                continue;
            }
            if let Some(existing) = &string_ids[index] {
                if strict {
                    return Err(CatalogError::ConflictingStringIds {
                        id,
                        first: existing.clone(),
                        second: key,
                    });
                }
                continue;
            }
            by_string.insert(key.clone(), index);
            string_ids[index] = Some(key);
        }
        Ok(Self {
            infos: infos.into_boxed_slice(),
            indices,
            string_ids: string_ids.into_boxed_slice(),
            by_string,
        })
    }

    #[must_use]
    pub fn by_id(&self, id: u32) -> Option<&ParamInfo> {
        self.indices.get(&id).map(|&index| &self.infos[index])
    }

    /// Stable string ID registered for `id`, if any (e.g. `"osc1.cutoff"`).
    #[must_use]
    pub fn string_id(&self, id: u32) -> Option<&str> {
        let &index = self.indices.get(&id)?;
        self.string_ids.get(index)?.as_deref()
    }

    /// Resolve a stable string ID to its numeric host parameter ID.
    #[must_use]
    pub fn id_by_string(&self, string_id: &str) -> Option<u32> {
        let &index = self.by_string.get(string_id)?;
        Some(self.infos[index].id)
    }

    /// Resolve a stable string ID directly to its parameter metadata.
    #[must_use]
    pub fn by_string_id(&self, string_id: &str) -> Option<&ParamInfo> {
        let &index = self.by_string.get(string_id)?;
        Some(&self.infos[index])
    }

    /// Dense declaration-order index of `id`.
    #[must_use]
    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.indices.get(&id).copied()
    }

    /// Descriptor at dense declaration-order `index`.
    #[must_use]
    pub fn by_index(&self, index: usize) -> Option<&ParamInfo> {
        self.infos.get(index)
    }

    /// Encode a dense catalog `index` as a non-zero host cookie (`index + 1`).
    ///
    /// Zero is reserved for "no cookie" (`NULL` in C APIs such as CLAP).
    #[must_use]
    pub const fn cookie_for_index(index: usize) -> usize {
        index.saturating_add(1)
    }

    /// Validate a host cookie against `id` in `O(1)` without hashing or scanning.
    ///
    /// Returns `None` if `cookie == 0`, out of bounds for the current catalog,
    /// or its slot's `ParamInfo::id` does not match `id` (e.g. a stale cookie
    /// surviving across a dynamic catalog rescan).
    #[must_use]
    pub fn validate_cookie(&self, cookie: usize, id: u32) -> Option<(usize, &ParamInfo)> {
        let index = cookie.checked_sub(1)?;
        let info = self.infos.get(index)?;
        (info.id == id).then_some((index, info))
    }

    /// Resolve `(dense_index, &ParamInfo)` using `cookie` on the `O(1)` fast
    /// path, falling back to [`Self::index_of`] when `cookie` is zero, out of
    /// bounds, or mismatched.
    #[must_use]
    pub fn resolve_cookie(&self, cookie: usize, id: u32) -> Option<(usize, &ParamInfo)> {
        self.validate_cookie(cookie, id).or_else(|| {
            let index = self.index_of(id)?;
            Some((index, &self.infos[index]))
        })
    }

    /// Sorted `(param_id, dense_index)` table for `O(log n)` `binary_search_by_key`
    /// lookups in adapters that maintain a flat sorted index (e.g. VST3).
    #[must_use]
    pub fn sorted_id_indices(&self) -> Vec<(u32, usize)> {
        let mut pairs: Vec<(u32, usize)> = self
            .infos
            .iter()
            .enumerate()
            .map(|(index, info)| (info.id, index))
            .collect();
        pairs.sort_unstable_by_key(|&(id, _)| id);
        pairs
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

    #[test]
    fn string_ids_round_trip_and_reject_collisions() {
        let catalog = ParamCatalog::with_string_ids(
            vec![info(10), info(20)],
            vec![(10, "osc1.cutoff".into()), (20, "osc1.res".into())],
        );
        assert_eq!(catalog.string_id(10), Some("osc1.cutoff"));
        assert_eq!(catalog.id_by_string("osc1.res"), Some(20));
        assert_eq!(catalog.by_string_id("osc1.cutoff").unwrap().id, 10);
        assert!(catalog.id_by_string("missing").is_none());
    }

    #[test]
    #[should_panic(expected = "duplicate parameter string ID 'osc1.cutoff'")]
    fn duplicate_string_id_panics_on_the_infallible_constructor() {
        let _ = ParamCatalog::with_string_ids(
            vec![info(10), info(20)],
            vec![(10, "osc1.cutoff".into()), (20, "osc1.cutoff".into())],
        );
    }

    #[test]
    fn runtime_catalogs_report_bad_tables_instead_of_panicking() {
        let err = |infos: Vec<ParamInfo>, ids: Vec<(u32, &str)>| {
            let ids = ids.into_iter().map(|(id, s)| (id, s.to_owned())).collect();
            ParamCatalog::try_with_string_ids(infos, ids)
                .err()
                .expect("rejected")
        };
        assert_eq!(
            err(vec![info(10), info(20)], vec![(10, "a"), (20, "a")]),
            CatalogError::DuplicateStringId {
                string_id: "a".into(),
                first: 10,
                second: 20
            }
        );
        assert_eq!(
            err(vec![info(10)], vec![(10, "a"), (10, "b")]),
            CatalogError::ConflictingStringIds {
                id: 10,
                first: "a".into(),
                second: "b".into()
            }
        );
        // More string IDs than descriptors: the extra one names no descriptor.
        assert_eq!(
            err(vec![info(10)], vec![(10, "a"), (11, "b")]),
            CatalogError::UnknownId {
                id: 11,
                string_id: "b".into()
            }
        );
        let dup = err(vec![info(10), info(10)], vec![]);
        assert!(matches!(dup, CatalogError::DuplicateId { id: 10, .. }));
        assert_eq!(
            dup.to_string(),
            "duplicate parameter ID 10: 'Value' and 'Value'"
        );
        assert!(ParamCatalog::try_new(vec![info(1), info(1)]).is_err());

        // Repeating an identical pair and empty keys are fine.
        let ok = ParamCatalog::try_with_string_ids(
            vec![info(10), info(20)],
            vec![(10, "a".into()), (10, "a".into()), (20, String::new())],
        )
        .expect("valid");
        assert_eq!(ok.string_id(10), Some("a"));
        assert_eq!(ok.string_id(20), None);
    }

    #[test]
    fn lossy_catalog_keeps_the_first_claim() {
        let catalog = ParamCatalog::with_string_ids_lossy(
            vec![info(10), info(20), info(10)],
            vec![(10, "a".into()), (20, "a".into()), (99, "b".into())],
        );
        assert_eq!(catalog.len(), 3, "descriptors are kept as given");
        assert_eq!(catalog.index_of(10), Some(0));
        assert_eq!(catalog.id_by_string("a"), Some(10));
        assert_eq!(catalog.string_id(20), None);
        assert_eq!(catalog.id_by_string("b"), None);
    }

    #[test]
    fn cookie_fast_path_null_fallback_and_stale_cookie_validation() {
        let catalog = ParamCatalog::new(vec![info(97), info(0x0100_0000), info(3)]);
        assert_eq!(catalog.index_of(97), Some(0));
        assert_eq!(catalog.index_of(0x0100_0000), Some(1));
        assert_eq!(catalog.index_of(3), Some(2));
        assert_eq!(catalog.index_of(42), None);

        // Fast-path: valid cookie for each slot resolves directly to (index, info).
        for (idx, id) in [(0, 97), (1, 0x0100_0000), (2, 3)] {
            let cookie = ParamCatalog::cookie_for_index(idx);
            assert!(cookie != 0);
            let (resolved_idx, resolved_info) = catalog
                .validate_cookie(cookie, id)
                .expect("valid cookie must validate");
            assert_eq!(resolved_idx, idx);
            assert_eq!(resolved_info.id, id);
            assert_eq!(
                catalog.resolve_cookie(cookie, id).map(|(i, p)| (i, p.id)),
                Some((idx, id))
            );
        }

        // Null cookie (0) is rejected by validate_cookie and falls back to ID lookup in resolve_cookie.
        assert!(catalog.validate_cookie(0, 97).is_none());
        assert_eq!(
            catalog.resolve_cookie(0, 97).map(|(i, p)| (i, p.id)),
            Some((0, 97))
        );

        // Stale / mismatched cookie (points at slot 0 = id 97, event asks for id 3)
        // is rejected by validate_cookie and resolved via ID fallback to slot 2.
        let slot0_cookie = ParamCatalog::cookie_for_index(0);
        assert!(catalog.validate_cookie(slot0_cookie, 3).is_none());
        assert_eq!(
            catalog
                .resolve_cookie(slot0_cookie, 3)
                .map(|(i, p)| (i, p.id)),
            Some((2, 3))
        );

        // Out-of-bounds cookie (from a larger prior catalog) falls back to ID lookup.
        let oob_cookie = ParamCatalog::cookie_for_index(99);
        assert!(catalog.validate_cookie(oob_cookie, 0x0100_0000).is_none());
        assert_eq!(
            catalog
                .resolve_cookie(oob_cookie, 0x0100_0000)
                .map(|(i, p)| (i, p.id)),
            Some((1, 0x0100_0000))
        );

        // Unknown param ID returns None even with a valid slot cookie.
        assert!(catalog.resolve_cookie(slot0_cookie, 999).is_none());
        assert_eq!(
            catalog.sorted_id_indices(),
            vec![(3, 2), (97, 0), (0x0100_0000, 1)]
        );
    }
}
