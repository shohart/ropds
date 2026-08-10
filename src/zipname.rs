//! ZIP entry name decoding.
//!
//! Archives produced by DOS/Windows tools store entry names in an OEM codepage
//! without setting the UTF-8 flag (general purpose bit 11). APPNOTE declares
//! such names to be CP437, which is what the `zip` crate decodes them as — so
//! a Cyrillic name written in CP866 comes back as mojibake. `library.zip_codepage`
//! names the codepage to use instead.

use std::io::{Read, Seek};
use std::sync::OnceLock;

use encoding_rs::Encoding;
use zip::ZipArchive;
use zip::read::HasZipMetadata;

/// Resolve `library.zip_codepage` to an encoding.
///
/// Returns `None` when the `zip` crate's own decoding should stand: CP437
/// itself (which the crate already applies, and for which `encoding_rs` has no
/// label), or an unrecognised label.
fn lookup(codepage: &str) -> Option<&'static Encoding> {
    let label = codepage.trim();
    let is_cp437 = label.eq_ignore_ascii_case("cp437") || label.eq_ignore_ascii_case("ibm437");
    if label.is_empty() || is_cp437 {
        return None;
    }
    let encoding = Encoding::for_label(label.as_bytes());
    if encoding.is_none() {
        static WARNED: OnceLock<()> = OnceLock::new();
        WARNED.get_or_init(|| {
            tracing::warn!(
                "Unknown library.zip_codepage '{label}'; ZIP entry names will be read as CP437"
            );
        });
    }
    encoding
}

/// Decode a raw ZIP entry name with `codepage`.
///
/// Returns `None` when the `zip` crate's own decoding already applies: the
/// entry is UTF-8 flagged, the name is pure ASCII (where every supported
/// codepage agrees with CP437), or the codepage is CP437/unrecognised.
pub fn decode(raw: &[u8], is_utf8: bool, codepage: &str) -> Option<String> {
    if is_utf8 || raw.is_ascii() {
        return None;
    }
    let (decoded, had_errors) = lookup(codepage)?.decode_without_bom_handling(raw);
    if had_errors {
        return None;
    }
    Some(decoded.into_owned())
}

/// Decode the name of an entry, falling back to the `zip` crate's decoding.
pub fn entry_name<R: Read>(entry: &zip::read::ZipFile<'_, R>, codepage: &str) -> String {
    decode(entry.name_raw(), entry.get_metadata().is_utf8, codepage)
        .unwrap_or_else(|| entry.name().to_string())
}

/// The last path component of a ZIP entry name.
fn basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

/// True when `name` identifies the entry the database refers to as `wanted`.
///
/// The scanner stores an entry's basename, so a nested entry matches on its
/// last path component as well as on its full name.
fn matches(name: &str, wanted: &str) -> bool {
    name == wanted || basename(name) == wanted
}

