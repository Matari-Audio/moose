//! Canonical plugin-state wire format.
//!
//! Lives in `moose-utils` (rather than `moose-core`) so build-time
//! tools - `cargo-moose` emits factory-preset files containing this
//! envelope at install time - can produce byte-identical blobs
//! without inheriting `moose-core`'s runtime dependency chain.
//! `moose-core::state` re-exports everything here and layers the
//! plugin-coupled helpers (`apply_state`, `snapshot_plugin`, ...)
//! on top.

/// Magic bytes for state identification.
const STATE_MAGIC: &[u8; 4] = b"OAST";
/// Envelope v1: numeric-ID parameter entries.
pub const STATE_VERSION_V1: u32 = 1;
/// Envelope v2: schema-versioned, string-keyed parameter entries with numeric fallback IDs.
pub const STATE_VERSION_V2: u32 = 2;
const STATE_VERSION: u32 = STATE_VERSION_V1;

/// Serialize plugin state in canonical OAST v1 format: numeric parameter IDs + extra + persist.
/// Empty `extra` slice serializes as the same `0u64` length-prefix that an
/// absent extra block would, so callers don't need an `Option`
/// wrapper to express "no extra state".
#[must_use]
pub fn serialize_state(
    plugin_id_hash: u64,
    param_ids: &[u32],
    param_values: &[f64],
    extra: &[u8],
    persist: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();

    // Header
    data.extend_from_slice(STATE_MAGIC);
    data.extend_from_slice(&STATE_VERSION.to_le_bytes());
    data.extend_from_slice(&plugin_id_hash.to_le_bytes());

    // Parameter block
    let count = crate::cast::len_u32(param_ids.len());
    data.extend_from_slice(&count.to_le_bytes());
    for (id, value) in param_ids.iter().zip(param_values.iter()) {
        data.extend_from_slice(&id.to_le_bytes());
        data.extend_from_slice(&value.to_le_bytes());
    }

    // Extra state block: length-prefixed, may be zero-length.
    data.extend_from_slice(&(extra.len() as u64).to_le_bytes());
    data.extend_from_slice(extra);

    // `#[persist]` block: length-prefixed, appended after `extra`. A
    // trailing block so envelopes written before persistence existed
    // (which stop after `extra`) still parse - `parse_state` reads it
    // only when bytes remain, defaulting to empty.
    data.extend_from_slice(&(persist.len() as u64).to_le_bytes());
    data.extend_from_slice(persist);

    data
}

/// Serialize plugin state in OAST v2 format: schema version + string-keyed
/// parameter entries (carrying both stable string ID and fallback numeric ID)
/// + extra + persist.
///
/// # Panics
/// Panics if any parameter string key exceeds `u16::MAX` bytes.
#[must_use]
pub fn serialize_state_v2(
    plugin_id_hash: u64,
    schema_version: u32,
    named_params: &[(&str, u32, f64)],
    extra: &[u8],
    persist: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();

    // Header: magic + envelope version (2) + plugin hash + plugin schema version
    data.extend_from_slice(STATE_MAGIC);
    data.extend_from_slice(&STATE_VERSION_V2.to_le_bytes());
    data.extend_from_slice(&plugin_id_hash.to_le_bytes());
    data.extend_from_slice(&schema_version.to_le_bytes());

    // Parameter block: count + (u16 key_len, utf8 key, u32 numeric_id, f64 value)
    let count = crate::cast::len_u32(named_params.len());
    data.extend_from_slice(&count.to_le_bytes());
    for &(key, id, value) in named_params {
        let key_len = u16::try_from(key.len()).expect("parameter string ID exceeds u16::MAX bytes");
        data.extend_from_slice(&key_len.to_le_bytes());
        data.extend_from_slice(key.as_bytes());
        data.extend_from_slice(&id.to_le_bytes());
        data.extend_from_slice(&value.to_le_bytes());
    }

    // Extra state block
    data.extend_from_slice(&(extra.len() as u64).to_le_bytes());
    data.extend_from_slice(extra);

    // `#[persist]` block
    data.extend_from_slice(&(persist.len() as u64).to_le_bytes());
    data.extend_from_slice(persist);

    data
}

