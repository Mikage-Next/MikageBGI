//! BGIPatch "PAKZ" fan-translation packages.
//!
//! A number of Simplified-Chinese BGI fan patches (the "BGIPatch" framework:
//! a loader exe injects a hook DLL alongside the stock game) ship a `.pak`
//! that the hook substitutes for the game's own resources at load time,
//! rather than replacing the `.arc` files. The Chinese text lives in no game
//! archive, which is why these patches can't be loaded by overriding archives.
//!
//! The container is simple enough to reproduce the substitution natively, so
//! the engine renders the translation without the Windows hook DLL:
//!
//! ```text
//! "PAKZ"  u32 count
//! count × record, each 0x10C bytes:
//!     [name: 256 bytes, NUL-padded]
//!     [u32 offset]          absolute file offset of the zstd frame
//!     [u32 decoded_size]    size after decompression
//!     [u32 encoded_size]    length of the zstd frame
//! ```
//!
//! Each frame decompresses to one decoded resource, keyed by the engine's
//! resource name (bare scenario-script names such as `s01_01b`, translated
//! image names, …). We register that mapping and let `read_decoded` return the
//! patched resource in place of the archive's original.

use crate::normalize_resource_name;
use ethornell_core::{EthornellError, Result};
use std::collections::HashMap;
use std::io::Read;

const PAKZ_MAGIC: &[u8; 4] = b"PAKZ";
const RECORD_NAME_LEN: usize = 0x100;
const RECORD_LEN: usize = RECORD_NAME_LEN + 12;
/// A fan patch carrying more entries than this is treated as malformed; the
/// real packages hold on the order of a thousand resources.
const MAX_ENTRIES: usize = 1 << 20;

#[derive(Debug, Clone, Copy)]
struct Entry {
    offset: usize,
    decoded_size: usize,
    encoded_size: usize,
}

/// A parsed BGIPatch package held in memory, decompressing entries on demand.
#[derive(Debug)]
pub struct PatchPackage {
    bytes: Vec<u8>,
    entries: HashMap<String, Entry>,
    max_decoded_bytes: usize,
}

impl PatchPackage {
    /// Parses a PAKZ package. Returns `Ok(None)` when the bytes are not a PAKZ
    /// container, so a caller can hand any candidate file here harmlessly.
    /// Entries whose frame falls outside the file, or whose decoded size
    /// exceeds `max_decoded_bytes`, are dropped rather than failing the parse.
    pub fn parse(bytes: Vec<u8>, max_decoded_bytes: usize) -> Result<Option<Self>> {
        if bytes.len() < 8 || &bytes[..4] != PAKZ_MAGIC {
            return Ok(None);
        }
        let count = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        if count > MAX_ENTRIES {
            return Err(EthornellError::Parse(format!(
                "BGIPatch package declares too many entries: {count}"
            )));
        }
        let index_end = 8usize
            .checked_add(count.checked_mul(RECORD_LEN).ok_or_else(|| {
                EthornellError::Parse("BGIPatch package index size overflows".into())
            })?)
            .ok_or_else(|| EthornellError::Parse("BGIPatch package index size overflows".into()))?;
        if index_end > bytes.len() {
            return Err(EthornellError::Parse(
                "BGIPatch package index is truncated".into(),
            ));
        }
        let mut entries = HashMap::new();
        for i in 0..count {
            let base = 8 + i * RECORD_LEN;
            let name_field = &bytes[base..base + RECORD_NAME_LEN];
            let name_len = name_field
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(RECORD_NAME_LEN);
            if name_len == 0 {
                continue;
            }
            let name = String::from_utf8_lossy(&name_field[..name_len]).into_owned();
            let fields = base + RECORD_NAME_LEN;
            let offset = u32::from_le_bytes(bytes[fields..fields + 4].try_into().unwrap()) as usize;
            let decoded_size =
                u32::from_le_bytes(bytes[fields + 4..fields + 8].try_into().unwrap()) as usize;
            let encoded_size =
                u32::from_le_bytes(bytes[fields + 8..fields + 12].try_into().unwrap()) as usize;
            // Drop entries we can't serve rather than rejecting the package: a
            // patch may legitimately pad its table, and an over-budget resource
            // should simply fall through to the game's own copy.
            if encoded_size == 0
                || offset < index_end
                || offset.checked_add(encoded_size).is_none_or(|end| end > bytes.len())
                || decoded_size > max_decoded_bytes
            {
                continue;
            }
            entries.insert(
                normalize_resource_name(&name),
                Entry {
                    offset,
                    decoded_size,
                    encoded_size,
                },
            );
        }
        Ok(Some(Self {
            bytes,
            entries,
            max_decoded_bytes,
        }))
    }

