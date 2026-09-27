//! Preserve distinct static archive members while removing exact duplicates.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Self-cleaning wrapper around the deduplicated archive. Holds the
/// temp dir we extracted into so the cleanup happens after clang
/// finishes consuming the archive.
pub(super) struct DedupedArchive {
    archive_path: PathBuf,
    temp_dir: PathBuf,
}

impl DedupedArchive {
    pub(super) fn path(&self) -> &Path {
        &self.archive_path
    }
}

impl Drop for DedupedArchive {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

/// Extract `staticlib`'s members into a temp directory then recompose
/// a fresh archive with byte-identical duplicate members removed.
///
/// Why this exists: some sys-crate chains (skia-bindings carrying a
/// full harfbuzz inside libskia.a is the live example) end up bundled
/// into the rustc-emitted staticlib *twice* - once via the depending
/// rlib's embedded native archive and once via the staticlib's own
/// native-dep pass. Those duplicate members are byte-identical and can
/// be dropped before `-all_load`.
///
/// Some native archives also contain multiple different object files with
/// the same member name. A plain `ar -x` overwrites those on extraction,
/// which silently drops required objects. We therefore read the archive
/// directly, drop only exact byte duplicates, and give preserved members
/// unique filesystem names before recomposing the archive.
pub(super) fn dedupe_archive_members(
    staticlib: &Path,
) -> Result<DedupedArchive, crate::CargoMooseError> {
    let parent = staticlib
        .parent()
        .ok_or_else(|| -> crate::CargoMooseError {
            format!(
                "staticlib path has no parent directory: {}",
                staticlib.display()
            )
            .into()
        })?;
    let stem = staticlib
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("staticlib");
    // Append PID so concurrent installs (rare but possible: `cargo
    // moose install -p A -p B` against the same workspace) don't
    // collide on the same temp dir.
    let temp_dir = parent.join(format!("{stem}.dedup-{}", std::process::id()));
    if temp_dir.exists() {
        let _ = std::fs::remove_dir_all(&temp_dir);
    }
    std::fs::create_dir_all(&temp_dir)?;

    let mut members: Vec<PathBuf> = Vec::new();
    let mut seen: HashMap<String, Vec<SeenArchiveMember>> = HashMap::new();
    for member in archive_members(staticlib)? {
        if member.name.is_empty() || member.name.starts_with("__.SYMDEF") || member.name == "/" {
            continue;
        }

        let len = member.bytes.len() as u64;
        let hash = member_hash(&member.bytes);
        let entry = seen.entry(member.name.clone()).or_default();
        let duplicate = entry.iter().any(|existing| {
            existing.len == len
                && existing.hash == hash
                && std::fs::read(&existing.path).is_ok_and(|bytes| bytes == member.bytes)
        });
        if duplicate {
            continue;
        }

        let unique = temp_dir.join(format!(
            "{:06}_{}",
            members.len(),
            archive_member_filename(&member.name)
        ));
        std::fs::write(&unique, &member.bytes)?;
        entry.push(SeenArchiveMember {
            len,
            hash,
            path: unique.clone(),
        });
        members.push(unique);
    }
    if members.is_empty() {
        return Err(format!(
            "archive dedupe found no object members in {} - archive may be empty or corrupt",
            staticlib.display()
        )
        .into());
    }

    let archive_path = temp_dir.join("deduplicated.a");
    if archive_path.exists() {
        let _ = std::fs::remove_file(&archive_path);
    }
    // `-rcs`: replace (`-r`) + create-if-missing (`-c`) + write symbol
    // table (`-s`). One pass; no separate `ranlib` step.
    let mut compose = Command::new("ar");
    compose.arg("-rcs").arg(&archive_path);
    for m in &members {
        compose.arg(m);
    }
    let composed = compose.output().map_err(|e| -> crate::CargoMooseError {
        format!("invoking ar -rcs for archive dedupe: {e}").into()
    })?;
    if !composed.status.success() {
        return Err(format!(
            "ar -rcs failed for {}:\n{}",
            archive_path.display(),
            String::from_utf8_lossy(&composed.stderr),
        )
        .into());
    }

    Ok(DedupedArchive {
        archive_path,
        temp_dir,
    })
}

struct SeenArchiveMember {
    len: u64,
    hash: u64,
    path: PathBuf,
}

fn member_hash(bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn archive_member_filename(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' => '_',
            ch => ch,
        })
        .collect()
}

struct ArchiveMember {
    name: String,
    bytes: Vec<u8>,
}

