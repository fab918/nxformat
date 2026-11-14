//! Bounded, portable reproduction of LZ4's greedy by-U32 streaming parser.
//! Adapted from LZ4 (Yann Collet, 2011-2020), BSD-2-Clause; see LICENSE.
//! Only byte-identical candidates become recipe instructions. Other encodings
//! retain literal topology, so no source encoder is presumed compatible.
use super::*;

fn length(out: &mut Vec<u8>, mut len: usize) {
    while len >= 255 {
        out.push(255);
        len -= 255;
    }
    out.push(len as u8);
}
fn literals(out: &mut Vec<u8>, bytes: &[u8]) -> usize {
    let token = out.len();
    out.push((bytes.len().min(15) as u8) << 4);
    if bytes.len() >= 15 {
        length(out, bytes.len() - 15);
    }
    out.extend_from_slice(bytes);
    token
}
fn hash(plain: &[u8], at: usize, bits: u8) -> usize {
    let word = u64::from_le_bytes(plain[at..at + 8].try_into().unwrap());
    ((word << 24).wrapping_mul(889523592379) >> (64 - bits)) as usize
}
fn equal4(plain: &[u8], a: usize, b: usize) -> bool {
    plain[a..a + 4] == plain[b..b + 4]
}

pub(in crate::writer::repack) fn compress(plain: &[u8], bits: u8) -> Result<Vec<u8>, WriterError> {
    if plain.is_empty() || plain.len() > 65536 || !(12..=16).contains(&bits) {
        return Err(bad("invalid restoration LZ4 parameters"));
    }
    let mut out = Vec::with_capacity(plain.len() + plain.len() / 255 + 16);
    if plain.len() < 13 {
        literals(&mut out, plain);
        return Ok(out);
    }
    let mut table = vec![0u32; 1usize << bits];
    let limit = plain.len() - 11;
    let match_limit = plain.len() - 5;
    let (mut ip, mut anchor) = (1usize, 0usize);
    let mut forward_hash = hash(plain, ip, bits);
    'search: loop {
        let (mut forward, mut step, mut searches) = (ip, 1usize, 64usize);
        let mut matched;
        loop {
            let h = forward_hash;
            ip = forward;
            forward += step;
            step = searches >> 6;
            searches += 1;
            if forward > limit {
                break 'search;
            }
            matched = table[h] as usize;
            forward_hash = hash(plain, forward, bits);
            table[h] = ip as u32;
            if matched + 65535 >= ip && equal4(plain, matched, ip) {
                break;
            }
        }
        while ip > anchor && matched > 0 && plain[ip - 1] == plain[matched - 1] {
            ip -= 1;
            matched -= 1;
        }
        let mut token = literals(&mut out, &plain[anchor..ip]);
        loop {
            out.extend_from_slice(&((ip - matched) as u16).to_le_bytes());
            let start = ip;
            ip += 4;
            matched += 4;
            while ip + 8 <= match_limit && plain[ip..ip + 8] == plain[matched..matched + 8] {
                ip += 8;
                matched += 8;
            }
            while ip < match_limit && plain[ip] == plain[matched] {
                ip += 1;
                matched += 1;
            }
            let code = ip - start - 4;
            out[token] |= code.min(15) as u8;
            if code >= 15 {
                length(&mut out, code - 15);
            }
            anchor = ip;
            if ip >= limit {
                break 'search;
            }
            table[hash(plain, ip - 2, bits)] = (ip - 2) as u32;
            let h = hash(plain, ip, bits);
            matched = table[h] as usize;
            table[h] = ip as u32;
            if matched + 65535 >= ip && equal4(plain, matched, ip) {
                token = out.len();
                out.push(0);
            } else {
                ip += 1;
                forward_hash = hash(plain, ip, bits);
                break;
            }
        }
    }
    literals(&mut out, &plain[anchor..]);
    Ok(out)
}

pub(in crate::writer::repack) struct Search {
    cached: Vec<u8>,
    searches_left: u8,
}
impl Default for Search {
    fn default() -> Self {
        Self {
            cached: Vec::new(),
            searches_left: 8,
        }
    }
}
impl Search {
    pub fn find(
        &mut self,
        plain: &[u8],
        encoded: &[u8],
        options: &WriterOptions,
    ) -> Result<Option<u8>, WriterError> {
        for &bits in &self.cached {
            cancelled(options)?;
            if compress(plain, bits)? == encoded {
                return Ok(Some(bits));
            }
        }
        if self.searches_left != 0 {
            self.searches_left -= 1;
            for bits in [13, 12, 14, 15, 16] {
                if self.cached.contains(&bits) {
                    continue;
                }
                cancelled(options)?;
                if compress(plain, bits)? == encoded {
                    self.cached.insert(0, bits);
                    self.cached.truncate(4);
                    return Ok(Some(bits));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoder_selection_is_byte_tested_and_unknown_encodings_fall_back() {
        let plain: Vec<_> = (0..65536).map(|i| ((i / 23 + i % 7) % 251) as u8).collect();
        let options = WriterOptions::default();
        let mut search = Search::default();
        for bits in 12..=16 {
            let frame = compress(&plain, bits).unwrap();
            let selected = search.find(&plain, &frame, &options).unwrap().unwrap();
            assert_eq!(compress(&plain, selected).unwrap(), frame);
        }
        assert!(search
            .find(&plain, b"arbitrary original frame", &options)
            .unwrap()
            .is_none());
        assert!(compress(&[], 13).is_err());
        assert!(compress(&vec![0; 65537], 13).is_err());
        assert!(compress(&plain, 17).is_err());
    }
    #[test]
    fn portable_encoder_matches_independent_upstream_stream_vectors() {
        // Digests generated independently using upstream liblz4 1.9.4,
        // LZ4_compress_fast_continue(acceleration=1), memory usage 14..18.
        let mut state = 0x12345678u32;
        let plain: Vec<_> = (0..65536)
            .map(|i| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                if i % 97 < 32 {
                    (state >> 24) as u8
                } else {
                    ((i / 31 + i % 7) % 251) as u8
                }
            })
            .collect();
        let expected = [
            "9b6307b182e2947e80f41342e7acf34a4173c73ac0b4c2ae196e0ff2294ab5bd",
            "3d9b71256706664c1efdf38bc50bc8ba4740273050b52a5ad8fa3505b3a635ac",
            "f21a176c55410d89dc917e2b2ae21a04a7af2130774b7679577627217ff888b3",
            "ea6a13b129501fa947c75d694c909e220cdb4f7b3ebf5682867fd8f0ceb47645",
            "f0876be4e9536e9b22e97dbfa45cd5da03ffa2ec9a6ce5bda15ece708eaa45c5",
        ];
        for (bits, digest) in (12..=16).zip(expected) {
            assert_eq!(
                crate::import::hex(&crate::hash::sha256(&compress(&plain, bits).unwrap())),
                digest
            );
        }
    }
}
