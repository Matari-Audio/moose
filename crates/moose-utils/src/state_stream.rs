//! Bounded incremental OAST input. No full-envelope staging allocation: read
//! parameter records and the two owned state lanes directly into their final
//! buffers. Legacy foreign blobs retain the bounded migration fallback.
use crate::state::{DeserializedState, NamedParam, STATE_VERSION_V1, STATE_VERSION_V2, StateParse};

pub enum StreamState {
    Envelope(StateParse),
    Foreign(Vec<u8>),
}

struct Input<F> {
    read: F,
    used: usize,
    limit: usize,
}
impl<F: FnMut(&mut [u8]) -> i64> Input<F> {
    fn read(&mut self, dst: &mut [u8]) -> Option<usize> {
        if dst.is_empty() {
            return Some(0);
        }
        let request = dst.len().min(8192);
        let dst = &mut dst[..request];
        let n = usize::try_from((self.read)(dst)).ok()?;
        self.used = self.used.checked_add(n)?;
        (n <= dst.len() && self.used <= self.limit).then_some(n)
    }
    fn exact(&mut self, mut dst: &mut [u8]) -> Option<()> {
        if dst.len() > self.limit.checked_sub(self.used)? {
            return None;
        }
        while !dst.is_empty() {
            let request = dst.len().min(8192);
            let n = self.read(&mut dst[..request])?;
            if n == 0 {
                return None;
            }
            dst = &mut dst[n..];
        }
        Some(())
    }
    fn word<const N: usize>(&mut self) -> Option<[u8; N]> {
        let mut out = [0; N];
        self.exact(&mut out)?;
        Some(out)
    }
    fn bytes(&mut self, len: usize) -> Option<Vec<u8>> {
        if len > self.limit.checked_sub(self.used)? {
            return None;
        }
        let mut out = Vec::new();
        out.try_reserve_exact(len).ok()?;
        out.resize(len, 0);
        self.exact(&mut out)?;
        Some(out)
    }
}

impl<F: FnMut(&mut [u8]) -> i64> std::io::Read for Input<F> {
    fn read(&mut self, dst: &mut [u8]) -> std::io::Result<usize> {
        Input::read(self, dst).ok_or_else(|| std::io::Error::other("invalid host state read"))
    }
}

/// Accept arbitrary short reads, reject impossible counts, errors, overflow,
/// trailing bytes and oversized declared lanes before allocating them.
/// All allocation and callbacks occur on the host's state thread.
pub fn read_state_stream(read: impl FnMut(&mut [u8]) -> i64, limit: usize) -> Option<StreamState> {
    read_state_stream_prepared::<()>(read, limit, |_, _, _, _| None).map(|(state, _)| state)
}

