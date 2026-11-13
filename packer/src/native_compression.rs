//! NCA compressed storage: bounded RAW/zero/LZ4 blocks above the IVFC data
//! layer. Original compressed bytes remain untouched in native NX snapshots.
use super::*;
const NODE: usize = 0x4000;
const CAPACITY: usize = (NODE - 16) / 24;
const BLOCK: usize = 65536;
#[derive(Debug)]
struct Entry {
    start: u64,
    physical: u64,
    codec: u8,
    stored: u32,
}

pub(super) struct Reader<R> {
    source: R,
    data_start: u64,
    entries: Vec<Entry>,
    pub len: u64,
    pub memory: u64,
    position: u64,
    cached: Option<usize>,
    frame: Vec<u8>,
    plain: Vec<u8>,
    token: CancellationToken,
}
impl<R: Read + Seek> Reader<R> {
    pub fn new(
        mut source: R,
        data_start: u64,
        data_len: u64,
        info: &[u8; 40],
        limit: u64,
        token: CancellationToken,
    ) -> Result<Self, WriterError> {
        let offset = u64_at(info, 0);
        let size = u64_at(info, 8);
        if &info[16..20] != b"BKTR" || u32_at(info, 20) != 1 || info[28..].iter().any(|&b| b != 0) {
            return Err(invalid("unsupported NCA compression table header"));
        }
        let count = u32_at(info, 24) as usize;
        let leaves = count.div_ceil(CAPACITY);
        let memory = (count as u64)
            .saturating_mul(48)
            .saturating_add(3 * BLOCK as u64);
        if memory > limit / 2 {
            return Err(limit_error("NCA compression index exceeds memory budget"));
        }
        if count == 0 || leaves > (NODE - 16) / 8 {
            return Err(invalid("unsupported NCA compression tree depth"));
        }
        if offset.checked_add(size).map_or(true, |end| end > data_len)
            || size < ((leaves + 1) * NODE) as u64
        {
            return Err(bad("NCA compression table exceeds integrity data"));
        }
        let mut node = [0u8; NODE];
        source.seek(SeekFrom::Start(data_start + offset))?;
        source.read_exact(&mut node)?;
        let len = u64_at(&node, 8);
        if u32_at(&node, 0) != 0
            || u32_at(&node, 4) as usize != leaves
            || len == 0
            || len > i64::MAX as u64
        {
            return Err(bad("invalid NCA compression root"));
        }
        let starts: Vec<_> = (0..leaves).map(|i| u64_at(&node, 16 + i * 8)).collect();
        let mut entries = Vec::with_capacity(count);
        for leaf in 0..leaves {
            if token.is_cancelled() {
                return Err(WriterError::new(
                    WriterErrorKind::Cancelled,
                    "NCA compression read cancelled",
                ));
            }
            source.seek(SeekFrom::Start(
                data_start + offset + ((leaf + 1) * NODE) as u64,
            ))?;
            source.read_exact(&mut node)?;
            let n = u32_at(&node, 4) as usize;
            let end = starts.get(leaf + 1).copied().unwrap_or(len);
            if u32_at(&node, 0) as usize != leaf
                || n != (count - entries.len()).min(CAPACITY)
                || u64_at(&node, 8) != end
            {
                return Err(bad("invalid NCA compression leaf"));
            }
            for i in 0..n {
                let at = 16 + i * 24;
                let start = u64_at(&node, at);
                let physical = u64_at(&node, at + 8);
                let codec = node[at + 16];
                let stored = u32_at(&node, at + 20);
                if !matches!(codec, 0 | 1 | 3) {
                    return Err(invalid("unsupported NCA compression codec"));
                }
                if node[at + 17..at + 20].iter().any(|&b| b != 0)
                    || start >= end
                    || (i == 0 && start != starts[leaf])
                    || entries.last().is_some_and(|e: &Entry| e.start >= start)
                {
                    return Err(bad("invalid NCA compression entry ordering"));
                }
                entries.push(Entry {
                    start,
                    physical,
                    codec,
                    stored,
                });
            }
        }
        if entries[0].start != 0 {
            return Err(bad("NCA compression does not cover offset zero"));
        }
        for (i, e) in entries.iter().enumerate() {
            let logical = entries.get(i + 1).map_or(len, |e| e.start) - e.start;
            let bytes = if e.codec == 0 {
                logical
            } else {
                e.stored as u64
            };
            if e.codec != 1
                && (e.physical > i64::MAX as u64
                    || e.physical
                        .checked_add(bytes)
                        .map_or(true, |end| end > offset))
            {
                return Err(bad("NCA compressed block exceeds payload"));
            }
            if e.codec == 3
                && (logical > BLOCK as u64
                    || e.stored == 0
                    || e.stored as usize > BLOCK
                    || e.physical % 16 != 0)
            {
                return Err(bad("invalid NCA LZ4 block bounds"));
            }
        }
        Ok(Self {
            source,
            data_start,
            entries,
            len,
            memory,
            position: 0,
            cached: None,
            frame: Vec::with_capacity(BLOCK),
            plain: Vec::with_capacity(BLOCK),
            token,
        })
    }
}
impl<R: Read + Seek> Read for Reader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.token.is_cancelled() {
            return Err(io::Error::other("NCA compression read cancelled"));
        }
        if bytes.is_empty() || self.position == self.len {
            return Ok(0);
        }
        let i = self.entries.partition_point(|e| e.start <= self.position) - 1;
        let e = &self.entries[i];
        let end = self.entries.get(i + 1).map_or(self.len, |e| e.start);
        let n = bytes
            .len()
            .min((end - self.position).min(usize::MAX as u64) as usize);
        match e.codec {
            0 => {
                self.source.seek(SeekFrom::Start(
                    self.data_start + e.physical + self.position - e.start,
                ))?;
                self.source.read_exact(&mut bytes[..n])?;
            }
            1 => bytes[..n].fill(0),
            3 => {
                if self.cached != Some(i) {
                    self.frame.resize(e.stored as usize, 0);
                    self.source
                        .seek(SeekFrom::Start(self.data_start + e.physical))?;
                    self.source.read_exact(&mut self.frame)?;
                    lz4(&self.frame, (end - e.start) as usize, &mut self.plain)?;
                    self.cached = Some(i);
                }
                let offset = (self.position - e.start) as usize;
                bytes[..n].copy_from_slice(&self.plain[offset..offset + n]);
            }
            _ => return Err(io::Error::other("unsupported NCA compression codec")),
        }
        self.position += n as u64;
        Ok(n)
    }
}
impl<R: Read + Seek> Seek for Reader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let at = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.position as i128 + n as i128,
            SeekFrom::End(n) => self.len as i128 + n as i128,
        };
        if at < 0 || at > self.len as i128 {
            return Err(io::Error::other("NCA compression seek out of bounds"));
        }
        self.position = at as u64;
        Ok(self.position)
    }
}
fn lz4(input: &[u8], expected: usize, out: &mut Vec<u8>) -> io::Result<()> {
    let err = || io::Error::other("invalid NCA LZ4 block");
    fn length(input: &[u8], at: &mut usize, mut n: usize) -> io::Result<usize> {
        if n == 15 {
            loop {
                let b = *input
                    .get(*at)
                    .ok_or_else(|| io::Error::other("truncated NCA LZ4 length"))?;
                *at += 1;
                n += b as usize;
                if n > BLOCK {
                    return Err(io::Error::other("NCA LZ4 expansion exceeds block"));
                }
                if b != 255 {
                    break;
                }
            }
        }
        Ok(n)
    }
    out.clear();
    let mut at = 0;
    while at < input.len() {
        let token = input[at];
        at += 1;
        let literals = length(input, &mut at, (token >> 4) as usize)?;
        if literals > input.len() - at || literals > expected - out.len() {
            return Err(err());
        }
        out.extend_from_slice(&input[at..at + literals]);
        at += literals;
        if at == input.len() {
            break;
        }
        if input.len() - at < 2 {
            return Err(err());
        }
        let offset = u16::from_le_bytes([input[at], input[at + 1]]) as usize;
        at += 2;
        if offset == 0 || offset > out.len() {
            return Err(err());
        }
        let mut n = length(input, &mut at, (token & 15) as usize)? + 4;
        if n > expected - out.len() {
            return Err(err());
        }
        let source = out.len() - offset;
        while n > 0 {
            let k = n.min(out.len() - source);
            out.extend_from_within(source..source + k);
            n -= k;
        }
    }
    if out.len() != expected {
        return Err(err());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    fn image() -> Vec<u8> {
        let mut b = vec![42; 512 + 65536];
        b.extend(vec![0; 65536]);
        b.extend((0..901).map(|i| (i % 251) as u8));
        b
    }
    #[test]
    fn mixed_codec_reads_cross_blocks_and_seek_with_bounded_memory() {
        let expected = image();
        let (bytes, info) = native::synthetic::compressed_storage(&expected);
        let size = bytes.len() as u64;
        let mut reader = Reader::new(
            Cursor::new(bytes),
            0,
            size,
            &info,
            8 * 1024 * 1024,
            CancellationToken::default(),
        )
        .unwrap();
        assert!(reader.memory < 256 * 1024);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, expected);
        for at in [0, 500, 66040, 131570] {
            reader.seek(SeekFrom::Start(at)).unwrap();
            let mut b = [0; 512];
            reader.read_exact(&mut b).unwrap();
            assert_eq!(b, expected[at as usize..at as usize + 512]);
        }
        assert!(reader.seek(SeekFrom::End(1)).is_err());
    }
    #[test]
    fn corrupt_tables_bad_codec_limits_and_lz4_are_rejected() {
        let expected = image();
        let (bytes, info) = native::synthetic::compressed_storage(&expected);
        let size = bytes.len() as u64;
        let at = u64_at(&info, 0) as usize;
        for (offset, value) in [(at + 4, 0), (at + NODE + 16 + 16, 4), (at + NODE + 16, 1)] {
            let mut damaged = bytes.clone();
            damaged[offset] = value;
            assert!(Reader::new(
                Cursor::new(damaged),
                0,
                size,
                &info,
                8 * 1024 * 1024,
                CancellationToken::default()
            )
            .is_err());
        }
        assert!(Reader::new(
            Cursor::new(bytes.clone()),
            0,
            size,
            &info,
            128,
            CancellationToken::default()
        )
        .is_err());
        let mut damaged = bytes;
        let lz4 = &damaged[at + NODE + 16 + 24..at + NODE + 16 + 48];
        let off = u64_at(lz4, 8) as usize;
        damaged[off + 2] = 0;
        damaged[off + 3] = 0;
        let mut reader = Reader::new(
            Cursor::new(damaged),
            0,
            size,
            &info,
            8 * 1024 * 1024,
            CancellationToken::default(),
        )
        .unwrap();
        reader.seek(SeekFrom::Start(512)).unwrap();
        assert!(reader.read_exact(&mut [0; 10]).is_err());
    }
}
