//! Нормализация частот и компактное представление модели rANS

use super::{MAX_HEADER_SIZE, TOTAL};
use crate::{invalid, lz77::MAX_ENCODED_SIZE, varint};
use std::io::{self, Read};

pub(super) fn write_model(output: &mut Vec<u8>, frequencies: &[u16; 256]) -> io::Result<()> {
    let symbols: Vec<_> = (0..256).filter(|&s| frequencies[s] != 0).collect();
    output.push((symbols.len() - 1) as u8);
    if symbols.len() <= 32 {
        output.extend(symbols.iter().map(|&s| s as u8));
    } else {
        let mut bitmap = [0_u8; 32];
        for &symbol in &symbols {
            bitmap[symbol / 8] |= 1 << (symbol % 8);
        }
        output.extend_from_slice(&bitmap);
    }
    // Последняя частота из суммы 4096, для одного символа нет частот
    for &symbol in &symbols[..symbols.len() - 1] {
        varint::write(output, u64::from(frequencies[symbol]))?;
    }
    Ok(())
}

pub(crate) struct Header {
    pub size: usize,
    pub frequencies: [u16; 256],
    pub state_offset: usize,
}

pub(crate) fn read_header(data: &[u8]) -> io::Result<Header> {
    if data.len() > MAX_HEADER_SIZE + 2 * MAX_ENCODED_SIZE {
        return Err(invalid("Переполнение потока rANS"));
    }
    let mut input = data;
    let size = varint::read_bounded(&mut input, MAX_ENCODED_SIZE)?;
    let model_offset = data.len() - input.len();
    let mut frequencies = [0_u16; 256];
    if size == 0 {
        if !input.is_empty() {
            return Err(invalid("Лишние данные rANS"));
        }
        return Ok(Header {
            size,
            frequencies,
            state_offset: model_offset,
        });
    }
    let mut byte = [0];
    input.read_exact(&mut byte)?;
    let distinct = usize::from(byte[0]) + 1;
    if distinct > size {
        return Err(invalid("Переполнение символов rANS"));
    }
    let symbols = if distinct <= 32 {
        let mut symbols = vec![0_u8; distinct];
        input.read_exact(&mut symbols)?;
        if symbols.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid("Нарушен порядок символов rANS"));
        }
        symbols
    } else {
        let mut bitmap = [0_u8; 32];
        input.read_exact(&mut bitmap)?;
        let symbols: Vec<_> = (0..256)
            .filter(|&s| bitmap[s / 8] & (1 << (s % 8)) != 0)
            .map(|s| s as u8)
            .collect();
        if symbols.len() != distinct {
            return Err(invalid("Неверное число битов rANS"));
        }
        symbols
    };
    let mut sum = 0;
    for (index, &symbol) in symbols.iter().enumerate() {
        let frequency = if index + 1 == distinct {
            TOTAL as usize - sum
        } else {
            varint::read_bounded(&mut input, TOTAL as usize - 1)?
        };
        // Оставляем хотя бы 1 каждому ещё не прочитанному символу
        if frequency == 0 || sum + frequency + distinct - index - 1 > TOTAL as usize {
            return Err(invalid("Неверные частоты rANS"));
        }
        frequencies[symbol as usize] = frequency as u16;
        sum += frequency;
    }
    let state_offset = data.len() - input.len();
    if input.len() < 4 {
        return Err(invalid("Оборвано состояние rANS"));
    }
    Ok(Header {
        size,
        frequencies,
        state_offset,
    })
}

pub(crate) fn frequencies(data: &[u8]) -> [u16; 256] {
    let mut counts = [0_u32; 256];
    for &symbol in data {
        counts[symbol as usize] += 1;
    }
    let mut frequencies = [0_u16; 256];
    if data.is_empty() {
        return frequencies;
    }

    // Всем символам даём 1, остаток разделяеи по частотам и по max(дробным частям)
    let distinct = counts.iter().filter(|&&count| count != 0).count() as u32;
    let available = TOTAL - distinct;
    let mut sum = 0;
    let mut remainders = Vec::new();
    for (symbol, &count) in counts.iter().enumerate() {
        if count != 0 {
            let weight = u64::from(count) * u64::from(available);
            let frequency = 1 + (weight / data.len() as u64) as u32;
            frequencies[symbol] = frequency as u16;
            sum += frequency;
            remainders.push((weight % data.len() as u64, symbol));
        }
    }
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for &(_, symbol) in remainders.iter().take((TOTAL - sum) as usize) {
        frequencies[symbol] += 1;
    }
    frequencies
}

pub(super) fn cumulative(frequencies: &[u16; 256]) -> [u32; 256] {
    let mut starts = [0; 256];
    let mut sum = 0;
    for (symbol, &frequency) in frequencies.iter().enumerate() {
        starts[symbol] = sum;
        sum += u32::from(frequency);
    }
    starts
}
