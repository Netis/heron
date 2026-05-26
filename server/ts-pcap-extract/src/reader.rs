//! Per-file iterator yielding pcap records. Supports plain `.pcap` and
//! `.pcap.snappy`. Tolerates EOF inside record header or data — the
//! pcap_dump writer flushes every ~1s, so a reader can race the in-flight
//! current minute file. Truncation ends iteration cleanly rather than
//! erroring.

use std::fs::File;
use std::io::{self, BufReader, Read};

use bytes::Bytes;
use snap::read::FrameDecoder;

use crate::candidates::CandidateFile;
use crate::format::PCAP_MAGIC;

#[derive(Debug, Clone)]
pub struct RawRec {
    pub ts_us: i64,
    pub caplen: u32,
    pub wirelen: u32,
    pub data: Bytes,
}

/// One opened file's iterator. Holds its own boxed reader because plain and
/// snappy have different concrete types.
pub struct PacketIter {
    inner: Box<dyn Read + Send>,
    pub link_type: u32,
}

impl PacketIter {
    pub fn open(file: &CandidateFile) -> io::Result<Self> {
        let f = File::open(&file.path)?;
        let buf = BufReader::with_capacity(64 * 1024, f);
        let mut inner: Box<dyn Read + Send> = if file.compressed {
            Box::new(FrameDecoder::new(buf))
        } else {
            Box::new(buf)
        };
        let mut header = [0u8; 24];
        inner.read_exact(&mut header)?;
        let magic = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if magic != PCAP_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad pcap magic {magic:#010x} in {}", file.path.display()),
            ));
        }
        let link_type = u32::from_le_bytes(header[20..24].try_into().unwrap());
        Ok(Self { inner, link_type })
    }
}

impl Iterator for PacketIter {
    type Item = RawRec;

    fn next(&mut self) -> Option<RawRec> {
        let mut hdr = [0u8; 16];
        if read_full_or_eof(&mut self.inner, &mut hdr).ok()? != 16 {
            return None;
        }
        let ts_sec = u32::from_le_bytes(hdr[0..4].try_into().unwrap()) as i64;
        let ts_usec = u32::from_le_bytes(hdr[4..8].try_into().unwrap()) as i64;
        let caplen = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        let wirelen = u32::from_le_bytes(hdr[12..16].try_into().unwrap());
        let mut data = vec![0u8; caplen as usize];
        if read_full_or_eof(&mut self.inner, &mut data).ok()? != caplen as usize {
            return None;
        }
        Some(RawRec {
            ts_us: ts_sec * 1_000_000 + ts_usec,
            caplen,
            wirelen,
            data: Bytes::from(data),
        })
    }
}

/// Read `buf` exactly, OR cleanly report short-read on EOF / truncation.
/// Returns the number of bytes actually filled. Caller treats `< buf.len()`
/// as "stop iteration".
fn read_full_or_eof<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => return Ok(filled),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(filled),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Path-helper for tests so we don't repeat the construct.
#[cfg(test)]
fn open_path(path: &std::path::Path, compressed: bool) -> io::Result<PacketIter> {
    PacketIter::open(&CandidateFile {
        path: path.to_path_buf(),
        compressed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_pcap_global_header(w: &mut impl Write, link_type: u32) -> io::Result<()> {
        w.write_all(&PCAP_MAGIC.to_le_bytes())?;
        w.write_all(&2u16.to_le_bytes())?;
        w.write_all(&4u16.to_le_bytes())?;
        w.write_all(&0i32.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?;
        w.write_all(&262_144u32.to_le_bytes())?;
        w.write_all(&link_type.to_le_bytes())?;
        Ok(())
    }
    fn write_record(w: &mut impl Write, ts_us: i64, data: &[u8]) -> io::Result<()> {
        let ts_sec = (ts_us / 1_000_000) as u32;
        let ts_usec = (ts_us % 1_000_000) as u32;
        w.write_all(&ts_sec.to_le_bytes())?;
        w.write_all(&ts_usec.to_le_bytes())?;
        w.write_all(&(data.len() as u32).to_le_bytes())?;
        w.write_all(&(data.len() as u32).to_le_bytes())?;
        w.write_all(data)?;
        Ok(())
    }

    #[test]
    fn reads_two_plain_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap");
        let mut f = File::create(&path).unwrap();
        write_pcap_global_header(&mut f, 1).unwrap();
        write_record(&mut f, 1_000_000, &[0xaa, 0xbb]).unwrap();
        write_record(&mut f, 2_500_000, &[0xcc]).unwrap();
        drop(f);
        let mut it = open_path(&path, false).unwrap();
        assert_eq!(it.link_type, 1);
        let r1 = it.next().unwrap();
        assert_eq!(r1.ts_us, 1_000_000);
        assert_eq!(&r1.data[..], &[0xaa, 0xbb]);
        let r2 = it.next().unwrap();
        assert_eq!(r2.ts_us, 2_500_000);
        assert!(it.next().is_none());
    }

    #[test]
    fn truncated_record_data_stops_cleanly() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap");
        let mut f = File::create(&path).unwrap();
        write_pcap_global_header(&mut f, 1).unwrap();
        // record header says caplen=10 but only writes 3 bytes
        f.write_all(&0u32.to_le_bytes()).unwrap(); // ts_sec
        f.write_all(&500_000u32.to_le_bytes()).unwrap(); // ts_usec
        f.write_all(&10u32.to_le_bytes()).unwrap(); // caplen
        f.write_all(&10u32.to_le_bytes()).unwrap(); // wirelen
        f.write_all(&[0xaa, 0xbb, 0xcc]).unwrap(); // truncated
        drop(f);
        let mut it = open_path(&path, false).unwrap();
        // No complete record before EOF.
        assert!(it.next().is_none());
    }

    #[test]
    fn truncated_record_header_stops_cleanly() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap");
        let mut f = File::create(&path).unwrap();
        write_pcap_global_header(&mut f, 1).unwrap();
        f.write_all(&[0u8; 5]).unwrap(); // partial 16-byte record header
        drop(f);
        let mut it = open_path(&path, false).unwrap();
        assert!(it.next().is_none());
    }

    #[test]
    fn rejects_bad_magic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap");
        let mut f = File::create(&path).unwrap();
        f.write_all(&[0u8; 24]).unwrap();
        drop(f);
        assert!(open_path(&path, false).is_err());
    }

    #[test]
    fn reads_snappy_records() {
        use snap::write::FrameEncoder;
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.pcap.snappy");
        let f = File::create(&path).unwrap();
        let mut enc = FrameEncoder::new(f);
        write_pcap_global_header(&mut enc, 1).unwrap();
        write_record(&mut enc, 1_000_000, &[0x10, 0x20]).unwrap();
        drop(enc);
        let mut it = open_path(&path, true).unwrap();
        assert_eq!(it.link_type, 1);
        let r = it.next().unwrap();
        assert_eq!(r.ts_us, 1_000_000);
        assert_eq!(&r.data[..], &[0x10, 0x20]);
    }
}
