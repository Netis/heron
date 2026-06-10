//! Byte-signature scanning for offset-based uprobe attach (Phase 3).
//!
//! Statically-linked, symbol-stripped TLS stacks — notably Bun's vendored
//! BoringSSL inside the Claude Code binary — expose no `SSL_read` / `SSL_write`
//! symbol to attach a uprobe to (`nm -D` and `.symtab` both empty of `SSL_*`).
//! The only handle left is the machine code itself: locate the function by a
//! masked byte signature of its prologue, then attach the uprobe at that **file
//! offset** (the kernel uprobe API takes a file offset into the binary's inode,
//! which for an executable segment equals the on-disk offset of the
//! instruction).
//!
//! This module is **pure and cross-platform**: it parses an ELF image from a
//! byte slice and scans its executable `PT_LOAD` segments. No kernel, no eBPF —
//! so the matcher (the genuinely fiddly part) is unit-tested on every host. The
//! Linux loader feeds it the mmap'd target binary and attaches by the returned
//! offset.
//!
//! Signatures are inherently **version-specific**: a given prologue pattern
//! pins one BoringSSL build. They are therefore data (carried per flavor /
//! overridable from config), never logic — a new Bun release is a new
//! signature, not a code change.

/// A masked byte signature. `bytes[i]` is compared against the haystack only
/// where `mask[i] != 0`; a zero mask byte is a wildcard (matches anything),
/// which lets a signature skip over relocated addresses / immediates that vary
/// between builds while pinning the stable opcodes around them.
#[derive(Debug, Clone)]
pub struct Signature {
    pub bytes: Vec<u8>,
    pub mask: Vec<u8>,
}

impl Signature {
    /// Build a signature from a pattern string of space-separated hex bytes,
    /// where `??` is a wildcard. e.g. `"55 48 89 e5 ?? ?? 48 8b"`.
    /// Returns `None` on a malformed token.
    pub fn parse(pattern: &str) -> Option<Self> {
        let mut bytes = Vec::new();
        let mut mask = Vec::new();
        for tok in pattern.split_whitespace() {
            if tok == "??" || tok == "?" {
                bytes.push(0);
                mask.push(0);
            } else {
                bytes.push(u8::from_str_radix(tok, 16).ok()?);
                mask.push(0xFF);
            }
        }
        if bytes.is_empty() {
            return None;
        }
        Some(Self { bytes, mask })
    }

    /// Number of bytes the signature spans.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// True if the signature matches `hay` starting at `pos`.
    fn matches_at(&self, hay: &[u8], pos: usize) -> bool {
        if pos + self.bytes.len() > hay.len() {
            return false;
        }
        self.bytes
            .iter()
            .zip(&self.mask)
            .enumerate()
            .all(|(i, (b, m))| *m == 0 || hay[pos + i] == *b)
    }

    /// Every index in `hay` where this signature matches (mask-aware).
    pub fn find_all(&self, hay: &[u8]) -> Vec<usize> {
        if self.bytes.is_empty() || hay.len() < self.bytes.len() {
            return Vec::new();
        }
        // Anchor the linear scan on the first non-wildcard byte so a leading
        // wildcard run doesn't force a full compare at every position.
        let anchor = self.mask.iter().position(|m| *m != 0);
        let last = hay.len() - self.bytes.len();
        let mut hits = Vec::new();
        match anchor {
            Some(a) => {
                let anchor_byte = self.bytes[a];
                let mut pos = 0;
                while pos <= last {
                    if hay[pos + a] == anchor_byte && self.matches_at(hay, pos) {
                        hits.push(pos);
                    }
                    pos += 1;
                }
            }
            None => {
                // All-wildcard signature: degenerate, every position matches.
                hits.extend(0..=last);
            }
        }
        hits
    }
}

/// An executable `PT_LOAD` segment's file-resident byte range.
struct ExecSegment {
    file_off: u64,
    file_size: u64,
}

/// Parse the executable `PT_LOAD` segments of a 64-bit little-endian ELF.
/// Returns an empty vec for a non-ELF / unsupported image rather than erroring,
/// so a misconfigured target degrades to "no offsets found" not a crash.
fn exec_segments(data: &[u8]) -> Vec<ExecSegment> {
    const PT_LOAD: u32 = 1;
    const PF_X: u32 = 0x1;
    // ELF header: magic + EI_CLASS(64) + EI_DATA(LE).
    if data.len() < 64 || &data[0..4] != b"\x7fELF" || data[4] != 2 || data[5] != 1 {
        return Vec::new();
    }
    let rd_u16 = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
    let rd_u32 = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
    let rd_u64 = |o: usize| u64::from_le_bytes(data[o..o + 8].try_into().unwrap());

    let e_phoff = rd_u64(0x20) as usize;
    let e_phentsize = rd_u16(0x36) as usize;
    let e_phnum = rd_u16(0x38) as usize;
    if e_phentsize < 56 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for i in 0..e_phnum {
        let ph = e_phoff + i * e_phentsize;
        if ph + 56 > data.len() {
            break;
        }
        let p_type = rd_u32(ph);
        let p_flags = rd_u32(ph + 4);
        let p_offset = rd_u64(ph + 8);
        let p_filesz = rd_u64(ph + 32);
        if p_type == PT_LOAD
            && (p_flags & PF_X) != 0
            && p_offset.saturating_add(p_filesz) as usize <= data.len()
        {
            out.push(ExecSegment {
                file_off: p_offset,
                file_size: p_filesz,
            });
        }
    }
    out
}