/// A prepared field decoder sees only the declared persistence section. Its
/// commit is returned only after the entire envelope, including EOF, is valid.
pub fn read_state_stream_prepared<T>(
    read: impl FnMut(&mut [u8]) -> i64,
    limit: usize,
    prepare: impl FnOnce(
        u64,
        u32,
        &mut dyn std::io::Read,
        usize,
    ) -> Option<std::io::Result<(Vec<u8>, T)>>,
) -> Option<(StreamState, Option<T>)> {
    let mut input = Input {
        read,
        used: 0,
        limit,
    };
    let mut prefix = Vec::new();
    while prefix.len() < 4 {
        let mut buf = [0; 4];
        let n = input.read(&mut buf[..4 - prefix.len()])?;
        if n == 0 {
            break;
        }
        prefix.extend_from_slice(&buf[..n]);
    }
    if prefix != b"OAST" {
        let mut buf = [0; 8192];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            prefix.extend_from_slice(&buf[..n]);
        }
        return Some((StreamState::Foreign(prefix), None));
    }
    let version = u32::from_le_bytes(input.word()?);
    if version != STATE_VERSION_V1 && version != STATE_VERSION_V2 {
        return Some((
            StreamState::Envelope(StateParse::UnknownVersion(version)),
            None,
        ));
    }
    let plugin_id = u64::from_le_bytes(input.word()?);
    let schema_version = if version == STATE_VERSION_V2 {
        u32::from_le_bytes(input.word()?)
    } else {
        0
    };
    let count = u32::from_le_bytes(input.word()?) as usize;
    let (params, named_params) = if version == STATE_VERSION_V1 {
        if count.checked_mul(12)? > limit.checked_sub(input.used)? {
            return None;
        }
        let mut params = Vec::new();
        params.try_reserve_exact(count).ok()?;
        for _ in 0..count {
            params.push((
                u32::from_le_bytes(input.word()?),
                f64::from_le_bytes(input.word()?),
            ));
        }
        (params, Vec::new())
    } else {
        if count.checked_mul(14)? > limit.checked_sub(input.used)? {
            return None;
        }
        let mut params = Vec::new();
        params.try_reserve_exact(count).ok()?;
        let mut named_params = Vec::new();
        named_params.try_reserve_exact(count).ok()?;
        for _ in 0..count {
            let key_len = usize::from(u16::from_le_bytes(input.word()?));
            let key_bytes = input.bytes(key_len)?;
            let key = String::from_utf8(key_bytes).ok()?;
            let id = u32::from_le_bytes(input.word()?);
            let value = f64::from_le_bytes(input.word()?);
            if key.is_empty() {
                params.push((id, value));
            } else {
                named_params.push(NamedParam {
                    key,
                    fallback_id: Some(id),
                    value,
                });
            }
        }
        (params, named_params)
    };
    let extra_len = usize::try_from(u64::from_le_bytes(input.word()?)).ok()?;
    let extra = if extra_len == 0 {
        None
    } else {
        Some(input.bytes(extra_len)?)
    };
    let mut first = [0];
    let mut prepared = None;
    let persist = if input.read(&mut first)? == 0 {
        Vec::new()
    } else {
        let mut word = [0; 8];
        word[0] = first[0];
        input.exact(&mut word[1..])?;
        let len = usize::try_from(u64::from_le_bytes(word)).ok()?;
        if len > limit.checked_sub(input.used)? {
            return None;
        }
        let mut section = std::io::Read::take(&mut input, len as u64);
        let result = prepare(plugin_id, schema_version, &mut section, len);
        let bytes = if let Some(result) = result {
            let (bytes, commit) = result.ok()?;
            if section.limit() != 0 {
                return None;
            }
            prepared = Some(commit);
            bytes
        } else {
            if section.limit() != len as u64 {
                return None;
            }
            input.bytes(len)?
        };
        if input.read(&mut first)? != 0 {
            return None;
        }
        bytes
    };
    // Reuse the format parser's routing for renamed plugin IDs without copying
    // either custom-state lane into another whole-envelope buffer.
    Some((
        StreamState::Envelope(StateParse::WrongPlugin {
            found: plugin_id,
            state: DeserializedState {
                params,
                named_params,
                schema_version,
                extra,
                persist,
            },
        }),
        prepared,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(bytes: &[u8], chunk: usize, limit: usize) -> Option<StreamState> {
        let mut pos = 0;
        read_state_stream(
            |dst| {
                let n = dst.len().min(chunk).min(bytes.len() - pos);
                dst[..n].copy_from_slice(&bytes[pos..pos + n]);
                pos += n;
                i64::try_from(n).unwrap()
            },
            limit,
        )
    }
    #[test]
    fn short_reads_large_lanes_and_legacy_are_lossless() {
        let limit = if cfg!(miri) { 1024 } else { 32 * 1024 * 1024 };
        let persist = vec![173; limit + 1024];
        let blob = crate::state::serialize_state(42, &[7], &[0.25], b"extra", &persist);
        let Some(StreamState::Envelope(StateParse::WrongPlugin { found, state })) =
            parse(&blob, 317, blob.len())
        else {
            panic!("stream rejected");
        };
        assert_eq!(found, 42);
        assert_eq!(state.params, [(7, 0.25)]);
        assert_eq!(state.extra.as_deref(), Some(b"extra".as_slice()));
        assert_eq!(state.persist, persist);
        assert!(parse(&blob, 8192, limit).is_none());
        assert!(matches!(parse(b"old",1,32),Some(StreamState::Foreign(bytes)) if bytes==b"old"));
    }
    #[test]
    fn malformed_and_overreported_streams_are_rejected() {
        assert!(read_state_stream(|_| -1, 1024).is_none());
        assert!(read_state_stream(|dst| i64::try_from(dst.len()).unwrap() + 1, 1024).is_none());
        let blob = crate::state::serialize_state(42, &[], &[], b"x", b"y");
        for end in 4..blob.len() {
            // Older envelopes may omit the entire persist lane.
            if end != blob.len() - 9 {
                assert!(parse(&blob[..end], 3, 1024).is_none(), "end={end}");
            }
        }
        let mut bad = blob.clone();
        bad.extend_from_slice(b"junk");
        assert!(parse(&bad, 17, 1024).is_none());
        let mut bad = blob;
        bad[20..28].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse(&bad, 17, 1024).is_none());
    }
}