/// Serialize the envelope a plugin's state schema calls for - the one
/// writer every preset and session save goes through.
///
/// `schema_version == 0` writes OAST v1, byte-identical to
/// [`serialize_state`] (string IDs are dropped), so plugins that never opted
/// into schema versioning keep producing files older builds can read.
/// `schema_version >= 1` writes OAST v2 via [`serialize_state_v2`], carrying
/// the schema version and each entry's stable string ID beside its numeric
/// fallback ID. Entries are `(string_id, numeric_id, value)`; an empty
/// string ID round-trips by numeric ID only.
///
/// # Panics
/// On the v2 path, panics if a string ID exceeds `u16::MAX` bytes.
#[must_use]
pub fn serialize_state_for_schema(
    plugin_id_hash: u64,
    schema_version: u32,
    entries: &[(&str, u32, f64)],
    extra: &[u8],
    persist: &[u8],
) -> Vec<u8> {
    if schema_version == 0 {
        let ids: Vec<u32> = entries.iter().map(|&(_, id, _)| id).collect();
        let values: Vec<f64> = entries.iter().map(|&(_, _, value)| value).collect();
        serialize_state(plugin_id_hash, &ids, &values, extra, persist)
    } else {
        serialize_state_v2(plugin_id_hash, schema_version, entries, extra, persist)
    }
}

/// A string-keyed parameter entry decoded from an OAST v2 envelope, keeping
/// its optional numeric fallback ID attached so resolving the string key never
/// leaks the fallback onto another parameter that later reused the old ID.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedParam {
    pub key: String,
    pub fallback_id: Option<u32>,
    pub value: f64,
}

impl NamedParam {
    /// Construct a named parameter with no numeric fallback (e.g. from a
    /// schema migration step).
    #[must_use]
    pub fn new(key: impl Into<String>, value: f64) -> Self {
        Self {
            key: key.into(),
            fallback_id: None,
            value,
        }
    }
}

/// Deserialized state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeserializedState {
    /// Numeric-ID parameter entries (OAST v1 entries, unnamed OAST v2 entries,
    /// or the resolved output of `resolve_named_params`).
    pub params: Vec<(u32, f64)>,
    /// String-keyed parameter entries decoded from OAST v2 envelopes. Empty
    /// for v1 envelopes and after `resolve_named_params`.
    pub named_params: Vec<NamedParam>,
    /// Plugin-defined state schema version from OAST v2 (`0` for v1 envelopes).
    pub schema_version: u32,
    pub extra: Option<Vec<u8>>,
    /// `#[persist]`-field bytes (a keyed blob the `Params` derive
    /// produces). Empty when the plugin has no persist fields, or the
    /// envelope predates persistence.
    pub persist: Vec<u8>,
}

/// Outcome of [`parse_state`]: what the bytes turned out to be.
/// Splits the causes [`deserialize_state`] collapses into one `None`
/// so callers can route each one differently (fail the load, offer
/// the bytes to the plugin's `migrate_state` hook, ...).
pub enum StateParse {
    Ok(DeserializedState),
    /// Wrong magic / too short to carry the header: not a moose
    /// envelope at all. Candidate for legacy-state migration.
    NotAnEnvelope,
    /// Valid magic, `version` field newer than this build understands.
    /// Reserved for moose-side envelope evolution: never handed to
    /// the plugin, the load fails with a distinct log line.
    UnknownVersion(u32),
    /// Structurally valid envelope whose `plugin_id_hash` isn't the
    /// expected one - a renamed / re-identified plugin. Params and
    /// extra are already decoded; the caller decides whether to
    /// offer them to the plugin.
    WrongPlugin {
        found: u64,
        state: DeserializedState,
    },
    /// Magic + version matched but the param / extra blocks are
    /// truncated or inconsistent. Never migrated, the load fails.
    Corrupt,
}