/// Scan every executable segment of the ELF image `data` for `sig`, returning
/// the **file offsets** of all matches — exactly the values to hand the uprobe
/// attach (`offset` arg, with `fn_name = None`).
pub fn scan_elf_executable(data: &[u8], sig: &Signature) -> Vec<u64> {
    let mut offsets = Vec::new();
    for seg in exec_segments(data) {
        let start = seg.file_off as usize;
        let end = (seg.file_off + seg.file_size) as usize;
        let slice = &data[start..end];
        for local in sig.find_all(slice) {
            offsets.push(seg.file_off + local as u64);
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pattern_with_wildcards() {
        let s = Signature::parse("55 48 ?? e5 ??").expect("parse");
        assert_eq!(s.bytes, vec![0x55, 0x48, 0x00, 0xe5, 0x00]);
        assert_eq!(s.mask, vec![0xFF, 0xFF, 0x00, 0xFF, 0x00]);
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(Signature::parse("zz").is_none());
        assert!(Signature::parse("").is_none());
    }

    #[test]
    fn find_all_exact_and_masked() {
        let hay = b"\x00\x55\x48\x89\xe5\x00\x55\x48\x99\xe5\x00";
        // Exact: prologue `55 48 89 e5` occurs once (at index 1).
        let exact = Signature::parse("55 48 89 e5").unwrap();
        assert_eq!(exact.find_all(hay), vec![1]);
        // Masked: `55 48 ?? e5` matches both index 1 and index 6.
        let masked = Signature::parse("55 48 ?? e5").unwrap();
        assert_eq!(masked.find_all(hay), vec![1, 6]);
    }

    #[test]
    fn find_all_no_match_returns_empty() {
        let hay = b"\xde\xad\xbe\xef";
        assert!(Signature::parse("12 34").unwrap().find_all(hay).is_empty());
    }

    #[test]
    fn signature_at_end_of_buffer() {
        let hay = b"\x90\x90\x55\x48";
        assert_eq!(Signature::parse("55 48").unwrap().find_all(hay), vec![2]);
        // One byte past the end must not match (bounds).
        assert!(Signature::parse("55 48 89").unwrap().find_all(hay).is_empty());
    }

    /// Build a minimal 64-bit LE ELF with a single executable PT_LOAD segment
    /// whose file-resident bytes contain a known prologue, and assert the
    /// scanner reports the correct **file offset** (segment p_offset + local).
    #[test]
    fn scan_elf_returns_file_offset_within_exec_segment() {
        let mut elf = vec![0u8; 0x200];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2; // 64-bit
        elf[5] = 1; // little-endian
        // e_phoff = 0x40, e_phentsize = 56, e_phnum = 1.
        elf[0x20..0x28].copy_from_slice(&0x40u64.to_le_bytes());
        elf[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        elf[0x38..0x3A].copy_from_slice(&1u16.to_le_bytes());
        // Program header @0x40: PT_LOAD, PF_X, p_offset=0x100, p_filesz=0x40.
        let ph = 0x40;
        elf[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        elf[ph + 4..ph + 8].copy_from_slice(&0x1u32.to_le_bytes()); // PF_X
        elf[ph + 8..ph + 16].copy_from_slice(&0x100u64.to_le_bytes()); // p_offset
        elf[ph + 32..ph + 40].copy_from_slice(&0x40u64.to_le_bytes()); // p_filesz
        // Plant a prologue at file offset 0x110 (0x10 into the segment).
        elf[0x110..0x114].copy_from_slice(&[0x55, 0x48, 0x89, 0xe5]);

        let sig = Signature::parse("55 48 89 e5").unwrap();
        assert_eq!(scan_elf_executable(&elf, &sig), vec![0x110]);
    }

    #[test]
    fn non_elf_yields_no_segments() {
        let sig = Signature::parse("55 48").unwrap();
        assert!(scan_elf_executable(b"not an elf at all..........", &sig).is_empty());
    }

    #[test]
    fn match_outside_exec_segment_is_ignored() {
        // Same prologue planted BEFORE the exec segment's file range must not
        // be reported — only executable bytes are scanned.
        let mut elf = vec![0u8; 0x200];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2;
        elf[5] = 1;
        elf[0x20..0x28].copy_from_slice(&0x40u64.to_le_bytes());
        elf[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        elf[0x38..0x3A].copy_from_slice(&1u16.to_le_bytes());
        let ph = 0x40;
        elf[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes());
        elf[ph + 4..ph + 8].copy_from_slice(&0x1u32.to_le_bytes());
        elf[ph + 8..ph + 16].copy_from_slice(&0x100u64.to_le_bytes());
        elf[ph + 32..ph + 40].copy_from_slice(&0x40u64.to_le_bytes());
        // Prologue at 0x80 — inside the header area, OUTSIDE [0x100, 0x140).
        elf[0x80..0x84].copy_from_slice(&[0x55, 0x48, 0x89, 0xe5]);
        let sig = Signature::parse("55 48 89 e5").unwrap();
        assert!(scan_elf_executable(&elf, &sig).is_empty());
    }
}