/// Write byte-identical OAST v1 framing without a second full-state buffer.
/// Reject a state outside the plugin's proven bound before writing any bytes.
pub fn write_state_stream(
    plugin_id: u64,
    ids: &[u32],
    values: &[f64],
    extra: &[u8],
    persist: &[u8],
    limit: usize,
    write: impl FnMut(&[u8]) -> i64,
) -> bool {
    write_state_stream_with_persist(
        plugin_id,
        ids,
        values,
        extra,
        persist.len() as u64,
        limit,
        |output| output.write_all(persist),
        write,
    )
}

/// Stream a frozen persistence snapshot without an encoded lane-sized buffer.
/// `persist_len` is checked against the envelope bound before any host write.
/// The producer must emit exactly that length; short/long output and host
/// errors return false. Callers must discard any partial state on failure.
#[allow(clippy::too_many_arguments)]
pub fn write_state_stream_with_persist(
    plugin_id: u64,
    ids: &[u32],
    values: &[f64],
    extra: &[u8],
    persist_len: u64,
    limit: usize,
    persist: impl FnOnce(&mut dyn std::io::Write) -> std::io::Result<()>,
    write: impl FnMut(&[u8]) -> i64,
) -> bool {
    use std::io::Write;
    let Some(size) = state_stream_size(ids, values, extra, persist_len, limit) else {
        return false;
    };
    let Ok(count) = u32::try_from(ids.len()) else {
        return false;
    };
    let mut output = Output {
        write,
        remaining: size,
    };
    let result = (|| {
        output.write_all(b"OAST")?;
        output.write_all(&1_u32.to_le_bytes())?;
        output.write_all(&plugin_id.to_le_bytes())?;
        output.write_all(&count.to_le_bytes())?;
        for (&id, &value) in ids.iter().zip(values) {
            output.write_all(&id.to_le_bytes())?;
            output.write_all(&value.to_le_bytes())?;
        }
        output.write_all(&(extra.len() as u64).to_le_bytes())?;
        output.write_all(extra)?;
        output.write_all(&persist_len.to_le_bytes())?;
        persist(&mut output)
    })();
    result.is_ok() && output.remaining == 0
}

/// Validate all lengths before allocating a contiguous host-owned v2 output.
#[must_use]
pub fn state_stream_size_v2(
    named_params: &[(&str, u32, f64)],
    extra: &[u8],
    persist_len: u64,
    limit: usize,
) -> Option<usize> {
    if u32::try_from(named_params.len()).is_err() {
        return None;
    }
    let persist_size = usize::try_from(persist_len).ok()?;
    let mut total: usize = 40;
    for &(key, _, _) in named_params {
        if u16::try_from(key.len()).is_err() {
            return None;
        }
        total = total.checked_add(14)?.checked_add(key.len())?;
    }
    total
        .checked_add(extra.len())?
        .checked_add(persist_size)
        .filter(|size| *size <= limit)
}