    /// Whether the package overrides this resource name.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(&normalize_resource_name(name))
    }

    /// The number of resources the package overrides.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The decoded (decompressed) bytes for an overridden resource, or `None`
    /// when the package does not carry it.
    pub fn read_decoded(&self, name: &str) -> Option<Result<Vec<u8>>> {
        let entry = *self.entries.get(&normalize_resource_name(name))?;
        Some(self.decode(&entry))
    }

    fn decode(&self, entry: &Entry) -> Result<Vec<u8>> {
        let frame = &self.bytes[entry.offset..entry.offset + entry.encoded_size];
        let decoder = ruzstd::StreamingDecoder::new(frame)
            .map_err(|err| EthornellError::Parse(format!("BGIPatch zstd frame invalid: {err}")))?;
        // Cap the decode so a crafted frame cannot exhaust memory; the real
        // decoded size is recorded in the index and bounded at parse time.
        let cap = self.max_decoded_bytes.saturating_add(1);
        let mut decoded = Vec::new();
        decoder
            .take(cap as u64)
            .read_to_end(&mut decoded)
            .map_err(|err| EthornellError::Parse(format!("BGIPatch zstd decode failed: {err}")))?;
        if decoded.len() > self.max_decoded_bytes {
            return Err(EthornellError::Parse(
                "BGIPatch resource exceeds the decoded-size budget".into(),
            ));
        }
        if decoded.len() != entry.decoded_size {
            return Err(EthornellError::Parse(format!(
                "BGIPatch resource decoded to {} bytes, expected {}",
                decoded.len(),
                entry.decoded_size
            )));
        }
        Ok(decoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A zstd frame for `PAYLOAD`, produced by a reference zstd encoder; ruzstd
    // is decode-only, so the fixture is embedded rather than compressed here.
    const PAYLOAD: &[u8] = b"BurikoCompiledScriptVer1.00 patched hello";
    const ZSTD_FRAME: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x29, 0x49, 0x01, 0x00, 0x42, 0x75, 0x72, 0x69, 0x6b, 0x6f,
        0x43, 0x6f, 0x6d, 0x70, 0x69, 0x6c, 0x65, 0x64, 0x53, 0x63, 0x72, 0x69, 0x70, 0x74, 0x56,
        0x65, 0x72, 0x31, 0x2e, 0x30, 0x30, 0x20, 0x70, 0x61, 0x74, 0x63, 0x68, 0x65, 0x64, 0x20,
        0x68, 0x65, 0x6c, 0x6c, 0x6f,
    ];

    fn build_package(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(PAKZ_MAGIC);
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let index_end = 8 + entries.len() * RECORD_LEN;
        // Lay the frames out after the index, recording each location.
        let mut frames = Vec::new();
        let mut cursor = index_end;
        for (name, frame) in entries {
            let mut name_field = vec![0u8; RECORD_NAME_LEN];
            name_field[..name.len()].copy_from_slice(name.as_bytes());
            out.extend_from_slice(&name_field);
            out.extend_from_slice(&(cursor as u32).to_le_bytes());
            out.extend_from_slice(&(PAYLOAD.len() as u32).to_le_bytes());
            out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            frames.push((cursor, *frame));
            cursor += frame.len();
        }
        for (at, frame) in frames {
            debug_assert_eq!(out.len(), at);
            out.extend_from_slice(frame);
        }
        out
    }

    #[test]
    fn parses_and_decompresses_an_override() {
        let bytes = build_package(&[("s01_01b", ZSTD_FRAME)]);
        let package = PatchPackage::parse(bytes, 1 << 20).unwrap().unwrap();
        assert_eq!(package.len(), 1);
        assert!(package.contains("S01_01B")); // name match is case-insensitive
        let decoded = package.read_decoded("s01_01b").unwrap().unwrap();
        assert_eq!(decoded, PAYLOAD);
    }

    #[test]
    fn non_pakz_bytes_are_not_a_package() {
        assert!(PatchPackage::parse(b"not a pakz".to_vec(), 1 << 20)
            .unwrap()
            .is_none());
    }

    #[test]
    fn missing_name_is_not_overridden() {
        let bytes = build_package(&[("s01_01b", ZSTD_FRAME)]);
        let package = PatchPackage::parse(bytes, 1 << 20).unwrap().unwrap();
        assert!(!package.contains("s99_99"));
        assert!(package.read_decoded("s99_99").is_none());
    }

    #[test]
    fn an_over_budget_entry_is_dropped() {
        let bytes = build_package(&[("s01_01b", ZSTD_FRAME)]);
        // Budget below the recorded decoded size: the entry is not registered.
        let package = PatchPackage::parse(bytes, PAYLOAD.len() - 1)
            .unwrap()
            .unwrap();
        assert!(package.is_empty());
    }

    // Validates the parser against a real BGIPatch package when one is supplied
    // via ETHORNELL_BGIPATCH_PAK; the fan-patch file itself is never committed.
    #[test]
    #[ignore = "requires ETHORNELL_BGIPATCH_PAK pointing at a real .pak"]
    fn reads_a_real_bgipatch_package() {
        let path = std::env::var_os("ETHORNELL_BGIPATCH_PAK").expect("package path");
        let bytes = std::fs::read(path).expect("read package");
        let package = PatchPackage::parse(bytes, 64 << 20)
            .expect("parse")
            .expect("is a PAKZ package");
        eprintln!("BGIPatch package overrides {} resources", package.len());
        assert!(package.len() > 100, "expected a populated package");
        // Scenario scripts are the translated payload; one decoded blob should
        // be a compiled Buriko script carrying non-ASCII (Chinese) text.
        let (name, decoded) = package
            .entries
            .keys()
            .find_map(|name| {
                let decoded = package.read_decoded(name)?.ok()?;
                decoded
                    .starts_with(b"BurikoCompiledScript")
                    .then_some((name.clone(), decoded))
            })
            .expect("a compiled script entry");
        eprintln!("decoded {name}: {} bytes", decoded.len());
        assert!(
            decoded.iter().any(|&b| b >= 0x80),
            "patched script should carry non-ASCII translated text"
        );
    }
}
