//! Repairs for ISO-BMFF (MP4/M4A) container defects.

use super::Patch;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// MP4/M4A extensions that route to symphonia's isomp4 demuxer and can carry a
/// truncated `mdat`. Raw AAC (`.aac`) is ADTS, not an ISO-BMFF container.
pub(super) const EXTENSIONS: &[&str] = &["m4a", "m4b", "m4p", "mp4", "mov"];

/// Scan the top-level atoms of an MP4/M4A file for an `mdat` box whose declared
/// size overruns the end of the file (an interrupted download or copy), and
/// return the byte patches that rewrite its size field to the number of bytes
/// actually present. Symphonia's isomp4 demuxer discards such an mdat wholesale
/// and then fails with "no atom pending read", even though the audio that IS
/// present decodes fine; with the size repaired it hits a clean EOF at the
/// truncation point instead.
///
/// Returns an empty vec for well-formed files. This only walks the small
/// top-level atom headers, not the media data. The file cursor is left where
/// the scan stopped; the caller is responsible for rewinding.
pub(super) fn mdat_size_patches(file: &File, file_len: u64) -> Vec<Patch> {
    let mut f = file;
    let mut pos: u64 = 0;
    let mut hdr = [0u8; 16];
    while pos + 8 <= file_len {
        if f.seek(SeekFrom::Start(pos)).is_err() || f.read_exact(&mut hdr[..8]).is_err() {
            break;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let atom_type = [hdr[4], hdr[5], hdr[6], hdr[7]];

        // Resolve the atom size and where/how wide its size field is.
        let (atom_size, size_off, size_is_64) = if size32 == 1 {
            // 64-bit size stored immediately after the type field.
            if f.read_exact(&mut hdr[8..16]).is_err() {
                break;
            }
            let size64 = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
            (size64, pos + 8, true)
        } else if size32 == 0 {
            // Size 0 already means "extends to end of file", and nothing can
            // follow it — no repair needed or possible.
            break;
        } else {
            (size32 as u64, pos, false)
        };

        if atom_size < 8 {
            break; // malformed header; give up rather than loop
        }

        let atom_end = pos.saturating_add(atom_size);

        if atom_type == *b"mdat" && atom_end > file_len {
            // Rewrite the size field so the box ends exactly at EOF.
            let corrected = file_len - pos;
            let bytes_at = |slice: &[u8]| -> Vec<Patch> {
                slice
                    .iter()
                    .enumerate()
                    .map(|(i, &b)| Patch {
                        offset: size_off + i as u64,
                        value: b,
                    })
                    .collect()
            };
            return if size_is_64 {
                bytes_at(&corrected.to_be_bytes())
            } else if corrected <= u32::MAX as u64 {
                bytes_at(&(corrected as u32).to_be_bytes())
            } else {
                Vec::new() // corrected size won't fit a 32-bit field
            };
        }

        pos = atom_end;
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a file with an `ftyp` atom followed by an `mdat` atom whose
    /// 32-bit size field declares `declared_payload` bytes of media data,
    /// while only `actual_payload` bytes are written (truncation).
    fn mp4_file(declared_payload: u32, actual_payload: usize) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&16u32.to_be_bytes()).unwrap();
        f.write_all(b"ftypM4A \0\0\0\0").unwrap();
        f.write_all(&(8 + declared_payload).to_be_bytes()).unwrap();
        f.write_all(b"mdat").unwrap();
        f.write_all(&vec![0xAAu8; actual_payload]).unwrap();
        f
    }

    fn patches_for(f: &tempfile::NamedTempFile) -> Vec<Patch> {
        let file = f.reopen().unwrap();
        let len = file.metadata().unwrap().len();
        mdat_size_patches(&file, len)
    }

    #[test]
    fn well_formed_file_needs_no_patches() {
        let f = mp4_file(100, 100);
        assert!(patches_for(&f).is_empty());
    }

    #[test]
    fn truncated_mdat_size_is_rewritten_to_bytes_on_disk() {
        // mdat declares 100 payload bytes but only 40 are present.
        let f = mp4_file(100, 40);
        let patches = patches_for(&f);

        // The 32-bit size field at offset 16 must now read 8 + 40 = 48.
        let expected: Vec<Patch> = 48u32
            .to_be_bytes()
            .iter()
            .enumerate()
            .map(|(i, &b)| Patch {
                offset: 16 + i as u64,
                value: b,
            })
            .collect();
        assert_eq!(patches, expected);
    }

    #[test]
    fn truncated_64bit_mdat_size_is_rewritten() {
        // mdat with size32 == 1 and a 64-bit size that overruns the file.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&16u32.to_be_bytes()).unwrap();
        f.write_all(b"ftypM4A \0\0\0\0").unwrap();
        f.write_all(&1u32.to_be_bytes()).unwrap();
        f.write_all(b"mdat").unwrap();
        f.write_all(&1000u64.to_be_bytes()).unwrap();
        f.write_all(&[0xAAu8; 24]).unwrap();

        let patches = patches_for(&f);

        // 16 (header+size64) + 24 payload = 40 bytes remain from the mdat
        // start; the 64-bit size field lives at offset 24.
        let expected: Vec<Patch> = 40u64
            .to_be_bytes()
            .iter()
            .enumerate()
            .map(|(i, &b)| Patch {
                offset: 24 + i as u64,
                value: b,
            })
            .collect();
        assert_eq!(patches, expected);
    }
}
