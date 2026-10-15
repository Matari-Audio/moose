//! Bounded incremental OAST input. No full-envelope staging allocation: read
//! parameter records and the two owned state lanes directly into their final
//! buffers. Legacy foreign blobs retain the bounded migration fallback.
use crate::state::{DeserializedState, StateParse};

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

/// Accept arbitrary short reads, reject impossible counts, errors, overflow,
/// trailing bytes and oversized declared lanes before allocating them.
/// All allocation and callbacks occur on the host's state thread.
pub fn read_state_stream(read: impl FnMut(&mut [u8]) -> i64, limit: usize) -> Option<StreamState> {
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
        return Some(StreamState::Foreign(prefix));
    }
    let version = u32::from_le_bytes(input.word()?);
    if version != 1 {
        return Some(StreamState::Envelope(StateParse::UnknownVersion(version)));
    }
    let plugin_id = u64::from_le_bytes(input.word()?);
    let count = u32::from_le_bytes(input.word()?) as usize;
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
    let extra_len = usize::try_from(u64::from_le_bytes(input.word()?)).ok()?;
    let extra = if extra_len == 0 {
        None
    } else {
        Some(input.bytes(extra_len)?)
    };
    let mut first = [0];
    let persist = if input.read(&mut first)? == 0 {
        Vec::new()
    } else {
        let mut word = [0; 8];
        word[0] = first[0];
        input.exact(&mut word[1..])?;
        let len = usize::try_from(u64::from_le_bytes(word)).ok()?;
        let bytes = input.bytes(len)?;
        if input.read(&mut first)? != 0 {
            return None;
        }
        bytes
    };
    // Reuse the format parser's routing for renamed plugin IDs without copying
    // either custom-state lane into another whole-envelope buffer.
    Some(StreamState::Envelope(StateParse::WrongPlugin {
        found: plugin_id,
        state: DeserializedState {
            params,
            extra,
            persist,
        },
    }))
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
                n as i64
            },
            limit,
        )
    }
    #[test]
    fn short_reads_large_lanes_and_legacy_are_lossless() {
        let persist = vec![173; 33 * 1024 * 1024];
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
        assert!(parse(&blob, 8192, 32 * 1024 * 1024).is_none());
        assert!(matches!(parse(b"old",1,32),Some(StreamState::Foreign(bytes)) if bytes==b"old"));
    }
    #[test]
    fn malformed_and_overreported_streams_are_rejected() {
        assert!(read_state_stream(|_| -1, 1024).is_none());
        assert!(read_state_stream(|dst| dst.len() as i64 + 1, 1024).is_none());
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
