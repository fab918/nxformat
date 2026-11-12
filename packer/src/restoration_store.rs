//! Packed literal ranges used only during edited restoration. Original NX
//! offsets are translated in memory; no sparse or full-size source NX is built.
use super::*;

#[derive(Clone, Debug)]
struct Span {
    original: u64,
    packed: u64,
    len: u64,
}

pub(super) struct Store {
    spans: Vec<Span>,
    pub bytes: u64,
    original_size: u64,
    header: Vec<u8>,
    tail: Option<(u64, u64)>,
}
impl Store {
    pub fn new(original_size: u64) -> Self {
        Self {
            spans: Vec::new(),
            bytes: 0,
            original_size,
            header: Vec::new(),
            tail: None,
        }
    }
    fn append(
        &mut self,
        file: &mut File,
        original: u64,
        bytes: &[u8],
        options: &WriterOptions,
    ) -> Result<(), WriterError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let end = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| limit_error("metadata store size overflow"))?;
        check_temporary_size(end, options)?;
        let merge = self
            .spans
            .last()
            .is_some_and(|s| s.original + s.len == original && s.packed + s.len == self.bytes);
        if !merge && (self.spans.len() as u64 + 1) * 64 > options.max_memory_bytes / 16 {
            return Err(limit_error("metadata store mapping exceeds memory budget"));
        }
        file.seek(SeekFrom::Start(self.bytes))?;
        file.write_all(bytes)?;
        if merge {
            self.spans.last_mut().unwrap().len += bytes.len() as u64;
        } else {
            self.spans.push(Span {
                original,
                packed: self.bytes,
                len: bytes.len() as u64,
            });
        }
        self.bytes = end;
        Ok(())
    }
    fn capture_range(
        &mut self,
        file: &mut File,
        at: u64,
        bytes: &[u8],
        range: (u64, u64),
        options: &WriterOptions,
    ) -> Result<(), WriterError> {
        let begin = at.max(range.0);
        let end = (at + bytes.len() as u64).min(range.1);
        if begin < end {
            self.append(
                file,
                begin,
                &bytes[(begin - at) as usize..(end - at) as usize],
                options,
            )?;
        }
        Ok(())
    }
    pub fn metadata(
        &mut self,
        file: &mut File,
        at: u64,
        bytes: &[u8],
        options: &WriterOptions,
    ) -> Result<(), WriterError> {
        self.capture_range(file, at, bytes, (0, DATA_START), options)?;
        if at < DATA_START {
            if self.header.len() as u64 != at {
                return Err(bad("nonliteral restoration header"));
            }
            self.header
                .extend_from_slice(&bytes[..bytes.len().min((DATA_START - at) as usize)]);
            if self.header.len() as u64 == DATA_START {
                // Only select ranges here. The normal archive parser validates
                // both headers, layout and all metadata hashes after replay.
                let slot = |index: usize| -> Result<Option<SourceHeader>, WriterError> {
                    let page =
                        &self.header[index * HEADER_PAGE_SIZE..(index + 1) * HEADER_PAGE_SIZE];
                    if page[256..].iter().any(|&b| b != 0) {
                        return Ok(None);
                    }
                    decode_slot(&page[..256])
                };
                let a = slot(0)?;
                let b = slot(1)?;
                let h = match (a, b) {
                    (Some(a), Some(b)) => {
                        if b.generation > a.generation {
                            b
                        } else {
                            a
                        }
                    }
                    (Some(h), None) | (None, Some(h)) => h,
                    _ => return Err(bad("no valid restoration metadata header")),
                };
                let start = if h.media_size == 0 {
                    h.catalog_offset
                } else {
                    h.media_offset
                };
                if start < DATA_START
                    || start > h.archive_size
                    || h.archive_size > self.original_size
                {
                    return Err(bad("invalid restoration metadata range"));
                }
                self.tail = Some((start, h.archive_size));
                self.header = Vec::new();
            }
        }
        if let Some(tail) = self.tail {
            self.capture_range(file, at, bytes, tail, options)?;
        }
        Ok(())
    }
    pub fn resources(
        &mut self,
        file: &mut File,
        at: u64,
        bytes: &[u8],
        ranges: &[(u64, u64)],
        options: &WriterOptions,
    ) -> Result<(), WriterError> {
        let first = ranges.partition_point(|r| r.1 <= at);
        for &r in &ranges[first..] {
            if r.0 >= at + bytes.len() as u64 {
                break;
            }
            self.capture_range(file, at, bytes, r, options)?;
        }
        Ok(())
    }
    pub fn sort(&mut self) {
        self.spans.sort_unstable_by_key(|s| s.original);
    }
    pub fn translate(&self, at: u64, len: u64) -> Result<u64, WriterError> {
        let i = self.spans.partition_point(|s| s.original <= at);
        let s = i.checked_sub(1).and_then(|i| self.spans.get(i)).ok_or_else(|| bad("retained metadata or resource bytes are absent; regenerate the extraction recipe before editing"))?;
        if len > s.len || at - s.original > s.len - len {
            return Err(bad("incomplete retained metadata or resource range"));
        }
        Ok(s.packed + at - s.original)
    }
    pub fn reader<'a>(&'a self, file: &'a mut File) -> Reader<'a> {
        Reader { store: self, file }
    }
    pub fn memory_bytes(&self) -> u64 {
        self.spans.len() as u64 * 64
    }
}
pub(super) struct Reader<'a> {
    store: &'a Store,
    file: &'a mut File,
}
impl ArchiveRead for Reader<'_> {
    fn archive_len(&self) -> Result<u64, WriterError> {
        Ok(self.store.original_size)
    }
    fn read_at(&mut self, at: u64, bytes: &mut [u8]) -> Result<(), WriterError> {
        let packed = self.store.translate(at, bytes.len() as u64)?;
        read_exact_at(self.file, packed, bytes)
    }
}

