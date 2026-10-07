//! Matcher LZ77 хеш двух байтов для ссылок и последний байт для коротких совпадений

use super::WINDOW;

const NONE: usize = usize::MAX;
const HASH_BITS: u32 = 12;
const HASH_SIZE: usize = 1 << HASH_BITS; //4096

#[derive(Default)]
pub(crate) struct Match {
    pub distance: usize,
    pub length: usize,
    pub candidates: usize,
}

pub(crate) struct Matcher {
    heads: Vec<usize>,
    previous: Vec<usize>,
    last_byte: [usize; 256],
}

impl Matcher {
    pub fn new(size: usize) -> Self {
        Self {
            heads: vec![NONE; HASH_SIZE],
            previous: vec![NONE; size],
            last_byte: [NONE; 256],
        }
    }

    pub fn find_best(&self, data: &[u8], position: usize, limit: usize) -> Match {
        let mut best = Match::default();
        if limit == 0 {
            return best;
        }
        if limit >= 2 {
            let mut candidate = self.heads[hash_pair(data, position)];
            // Не ограничиваем число кандидатов
            while candidate != NONE && position - candidate <= WINDOW {
                best.candidates += 1;
                let mut length = 0;
                while length < limit && data[candidate + length] == data[position + length] {
                    length += 1;
                }
                if length >= 2 && length > best.length {
                    best.length = length;
                    best.distance = position - candidate;
                    if length == limit {
                        return best;
                    }
                }
                candidate = self.previous[candidate];
            }
        }
        if best.length == 0 {
            let candidate = self.last_byte[data[position] as usize];
            if candidate != NONE && position - candidate <= WINDOW {
                best.candidates += 1;
                best.distance = position - candidate;
                best.length = 1;
            }
        }
        best
    }

    pub fn insert(&mut self, data: &[u8], position: usize) {
        self.last_byte[data[position] as usize] = position;
        if position + 1 < data.len() {
            let hash = hash_pair(data, position);
            self.previous[position] = self.heads[hash];
            self.heads[hash] = position;
        }
    }
}

pub(crate) fn hash_pair(data: &[u8], position: usize) -> usize {
    let pair = u16::from_le_bytes([data[position], data[position + 1]]);
    (u32::from(pair).wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}
