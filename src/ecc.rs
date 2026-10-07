//! Систематический RS(255,223) из reed-solomon, 32 проверочных байта
//! Последнее слово урезано до k данных + 32 чётности, без заполнения

use std::io;

use reed_solomon::{Decoder, Encoder};

use crate::{BLOCK_SIZE, invalid};

const DATA_SIZE: usize = 223;
const PARITY_SIZE: usize = 32;
const WORD_SIZE: usize = DATA_SIZE + PARITY_SIZE;

pub(crate) fn encoded_size(size: usize) -> usize {
    size + size.div_ceil(DATA_SIZE) * PARITY_SIZE
}

pub(crate) fn encode(data: &[u8]) -> Vec<u8> {
    let encoder = Encoder::new(PARITY_SIZE);
    let mut output = Vec::with_capacity(encoded_size(data.len()));
    for part in data.chunks(DATA_SIZE) {
        output.extend_from_slice(&encoder.encode(part));
    }
    output
}

pub(crate) fn decode(data: &[u8], original_size: usize) -> io::Result<(Vec<u8>, u64)> {
    if original_size > BLOCK_SIZE || data.len() != encoded_size(original_size) {
        return Err(invalid("Некорректный размер данных RS"));
    }
    let decoder = Decoder::new(PARITY_SIZE);
    let mut output = Vec::with_capacity(original_size);
    let mut corrected = 0;
    for word in data.chunks(WORD_SIZE) {
        let (restored, count) = decoder
            .correct_err_count(word, None)
            .map_err(|_| invalid("RS не удалось исправить повреждение"))?;
        output.extend_from_slice(restored.data());
        corrected += count as u64;
    }
    Ok((output, corrected))
}