/// Deserialize plugin state. Thin wrapper over [`parse_state`] for
/// callers that only care about the strict-success case (preset
/// codecs, tests).
#[must_use]
pub fn deserialize_state(data: &[u8], expected_plugin_id: u64) -> Option<DeserializedState> {
    match parse_state(data, expected_plugin_id) {
        StateParse::Ok(state) => Some(state),
        _ => None,
    }
}

/// Parse a state blob, distinguishing every failure cause. See
/// [`StateParse`] for the routing each variant is meant for.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn parse_state(data: &[u8], expected_plugin_id: u64) -> StateParse {
    if data.len() < 8 || &data[0..4] != STATE_MAGIC {
        return StateParse::NotAnEnvelope;
    }

    let Ok(version_bytes) = data[4..8].try_into() else {
        return StateParse::NotAnEnvelope;
    };
    let version = u32::from_le_bytes(version_bytes);
    if version != STATE_VERSION_V1 && version != STATE_VERSION_V2 {
        return StateParse::UnknownVersion(version);
    }

    // From here on the bytes claim to be a known-version envelope:
    // any structural violation is corruption, not foreign data.
    let Some(id_bytes) = data.get(8..16) else {
        return StateParse::Corrupt;
    };
    let Ok(id_bytes) = id_bytes.try_into() else {
        return StateParse::Corrupt;
    };
    let plugin_id = u64::from_le_bytes(id_bytes);

    let mut offset = 16;

    let schema_version = if version == STATE_VERSION_V2 {
        let Some(sv_bytes) = data.get(offset..offset + 4) else {
            return StateParse::Corrupt;
        };
        let Ok(sv_bytes) = sv_bytes.try_into() else {
            return StateParse::Corrupt;
        };
        offset += 4;
        u32::from_le_bytes(sv_bytes)
    } else {
        0
    };

    // Parameter block
    if offset + 4 > data.len() {
        return StateParse::Corrupt;
    }
    let Ok(count_bytes) = data[offset..offset + 4].try_into() else {
        return StateParse::Corrupt;
    };
    let count = u32::from_le_bytes(count_bytes) as usize;
    offset += 4;

    let (params, named_params) = if version == STATE_VERSION_V1 {
        // Cap the pre-allocation by what the remaining buffer could
        // possibly hold (12 bytes per v1 entry).
        let max_count = data.len().saturating_sub(offset) / 12;
        let mut params = Vec::with_capacity(count.min(max_count));
        for _ in 0..count {
            if offset + 12 > data.len() {
                return StateParse::Corrupt;
            }
            let Ok(id_bytes) = data[offset..offset + 4].try_into() else {
                return StateParse::Corrupt;
            };
            let id = u32::from_le_bytes(id_bytes);
            offset += 4;
            let Ok(value_bytes) = data[offset..offset + 8].try_into() else {
                return StateParse::Corrupt;
            };
            let value = f64::from_le_bytes(value_bytes);
            offset += 8;
            params.push((id, value));
        }
        (params, Vec::new())
    } else {
        // v2 entry: u16 key_len + key_bytes + u32 numeric_id + f64 value (min 14 bytes).
        let max_count = data.len().saturating_sub(offset) / 14;
        let prealloc = count.min(max_count);
        let mut params = Vec::with_capacity(prealloc);
        let mut named_params = Vec::with_capacity(prealloc);
        for _ in 0..count {
            if offset + 2 > data.len() {
                return StateParse::Corrupt;
            }
            let Ok(klen_bytes) = data[offset..offset + 2].try_into() else {
                return StateParse::Corrupt;
            };
            let key_len = usize::from(u16::from_le_bytes(klen_bytes));
            offset += 2;
            let Some(key_end) = offset.checked_add(key_len) else {
                return StateParse::Corrupt;
            };
            let Some(entry_end) = key_end.checked_add(12) else {
                return StateParse::Corrupt;
            };
            if entry_end > data.len() {
                return StateParse::Corrupt;
            }
            let Ok(key) = std::str::from_utf8(&data[offset..key_end]) else {
                return StateParse::Corrupt;
            };
            offset = key_end;
            let Ok(id_bytes) = data[offset..offset + 4].try_into() else {
                return StateParse::Corrupt;
            };
            let id = u32::from_le_bytes(id_bytes);
            offset += 4;
            let Ok(value_bytes) = data[offset..offset + 8].try_into() else {
                return StateParse::Corrupt;
            };
            let value = f64::from_le_bytes(value_bytes);
            offset += 8;
            if key.is_empty() {
                params.push((id, value));
            } else {
                named_params.push(NamedParam {
                    key: key.to_owned(),
                    fallback_id: Some(id),
                    value,
                });
            }
        }
        (params, named_params)
    };

    // Extra state block
    if offset + 8 > data.len() {
        return StateParse::Corrupt;
    }
    let Ok(len_bytes) = data[offset..offset + 8].try_into() else {
        return StateParse::Corrupt;
    };
    // The wire format encodes `extra_len` as `u64`; on 32-bit
    // targets the cast may truncate, but the next branch validates
    // `offset.checked_add(extra_len)` against the buffer length.
    #[allow(clippy::cast_possible_truncation)]
    let extra_len = u64::from_le_bytes(len_bytes) as usize;
    offset += 8;

    let extra = if extra_len > 0 {
        // `offset + extra_len` can wrap to a small value when
        // `extra_len` is huge (host-supplied), making the comparison
        // pass even though the slice would overrun. Use `checked_add`
        // and reject overflow as malformed.
        match offset.checked_add(extra_len) {
            Some(end) if end <= data.len() => Some(data[offset..end].to_vec()),
            _ => return StateParse::Corrupt,
        }
    } else {
        None
    };
    offset += extra_len;

    // Only legacy v1 envelopes may omit the trailing persist length.
    let persist = if offset + 8 <= data.len() {
        let Ok(len_bytes) = data[offset..offset + 8].try_into() else {
            return StateParse::Corrupt;
        };
        #[allow(clippy::cast_possible_truncation)]
        let persist_len = u64::from_le_bytes(len_bytes) as usize;
        offset += 8;
        match offset.checked_add(persist_len) {
            Some(end) if end <= data.len() => {
                if version == STATE_VERSION_V2 && end != data.len() {
                    return StateParse::Corrupt;
                }
                data[offset..end].to_vec()
            }
            _ => return StateParse::Corrupt,
        }
    } else if version == STATE_VERSION_V2 {
        return StateParse::Corrupt;
    } else {
        Vec::new()
    };

    let state = DeserializedState {
        params,
        named_params,
        schema_version,
        extra,
        persist,
    };
    if plugin_id == expected_plugin_id {
        StateParse::Ok(state)
    } else {
        StateParse::WrongPlugin {
            found: plugin_id,
            state,
        }
    }
}