pub(super) fn resource_ranges(
    path: &Path,
    store: &Store,
    options: &WriterOptions,
) -> Result<Vec<(u64, u64)>, WriterError> {
    let mut file = File::open(path)?;
    let mut budget = MemoryBudget {
        current: store.memory_bytes(),
        peak: 0,
        limit: options.max_memory_bytes / 2,
    };
    let archive = parse_archive(&mut store.reader(&mut file), 0, options, &mut budget)?;
    let mut ranges = Vec::new();
    for app in &archive.applications {
        for &(_, root) in &app.source_resources {
            for entry in &archive.directories[root as usize] {
                let record = &archive.files[entry.target as usize];
                for i in 0..record.block_count {
                    budget.charge(32, "retained resource ranges")?;
                    let b = &archive.blocks
                        [archive.block_refs[(record.first_ref + i) as usize] as usize];
                    ranges.push((b.range.offset, b.range.offset + b.stored_size as u64));
                }
            }
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for r in ranges {
        if let Some(last) = merged.last_mut() {
            if r.0 <= last.1 {
                last.1 = last.1.max(r.1);
                continue;
            }
        }
        merged.push(r);
    }
    Ok(merged)
}

// A second bounded streaming pass is needed only when source resources exist.
// This prevents literal game blocks (including portable recipes) being staged.
#[allow(clippy::too_many_arguments)]
pub(super) fn collect_resources(
    root: &SourceRoot,
    store: &mut Store,
    output: &mut File,
    ranges: &[(u64, u64)],
    prefix: u64,
    checksum: [u8; 32],
    options: &WriterOptions,
) -> Result<(), WriterError> {
    if ranges.is_empty() {
        return Ok(());
    }
    let mut decoder =
        zstd::stream::read::Decoder::new(BufReader::new(root.open_relative(RESTORATION_FILE)?))?;
    decoder.window_log_max(23)?;
    let mut input = HashRead {
        inner: decoder,
        hash: Sha256::new(),
        remaining: store
            .original_size
            .saturating_mul(16)
            .saturating_add(options.max_memory_bytes),
    };
    let mut buffer = vec![0; CHUNK];
    fn skip(
        r: &mut impl Read,
        mut n: u64,
        buffer: &mut [u8],
        options: &WriterOptions,
    ) -> Result<(), WriterError> {
        while n > 0 {
            cancelled(options)?;
            let k = n.min(buffer.len() as u64) as usize;
            r.read_exact(&mut buffer[..k])?;
            n -= k as u64;
        }
        Ok(())
    }
    skip(&mut input, prefix, &mut buffer, options)?;
    let mut at = 0u64;
    loop {
        cancelled(options)?;
        match byte(&mut input)? {
            255 => break,
            0 => {
                let n = number(&mut input)?;
                if n == 0 || n > store.original_size.saturating_sub(at) {
                    return Err(bad("invalid resource replay literal"));
                }
                let mut left = n;
                while left > 0 {
                    cancelled(options)?;
                    let k = left.min(buffer.len() as u64) as usize;
                    input.read_exact(&mut buffer[..k])?;
                    store.resources(output, at, &buffer[..k], ranges, options)?;
                    left -= k as u64;
                    at += k as u64;
                }
            }
            1 => {
                skip(&mut input, 2, &mut buffer, options)?;
                let logical = number(&mut input)?;
                let stored = number(&mut input)?;
                let count = number(&mut input)?;
                if logical == 0
                    || logical > 1024 * 1024
                    || count > logical
                    || stored > store.original_size.saturating_sub(at)
                {
                    return Err(bad("invalid resource replay block"));
                }
                for _ in 0..count {
                    let kind = byte(&mut input)?;
                    let n = number(&mut input)?;
                    match kind {
                        0 => skip(&mut input, n, &mut buffer, options)?,
                        1 => skip(&mut input, 12, &mut buffer, options)?,
                        2 => skip(&mut input, 17, &mut buffer, options)?,
                        _ => return Err(bad("invalid resource replay piece")),
                    }
                }
                at += stored;
            }
            _ => return Err(bad("invalid resource replay operation")),
        }
    }
    let actual = input.hash.finish();
    let mut footer = [0; 32];
    input.inner.read_exact(&mut footer)?;
    let mut extra = [0];
    if actual != checksum
        || footer != checksum
        || at != store.original_size
        || input.inner.read(&mut extra)? != 0
    {
        return Err(bad("restoration recipe changed during resource replay"));
    }
    store.sort();
    // Each required range must be present literally. Old recipes which referred
    // to a game file instead fail before any modified archive is published.
    for &(start, end) in ranges {
        store.translate(start, end - start)?;
    }
    Ok(())
}