/// Write byte-identical OAST v2 framing without a second full-state buffer.
#[allow(clippy::too_many_arguments)]
pub fn write_state_stream_v2(
    plugin_id: u64,
    schema_version: u32,
    named_params: &[(&str, u32, f64)],
    extra: &[u8],
    persist: &[u8],
    limit: usize,
    write: impl FnMut(&[u8]) -> i64,
) -> bool {
    write_state_stream_v2_with_persist(
        plugin_id,
        schema_version,
        named_params,
        extra,
        persist.len() as u64,
        limit,
        |output| output.write_all(persist),
        write,
    )
}

/// Stream a frozen persistence snapshot inside an OAST v2 envelope.
#[allow(clippy::too_many_arguments)]
pub fn write_state_stream_v2_with_persist(
    plugin_id: u64,
    schema_version: u32,
    named_params: &[(&str, u32, f64)],
    extra: &[u8],
    persist_len: u64,
    limit: usize,
    persist: impl FnOnce(&mut dyn std::io::Write) -> std::io::Result<()>,
    write: impl FnMut(&[u8]) -> i64,
) -> bool {
    use std::io::Write;
    let Some(size) = state_stream_size_v2(named_params, extra, persist_len, limit) else {
        return false;
    };
    let Ok(count) = u32::try_from(named_params.len()) else {
        return false;
    };
    let mut output = Output {
        write,
        remaining: size,
    };
    let result = (|| {
        output.write_all(b"OAST")?;
        output.write_all(&STATE_VERSION_V2.to_le_bytes())?;
        output.write_all(&plugin_id.to_le_bytes())?;
        output.write_all(&schema_version.to_le_bytes())?;
        output.write_all(&count.to_le_bytes())?;
        for &(key, id, value) in named_params {
            let key_len = u16::try_from(key.len())
                .map_err(|_| std::io::Error::other("parameter string ID exceeds u16::MAX"))?;
            output.write_all(&key_len.to_le_bytes())?;
            output.write_all(key.as_bytes())?;
            output.write_all(&id.to_le_bytes())?;
            output.write_all(&value.to_le_bytes())?;
        }
        output.write_all(&(extra.len() as u64).to_le_bytes())?;
        output.write_all(extra)?;
        output.write_all(&persist_len.to_le_bytes())?;
        persist(&mut output)
    })();
    result.is_ok() && output.remaining == 0
}

/// Validate all lengths before allocating a contiguous host-owned output.
#[must_use]
pub fn state_stream_size(
    ids: &[u32],
    values: &[f64],
    extra: &[u8],
    persist_len: u64,
    limit: usize,
) -> Option<usize> {
    if ids.len() != values.len() || u32::try_from(ids.len()).is_err() {
        return None;
    }
    let persist_size = usize::try_from(persist_len).ok()?;
    ids.len()
        .checked_mul(12)?
        .checked_add(36)?
        .checked_add(extra.len())?
        .checked_add(persist_size)
        .filter(|size| *size <= limit)
}

struct Output<F> {
    write: F,
    remaining: usize,
}