/// Locate the entry matching the filename stored in the database.
///
/// `ZipArchive::by_name` cannot be used here: it keys on the *raw* name bytes,
/// so a name that had to be decoded never matches.
pub fn find_entry_index<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    wanted: &str,
    codepage: &str,
) -> Option<usize> {
    // Fast path: raw bytes are the name, i.e. a genuinely UTF-8 archive.
    if let Some(index) = archive.index_for_name(wanted) {
        return Some(index);
    }
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(index) else {
            continue;
        };
        if matches(&entry_name(&entry, codepage), wanted) {
            return Some(index);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Стругацкий - Страна багровых туч.fb2` as stored by a DOS/Windows
    /// archiver in CP866.
    const CP866_NAME: &[u8] = &[
        0x91, 0xe2, 0xe0, 0xe3, 0xa3, 0xa0, 0xe6, 0xaa, 0xa8, 0xa9, 0x20, 0x2d, 0x20, 0x91, 0xe2,
        0xe0, 0xa0, 0xad, 0xa0, 0x20, 0xa1, 0xa0, 0xa3, 0xe0, 0xae, 0xa2, 0xeb, 0xe5, 0x20, 0xe2,
        0xe3, 0xe7, 0x2e, 0x66, 0x62, 0x32,
    ];
    /// The decoded name those bytes stand for.
    const UTF8_NAME: &str = "Стругацкий - Страна багровых туч.fb2";

    #[test]
    fn decodes_cp866_entry_name() {
        assert_eq!(decode(CP866_NAME, false, "cp866").as_deref(), Some(UTF8_NAME));
    }

    #[test]
    fn keeps_crate_decoding_for_utf8_and_ascii_names() {
        // UTF-8 flagged names are already correct.
        assert_eq!(decode(UTF8_NAME.as_bytes(), true, "cp866"), None);
        // ASCII agrees across codepages.
        assert_eq!(decode(b"book.fb2", false, "cp866"), None);
    }

    #[test]
    fn keeps_crate_decoding_for_cp437_and_unknown_labels() {
        assert_eq!(decode(CP866_NAME, false, "cp437"), None);
        assert_eq!(decode(CP866_NAME, false, "not-a-codepage"), None);
        assert_eq!(decode(CP866_NAME, false, ""), None);
    }

    #[test]
    fn decodes_other_common_codepages() {
        // windows-1251 "Страна.fb2"
        let cp1251 = &[0xd1, 0xf2, 0xf0, 0xe0, 0xed, 0xe0, 0x2e, 0x66, 0x62, 0x32];
        assert_eq!(
            decode(cp1251, false, "windows-1251").as_deref(),
            Some("Страна.fb2")
        );
        assert_eq!(
            decode(cp1251, false, "cp1251").as_deref(),
            Some("Страна.fb2")
        );
    }

    /// Build an in-memory ZIP with one entry whose name is raw `bytes`.
    fn zip_with_raw_name(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        // The `zip` writer always encodes names as UTF-8, so craft the archive
        // by hand to get a non-UTF-8 name with the language-encoding flag clear.
        let mut buf = Vec::new();
        let data = b"content";
        let crc = crc32fast::hash(data);
        let name_len = bytes.len() as u16;

        // Local file header
        buf.write_all(&0x0403_4b50u32.to_le_bytes()).unwrap();
        buf.write_all(&[10, 0, 0, 0, 0, 0]).unwrap(); // version, flags (bit 11 clear), method=stored
        buf.write_all(&[0, 0, 0, 0]).unwrap(); // mod time/date
        buf.write_all(&crc.to_le_bytes()).unwrap();
        buf.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
        buf.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
        buf.write_all(&name_len.to_le_bytes()).unwrap();
        buf.write_all(&0u16.to_le_bytes()).unwrap(); // extra len
        buf.write_all(bytes).unwrap();
        buf.write_all(data).unwrap();

        // Central directory
        let cd_offset = buf.len() as u32;
        buf.write_all(&0x0201_4b50u32.to_le_bytes()).unwrap();
        buf.write_all(&[10, 0, 10, 0, 0, 0, 0, 0]).unwrap(); // versions, flags, method
        buf.write_all(&[0, 0, 0, 0]).unwrap(); // mod time/date
        buf.write_all(&crc.to_le_bytes()).unwrap();
        buf.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
        buf.write_all(&(data.len() as u32).to_le_bytes()).unwrap();
        buf.write_all(&name_len.to_le_bytes()).unwrap();
        buf.write_all(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap(); // extra/comment/disk/attrs
        buf.write_all(&0u32.to_le_bytes()).unwrap(); // local header offset
        buf.write_all(bytes).unwrap();
        let cd_size = buf.len() as u32 - cd_offset;

        // End of central directory
        buf.write_all(&0x0605_4b50u32.to_le_bytes()).unwrap();
        buf.write_all(&[0, 0, 0, 0]).unwrap(); // disk numbers
        buf.write_all(&1u16.to_le_bytes()).unwrap();
        buf.write_all(&1u16.to_le_bytes()).unwrap();
        buf.write_all(&cd_size.to_le_bytes()).unwrap();
        buf.write_all(&cd_offset.to_le_bytes()).unwrap();
        buf.write_all(&0u16.to_le_bytes()).unwrap(); // comment len
        buf
    }

    #[test]
    fn finds_entry_by_decoded_name() {
        let raw = zip_with_raw_name(CP866_NAME);
        let mut archive = ZipArchive::new(std::io::Cursor::new(raw)).unwrap();
        assert_eq!(
            find_entry_index(&mut archive, UTF8_NAME, "cp866"),
            Some(0)
        );
    }

    #[test]
    fn finds_nested_entry_by_basename() {
        let mut nested = b"books/".to_vec();
        nested.extend_from_slice(CP866_NAME);
        let raw = zip_with_raw_name(&nested);
        let mut archive = ZipArchive::new(std::io::Cursor::new(raw)).unwrap();
        assert_eq!(
            find_entry_index(&mut archive, UTF8_NAME, "cp866"),
            Some(0)
        );
    }

    #[test]
    fn missing_entry_returns_none() {
        let raw = zip_with_raw_name(CP866_NAME);
        let mut archive = ZipArchive::new(std::io::Cursor::new(raw)).unwrap();
        assert_eq!(find_entry_index(&mut archive, "absent.fb2", "cp866"), None);
    }
}