/// Compute a simple hash of the plugin ID string for state identification.
///
/// Uses FNV-1a-64. **Do not change this without bumping the envelope's
/// `STATE_VERSION` and writing a migration:** the returned hash is
/// stored verbatim in every `.pluginstate` blob the host has saved,
/// and a different algorithm here would invalidate every shipped
/// session. If a stronger hash is ever needed, it must be selected via
/// the version byte in the envelope, not by replacing this function in
/// place.
#[must_use]
pub fn hash_plugin_id(id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3); // FNV prime
    }
    hash
}

/// The 16-byte VST3 class ID for a plugin ID string.
///
/// FNV-1a-128, per <http://www.isthe.com/chongo/tech/comp/fnv/>.
/// Standard constants - DAWs persist this CID as the plugin's identity
/// in saved sessions and `.vstpreset` files, so the algorithm and
/// constants must stay stable across releases. `moose-vst3` reports
/// this as the component's TUID; `cargo-moose` stamps the same bytes
/// (hex-encoded) into emitted `.vstpreset` headers.
#[must_use]
pub fn vst3_cid(id: &str) -> [u8; 16] {
    const FNV_OFFSET_BASIS: u128 = 0x6C62_272E_07BB_0142_62B8_2175_6295_C58D;
    const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013B;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in id.bytes() {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash.to_le_bytes()
}

/// Resolve an optional exact VST3 class ID, preserving the historical
/// [`vst3_cid`] derivation when no override is declared.
#[must_use]
pub fn resolve_vst3_cid(explicit: Option<[u8; 16]>, id: &str) -> [u8; 16] {
    explicit.unwrap_or_else(|| vst3_cid(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_state() {
        let plugin_id = hash_plugin_id("com.test.plugin");
        let ids = [0u32, 1, 2];
        let values = [0.5f64, 1.0, -12.0];
        let extra = b"hello extra state";

        let data = serialize_state(plugin_id, &ids, &values, extra, &[]);
        let state = deserialize_state(&data, plugin_id).unwrap();

        assert_eq!(state.params.len(), 3);
        assert_eq!(state.params[0], (0, 0.5));
        assert_eq!(state.params[1], (1, 1.0));
        assert_eq!(state.params[2], (2, -12.0));
        assert_eq!(state.extra.unwrap(), b"hello extra state");
    }

    #[test]
    fn wrong_plugin_id_fails() {
        let plugin_id = hash_plugin_id("com.test.plugin");
        let data = serialize_state(plugin_id, &[], &[], &[], &[]);
        assert!(deserialize_state(&data, 12345).is_none());
    }

    #[test]
    fn parse_distinguishes_foreign_bytes() {
        assert!(matches!(
            parse_state(b"{\"legacy\":true}", 1),
            StateParse::NotAnEnvelope
        ));
        assert!(matches!(parse_state(b"", 1), StateParse::NotAnEnvelope));
        assert!(matches!(parse_state(b"OAS", 1), StateParse::NotAnEnvelope));
    }

    #[test]
    fn parse_distinguishes_future_version() {
        let mut data = serialize_state(1, &[], &[], &[], &[]);
        data[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            parse_state(&data, 1),
            StateParse::UnknownVersion(99)
        ));
    }

    #[test]
    fn round_trip_state_v2_with_string_keys_and_schema_version() {
        let plugin_id = hash_plugin_id("com.test.v2");
        let entries = [("osc1.cutoff", 10u32, 440.0), ("mix", 20u32, 0.5)];
        let data = serialize_state_v2(plugin_id, 3, &entries, b"extra-v2", b"persist-v2");
        let state = deserialize_state(&data, plugin_id).expect("v2 envelope parses");
        assert_eq!(state.schema_version, 3);
        assert_eq!(state.params, Vec::new());
        assert_eq!(
            state.named_params,
            vec![
                NamedParam {
                    key: "osc1.cutoff".to_string(),
                    fallback_id: Some(10),
                    value: 440.0,
                },
                NamedParam {
                    key: "mix".to_string(),
                    fallback_id: Some(20),
                    value: 0.5,
                },
            ]
        );
        assert_eq!(state.extra.as_deref(), Some(&b"extra-v2"[..]));
        assert_eq!(state.persist, b"persist-v2");

        // Trailing bytes after a v2 envelope are rejected as Corrupt.
        let mut trailing = data.clone();
        trailing.push(0x00);
        assert!(matches!(
            parse_state(&trailing, plugin_id),
            StateParse::Corrupt
        ));

        // Corrupt UTF-8 key is rejected as Corrupt.
        let mut bad = data;
        bad[26] = 0xFF;
        assert!(matches!(parse_state(&bad, plugin_id), StateParse::Corrupt));
    }

    #[test]
    fn parse_decodes_wrong_plugin_envelope() {
        let data = serialize_state(99, &[7], &[0.25], b"extra", &[]);
        match parse_state(&data, 1) {
            StateParse::WrongPlugin { found, state } => {
                assert_eq!(found, 99);
                assert_eq!(state.params, vec![(7, 0.25)]);
                assert_eq!(state.extra.as_deref(), Some(&b"extra"[..]));
            }
            _ => panic!("expected WrongPlugin"),
        }
    }

    #[test]
    fn parse_flags_truncated_envelope_as_corrupt() {
        let data = serialize_state(1, &[7, 8], &[0.25, 0.5], b"extra", &[]);
        // Cut inside the param block: valid header, broken body.
        assert!(matches!(parse_state(&data[..22], 1), StateParse::Corrupt));
    }
}
