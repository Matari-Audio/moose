//! Owned, immutable persistence snapshots for streaming host state output.
use std::io::{self, Write};

/// A frozen write-side snapshot, independent of the live parameter store.
///
/// Preparation and output are non-realtime operations. Implementations may
/// retain immutable `Arc` payloads instead of a second encoded copy. Every
/// invocation must emit exactly `byte_len()` bytes from the same snapshot;
/// controls must not be reread during output. This is separate from the generic
/// `PreparedPersist<P>` read/restore commit API.
pub trait PreparedPersistWrite: Send + Sync {
    fn byte_len(&self) -> u64;
    fn write_to(&self, output: &mut dyn Write) -> io::Result<()>;
}

impl PreparedPersistWrite for Vec<u8> {
    fn byte_len(&self) -> u64 {
        self.len() as u64
    }

    fn write_to(&self, output: &mut dyn Write) -> io::Result<()> {
        output.write_all(self)
    }
}

/// Write exactly the declared length, detecting a faulty snapshot without
/// allowing it to consume bytes belonging to the following keyed field.
pub fn write_prepared_persist(
    snapshot: &dyn PreparedPersistWrite,
    output: &mut dyn Write,
) -> io::Result<()> {
    write_exact(snapshot, snapshot.byte_len(), output)
}

fn write_exact(
    snapshot: &dyn PreparedPersistWrite,
    byte_len: u64,
    output: &mut dyn Write,
) -> io::Result<()> {
    let mut limited = ExactWriter {
        output,
        remaining: byte_len,
    };
    snapshot.write_to(&mut limited)?;
    if limited.remaining != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short persistence snapshot"));
    }
    Ok(())
}

struct ExactWriter<'a> {
    output: &'a mut dyn Write,
    remaining: u64,
}

impl Write for ExactWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized persistence snapshot"));
        }
        let written = self.output.write(bytes)?;
        self.remaining -= written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

/// Derive support for the existing count/key/u32-length persistence format.
/// Owns only field snapshots and their small framing, never a combined blob.
#[doc(hidden)]
pub struct KeyedPersistWrite {
    fields: Vec<KeyedField>,
    count: u32,
    byte_len: u64,
}

struct KeyedField {
    key: &'static str,
    key_len: u32,
    byte_len: u32,
    snapshot: Box<dyn PreparedPersistWrite>,
}

impl KeyedPersistWrite {
    pub fn new(fields: Vec<(&'static str, Box<dyn PreparedPersistWrite>)>) -> io::Result<Self> {
        let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "persistence framing exceeds u32");
        let count = u32::try_from(fields.len()).map_err(|_| invalid())?;
        let mut byte_len = 4_u64;
        let mut prepared = Vec::with_capacity(fields.len());
        for (key, snapshot) in fields {
            let key_len = u32::try_from(key.len()).map_err(|_| invalid())?;
            let field_len = u32::try_from(snapshot.byte_len()).map_err(|_| invalid())?;
            byte_len = byte_len.checked_add(8 + u64::from(key_len))
                .and_then(|len| len.checked_add(u64::from(field_len)))
                .ok_or_else(invalid)?;
            prepared.push(KeyedField { key, key_len, byte_len: field_len, snapshot });
        }
        Ok(Self { fields: prepared, count, byte_len })
    }
}

impl PreparedPersistWrite for KeyedPersistWrite {
    fn byte_len(&self) -> u64 {
        self.byte_len
    }

    fn write_to(&self, output: &mut dyn Write) -> io::Result<()> {
        // Preserve the old wire format, including an empty nested store's
        // zero-length field value. Lengths were checked and frozen at prepare.
        output.write_all(&self.count.to_le_bytes())?;
        for field in &self.fields {
            output.write_all(&field.key_len.to_le_bytes())?;
            output.write_all(field.key.as_bytes())?;
            output.write_all(&field.byte_len.to_le_bytes())?;
            write_exact(field.snapshot.as_ref(), u64::from(field.byte_len), output)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Declared {
        len: u64,
        bytes: &'static [u8],
    }

    impl PreparedPersistWrite for Declared {
        fn byte_len(&self) -> u64 { self.len }
        fn write_to(&self, output: &mut dyn Write) -> io::Result<()> {
            output.write_all(self.bytes)
        }
    }

    #[test]
    fn rejects_unrepresentable_field_before_writing() {
        let snapshot = Declared { len: u64::from(u32::MAX) + 1, bytes: &[] };
        assert!(KeyedPersistWrite::new(vec![("huge", Box::new(snapshot))]).is_err());
    }

    #[test]
    fn rejects_under_and_overreported_lengths() {
        for len in [0, 2] {
            let snapshot = Declared { len, bytes: &[1] };
            let mut output = Vec::new();
            assert_eq!(write_prepared_persist(&snapshot, &mut output).unwrap_err().kind(),
                io::ErrorKind::InvalidData);
            if len == 0 { assert!(output.is_empty()); }
        }
    }

    #[test]
    fn supports_partial_writes_and_propagates_host_failure() {
        struct Partial { bytes: Vec<u8>, fail_at: usize }
        impl Write for Partial {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.bytes.len() == self.fail_at {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "host stopped"));
                }
                let len = bytes.len().min(1);
                self.bytes.extend_from_slice(&bytes[..len]);
                Ok(len)
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let snapshot = vec![1, 2, 3];
        let mut output = Partial { bytes: Vec::new(), fail_at: usize::MAX };
        write_prepared_persist(&snapshot, &mut output).unwrap();
        assert_eq!(output.bytes, snapshot);
        let mut output = Partial { bytes: Vec::new(), fail_at: 1 };
        assert_eq!(write_prepared_persist(&snapshot, &mut output).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe);
        assert_eq!(output.bytes, [1]);
    }
}
