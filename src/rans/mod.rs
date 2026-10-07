//! Байтовый rANS, одно 32 битное состояние, статическая модель с точностью 12 бит

use std::io;

use crate::{invalid, lz77::MAX_ENCODED_SIZE, varint};

const SCALE_BITS: u32 = 12;
const TOTAL: u32 = 1 << SCALE_BITS;
const LOWER_BOUND: u32 = 1 << 23;
// Минимум непустого потока => длина + D-1 + один символ + состояние
pub(crate) const MIN_HEADER_SIZE: usize = 7;
const MAX_HEADER_SIZE: usize = 3 + 1 + 32 + 255 * 2 + 4;

mod model;
use model::{cumulative, write_model};
pub(crate) use model::{frequencies, read_header};
pub(crate) fn encode(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.len() > MAX_ENCODED_SIZE {
        return Err(invalid("Переполнение входных данных rANS"));
    }
    if data.is_empty() {
        return Ok(vec![0]);
    }
    let frequencies = frequencies(data);
    let starts = cumulative(&frequencies);
    let mut state = LOWER_BOUND;
    let mut emitted = Vec::new();
    // ANS == стек => парсим с сконца
    for &symbol in data.iter().rev() {
        let symbol = symbol as usize;
        let frequency = u32::from(frequencies[symbol]);
        let threshold = ((LOWER_BOUND >> SCALE_BITS) << 8) * frequency;
        while state >= threshold {
            emitted.push(state as u8);
            state >>= 8;
        }
        state = (state / frequency) * TOTAL + state % frequency + starts[symbol];
    }

    let mut output = Vec::with_capacity(MAX_HEADER_SIZE + emitted.len());
    varint::write(&mut output, data.len() as u64)?;
    write_model(&mut output, &frequencies)?;
    output.extend_from_slice(&state.to_le_bytes());
    output.extend(emitted.into_iter().rev());
    Ok(output)
}

pub(crate) fn decode(data: &[u8]) -> io::Result<Vec<u8>> {
    let header = read_header(data)?;
    let size = header.size;
    if size == 0 {
        return Ok(Vec::new());
    }
    let frequencies = header.frequencies;
    let starts = cumulative(&frequencies);
    let mut symbols = [0_u8; TOTAL as usize];
    for symbol in 0..256 {
        let start = starts[symbol] as usize;
        let end = start + frequencies[symbol] as usize;
        symbols[start..end].fill(symbol as u8);
    }

    let mut position = header.state_offset + 4;
    let mut state = u32::from_le_bytes(data[header.state_offset..position].try_into().unwrap());
    if !(LOWER_BOUND..LOWER_BOUND * 256).contains(&state) {
        return Err(invalid("Некорректное состояние rANS"));
    }
    let mut output = Vec::with_capacity(size);
    for _ in 0..size {
        let slot = state & (TOTAL - 1);
        let symbol = symbols[slot as usize] as usize;
        output.push(symbol as u8);
        state = u32::from(frequencies[symbol]) * (state >> SCALE_BITS) + slot - starts[symbol];
        while state < LOWER_BOUND {
            let byte = *data
                .get(position)
                .ok_or_else(|| invalid("Оборван поток rANS"))?;
            state = (state << 8) | u32::from(byte);
            position += 1;
        }
    }
    if position != data.len() || state != LOWER_BOUND {
        return Err(invalid("Оборван поток rANS"));
    }
    Ok(output)
}