impl<F: FnMut(&[u8]) -> i64> std::io::Write for Output<F> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other("oversized state output"));
        }
        let request = &bytes[..bytes.len().min(8192)];
        let n = usize::try_from((self.write)(request))
            .map_err(|_| std::io::Error::other("host state write failed"))?;
        if n > request.len() {
            return Err(std::io::Error::other("invalid host state write count"));
        }
        self.remaining -= n;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;
    #[test]
    fn streamed_output_matches_old_envelopes_and_partial_write_errors_are_honest() {
        let ids = [1, 7];
        let values = [-0.0, f64::from_bits(0x3fda_1234_5678_90ab)];
        let limit = if cfg!(miri) {
            16 * 1024
        } else {
            32 * 1024 * 1024
        };
        let persist = vec![
            73;
            if cfg!(miri) {
                17 * 1024
            } else {
                33 * 1024 * 1024
            }
        ];
        let expected = crate::state::serialize_state(42, &ids, &values, b"extra", &persist);
        let mut output = Vec::new();
        assert!(write_state_stream(
            42,
            &ids,
            &values,
            b"extra",
            &persist,
            expected.len(),
            |bytes| {
                let n = bytes.len().min(317);
                output.extend_from_slice(&bytes[..n]);
                i64::try_from(n).unwrap()
            }
        ));
        assert_eq!(output, expected);
        assert!(!write_state_stream(
            42,
            &ids,
            &values,
            b"extra",
            &persist,
            limit,
            |_| panic!("oversize wrote bytes")
        ));
        for reply in [-1, 0, 10000] {
            assert!(!write_state_stream(42, &[], &[], b"", b"", 1024, |_| reply));
        }
    }

    #[test]
    fn prepared_lane_streams_without_staging_and_preserves_float_bits() {
        let ids = [5];
        let values = [-0.0];
        let payload = vec![31; if cfg!(miri) { 8_194 } else { 65_537 }];
        let expected = crate::state::serialize_state(42, &ids, &values, b"extra", &payload);
        let mut output = Vec::new();
        assert!(write_state_stream_with_persist(
            42,
            &ids,
            &values,
            b"extra",
            payload.len() as u64,
            expected.len(),
            |writer| {
                // Multiple payload segments, including one larger than the
                // host callback's bounded request size, need no staging blob.
                writer.write_all(&payload[..1])?;
                writer.write_all(&payload[1..])
            },
            |bytes| {
                assert!(bytes.len() <= 8192);
                let len = bytes.len().min(13);
                output.extend_from_slice(&bytes[..len]);
                i64::try_from(len).unwrap()
            }
        ));
        assert_eq!(output, expected);
    }

    #[test]
    fn prepared_lane_bound_and_length_failures_are_honest() {
        for len in [u64::MAX, 1025] {
            assert!(!write_state_stream_with_persist(
                42,
                &[],
                &[],
                b"",
                len,
                1024,
                |_| panic!("oversize prepared its output"),
                |_| panic!("oversize wrote bytes")
            ));
        }
        for actual in [0, 2] {
            let mut bytes = Vec::new();
            assert!(!write_state_stream_with_persist(
                42,
                &[],
                &[],
                b"",
                1,
                1024,
                |writer| writer.write_all(&[9; 2][..actual]),
                |part| {
                    bytes.extend_from_slice(part);
                    i64::try_from(part.len()).unwrap()
                }
            ));
            assert!(bytes.len() <= 37);
        }
        assert!(!write_state_stream_with_persist(
            42,
            &[],
            &[],
            b"",
            1,
            1024,
            |_| Err(std::io::Error::other("encoding failed")),
            |part| i64::try_from(part.len()).unwrap()
        ));
    }
}