fn archive_members(archive: &Path) -> Result<Vec<ArchiveMember>, crate::CargoMooseError> {
    const MAGIC: &[u8] = b"!<arch>\n";
    const HEADER_LEN: usize = 60;

    let data = std::fs::read(archive)?;
    if !data.starts_with(MAGIC) {
        return Err(format!("{} is not an ar archive", archive.display()).into());
    }

    let mut members = Vec::new();
    let mut gnu_names: Option<Vec<u8>> = None;
    let mut offset = MAGIC.len();
    while offset + HEADER_LEN <= data.len() {
        let header = &data[offset..offset + HEADER_LEN];
        if &header[58..60] != b"`\n" {
            return Err(format!(
                "{} has an invalid ar header at byte {offset}",
                archive.display()
            )
            .into());
        }

        let raw_name = ascii_field(&header[0..16]);
        let size = ascii_field(&header[48..58]).parse::<usize>().map_err(
            |e| -> crate::CargoMooseError {
                format!(
                    "{} has an invalid ar member size at byte {offset}: {e}",
                    archive.display()
                )
                .into()
            },
        )?;
        let payload_start = offset + HEADER_LEN;
        let payload_end =
            payload_start
                .checked_add(size)
                .ok_or_else(|| -> crate::CargoMooseError {
                    format!("{} has an overflowing ar member size", archive.display()).into()
                })?;
        if payload_end > data.len() {
            return Err(format!(
                "{} has a truncated ar member at byte {offset}",
                archive.display()
            )
            .into());
        }

        let payload = &data[payload_start..payload_end];
        let (name, bytes) = parse_archive_member(&raw_name, payload, gnu_names.as_deref())?;
        if raw_name == "//" {
            gnu_names = Some(payload.to_vec());
        } else {
            members.push(ArchiveMember { name, bytes });
        }

        offset = payload_end + (payload_end % 2);
    }

    if offset != data.len() {
        return Err(format!(
            "{} has trailing bytes after the final ar member",
            archive.display()
        )
        .into());
    }

    Ok(members)
}

fn parse_archive_member(
    raw_name: &str,
    payload: &[u8],
    gnu_names: Option<&[u8]>,
) -> Result<(String, Vec<u8>), crate::CargoMooseError> {
    if let Some(name_len) = raw_name.strip_prefix("#1/") {
        let name_len = name_len
            .parse::<usize>()
            .map_err(|e| -> crate::CargoMooseError {
                format!("invalid BSD ar extended-name length `{name_len}`: {e}").into()
            })?;
        if name_len > payload.len() {
            return Err(format!(
                "BSD ar extended-name length {name_len} exceeds member payload size {}",
                payload.len()
            )
            .into());
        }
        let name = archive_member_name(&String::from_utf8_lossy(&payload[..name_len]));
        return Ok((name, payload[name_len..].to_vec()));
    }

    if let Some(offset) = raw_name.strip_prefix('/')
        && raw_name != "/"
        && raw_name != "//"
        && offset.chars().all(|ch| ch.is_ascii_digit())
    {
        let offset = offset
            .parse::<usize>()
            .map_err(|e| -> crate::CargoMooseError {
                format!("invalid GNU ar long-name offset `{offset}`: {e}").into()
            })?;
        let names = gnu_names.ok_or_else(|| -> crate::CargoMooseError {
            "GNU ar long-name member appeared before the string table".into()
        })?;
        if offset >= names.len() {
            return Err(format!(
                "GNU ar long-name offset {offset} exceeds string table size {}",
                names.len()
            )
            .into());
        }
        let end = names[offset..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(names.len(), |idx| offset + idx);
        let name = archive_member_name(&String::from_utf8_lossy(&names[offset..end]));
        return Ok((name, payload.to_vec()));
    }

    Ok((archive_member_name(raw_name), payload.to_vec()))
}

fn ascii_field(field: &[u8]) -> String {
    String::from_utf8_lossy(field)
        .trim_matches(|ch| ch == ' ' || ch == '\0')
        .to_string()
}

fn archive_member_name(name: &str) -> String {
    name.trim_end_matches(['\0', '/']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn member(out: &mut Vec<u8>, name: &str, bytes: &[u8]) {
        out.extend_from_slice(
            format!(
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                0,
                "100644",
                bytes.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(bytes);
        if !bytes.len().is_multiple_of(2) {
            out.push(b'\n');
        }
    }
    #[test]
    fn dedupe_preserves_distinct_same_name_objects_and_long_names() {
        let dir = std::env::temp_dir().join(format!("moose-archive-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("input.a");
        let mut bytes = b"!<arch>\n".to_vec();
        member(&mut bytes, "same.o/", b"first");
        member(&mut bytes, "same.o/", b"second");
        member(&mut bytes, "same.o/", b"first");
        member(&mut bytes, "#1/8", b"long.o\0\0bsd");
        member(&mut bytes, "//", b"gnu-long-name.o/\n");
        member(&mut bytes, "/0", b"gnu");
        std::fs::write(&input, bytes).unwrap();
        let deduped = dedupe_archive_members(&input).unwrap();
        let mut payloads: Vec<_> = archive_members(deduped.path())
            .unwrap()
            .into_iter()
            .filter(|m| !m.name.is_empty() && !m.name.starts_with("__.SYMDEF"))
            .map(|m| m.bytes)
            .collect();
        payloads.sort();
        assert_eq!(
            payloads,
            [
                b"bsd".to_vec(),
                b"first".to_vec(),
                b"gnu".to_vec(),
                b"second".to_vec()
            ]
        );
        let path = deduped.path().to_owned();
        drop(deduped);
        assert!(!path.exists());
        std::fs::write(&input, b"!<arch>\ntruncated").unwrap();
        assert!(archive_members(&input).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
