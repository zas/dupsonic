//! Detection and in-flight repair of malformed audio container headers.
//!
//! Some audio files carry container-level defects (e.g. a truncated MP4 whose
//! `mdat` box size overruns the real end of the file) that make symphonia
//! reject them even though the audio present decodes fine. Each format-specific
//! submodule detects one class of defect and describes the fix as byte
//! [`Patch`]es; [`PatchedSource`] overlays those patches onto the file as it is
//! read, so the file on disk is never modified.
//!
//! To support a new format repair, add a submodule and a match arm in
//! [`detect_repairs`].

mod mp4;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use symphonia::core::io::MediaSource;

/// A single-byte overlay: when reading, the byte at `offset` is replaced by
/// `value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patch {
    /// Absolute file offset of the byte to replace.
    pub offset: u64,
    /// Replacement byte.
    pub value: u8,
}

/// Scan `file` (whose format is chosen by `path`'s extension) for known
/// container defects and return the byte patches that repair them.
///
/// Returns an empty vec for well-formed files (the common case), so the caller
/// decodes the original file untouched. Only small header regions are read,
/// not the media data, and the file cursor is rewound to the start before
/// returning.
pub fn detect_repairs(file: &File, path: &Path, file_len: u64) -> Vec<Patch> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    let patches = match ext.as_deref() {
        Some(e) if mp4::EXTENSIONS.contains(&e) => mp4::mdat_size_patches(file, file_len),
        _ => Vec::new(),
    };

    // The scan moves the shared file cursor; rewind so the caller reads from
    // the top. If this fails, the caller's own reads will surface the error.
    let mut f = file;
    let _ = f.seek(SeekFrom::Start(0));

    patches
}

/// A [`MediaSource`] that overlays a handful of single-byte [`Patch`]es onto a
/// file as it is read, used to repair a malformed container header (see
/// [`detect_repairs`]). All other bytes pass through unchanged.
pub struct PatchedSource {
    inner: File,
    len: u64,
    patches: Vec<Patch>,
}

impl PatchedSource {
    /// Wrap `inner` (of total size `len`) so that reads see `patches` applied.
    pub fn new(inner: File, len: u64, patches: Vec<Patch>) -> Self {
        Self {
            inner,
            len,
            patches,
        }
    }
}

impl Read for PatchedSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = self.inner.stream_position()?;
        let n = self.inner.read(buf)?;
        let end = start + n as u64;
        for &Patch { offset, value } in &self.patches {
            if offset >= start && offset < end {
                buf[(offset - start) as usize] = value;
            }
        }
        Ok(n)
    }
}

impl Seek for PatchedSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for PatchedSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}