#[cfg(test)]
mod prepared_tests {
    use super::*;
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the callback contract permits skipping preparation"
    )]
    fn prepare(
        _id: u64,
        _schema: u32,
        input: &mut dyn std::io::Read,
        len: usize,
    ) -> Option<std::io::Result<(Vec<u8>, u64)>> {
        Some((|| {
            let mut remaining = len;
            let mut sum = 0_u64;
            let mut scratch = [0; 8192];
            while remaining > 0 {
                let n = remaining.min(scratch.len());
                input.read_exact(&mut scratch[..n])?;
                sum += scratch[..n]
                    .iter()
                    .map(|byte| u64::from(*byte))
                    .sum::<u64>();
                remaining -= n;
            }
            Ok((Vec::new(), sum))
        })())
    }
    #[test]
    fn prepared_fields_do_not_return_a_serialized_staging_buffer() {
        let len = if cfg!(miri) { 10_000 } else { 100_000 };
        let payload = vec![37; len];
        let blob = crate::state::serialize_state(42, &[7], &[0.25], b"", &payload);
        let mut pos = 0;
        let (state, prepared) = read_state_stream_prepared(
            |dst| {
                assert!(dst.len() <= 8192);
                let n = dst.len().min(317).min(blob.len() - pos);
                dst[..n].copy_from_slice(&blob[pos..pos + n]);
                pos += n;
                i64::try_from(n).unwrap()
            },
            blob.len(),
            prepare,
        )
        .unwrap();
        assert_eq!(prepared, Some(37 * len as u64));
        let StreamState::Envelope(StateParse::WrongPlugin { state, .. }) = state else {
            panic!()
        };
        assert_eq!(state.persist, [] as [u8; 0]);
        assert_eq!(state.params, [(7, 0.25)]);
    }
    #[test]
    fn truncated_trailing_and_underconsumed_preparations_never_escape() {
        let base = crate::state::serialize_state(42, &[], &[], b"", &[7; 100]);
        for blob in [
            &base[..base.len() - 1],
            &[base.as_slice(), b"junk"].concat(),
        ] {
            let mut pos = 0;
            assert!(
                read_state_stream_prepared(
                    |dst| {
                        let n = dst.len().min(blob.len() - pos);
                        dst[..n].copy_from_slice(&blob[pos..pos + n]);
                        pos += n;
                        i64::try_from(n).unwrap()
                    },
                    1024,
                    prepare
                )
                .is_none()
            );
        }
        let mut pos = 0;
        assert!(
            read_state_stream_prepared(
                |dst| {
                    let n = dst.len().min(base.len() - pos);
                    dst[..n].copy_from_slice(&base[pos..pos + n]);
                    pos += n;
                    i64::try_from(n).unwrap()
                },
                1024,
                |_, _, _, _| Some(Ok((Vec::new(), ())))
            )
            .is_none()
        );
    }

    #[test]
    fn v2_stream_write_and_read_match_serialize_state_v2() {
        let entries = [("osc1.cutoff", 10u32, 880.0), ("mix", 20u32, 0.25)];
        let expected =
            crate::state::serialize_state_v2(42, 7, &entries, b"extra-v2", b"persist-v2");
        let mut out = Vec::new();
        assert!(write_state_stream_v2(
            42,
            7,
            &entries,
            b"extra-v2",
            b"persist-v2",
            expected.len(),
            |bytes| {
                let n = bytes.len().min(11);
                out.extend_from_slice(&bytes[..n]);
                i64::try_from(n).unwrap()
            }
        ));
        assert_eq!(out, expected);

        let mut pos = 0;
        let Some(StreamState::Envelope(StateParse::WrongPlugin { found, state })) =
            read_state_stream(
                |dst| {
                    let n = dst.len().min(7).min(out.len() - pos);
                    dst[..n].copy_from_slice(&out[pos..pos + n]);
                    pos += n;
                    i64::try_from(n).unwrap()
                },
                out.len(),
            )
        else {
            panic!("v2 stream rejected");
        };
        assert_eq!(found, 42);
        assert_eq!(state.schema_version, 7);
        assert_eq!(state.params, Vec::new());
        assert_eq!(
            state.named_params,
            vec![
                NamedParam {
                    key: "osc1.cutoff".to_string(),
                    fallback_id: Some(10),
                    value: 880.0,
                },
                NamedParam {
                    key: "mix".to_string(),
                    fallback_id: Some(20),
                    value: 0.25,
                },
            ]
        );
        assert_eq!(state.extra.as_deref(), Some(&b"extra-v2"[..]));
        assert_eq!(state.persist, b"persist-v2");
    }
}
