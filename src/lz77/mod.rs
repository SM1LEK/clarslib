//! LZ77(расстояние, длина, следующий байт)
//! Длина u8, ненулевое расстояние varint, следующий байт. Окно 4096, длина до 255.

use std::io::{self, Read};

use crate::{BLOCK_SIZE, invalid, varint};

const WINDOW: usize = 4096;
const MAX_MATCH: usize = 255;
pub(crate) mod matcher;
use matcher::Matcher;

// литерал 2 байта на 1 исходный; ссылка до 4 байт на >=2 исходных
pub(crate) const MAX_ENCODED_SIZE: usize = 3 + BLOCK_SIZE * 2;

pub(crate) fn encode(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.len() > BLOCK_SIZE {
        return Err(invalid("Переполнение блока LZ77"));
    }
    let mut output = Vec::new();
    varint::write(&mut output, data.len() as u64)?;

    let mut matcher = Matcher::new(data.len());
    let mut position = 0;
    while position < data.len() {
        let limit = MAX_MATCH.min(data.len() - position - 1);
        let best = matcher.find_best(data, position, limit);
        output.push(best.length as u8);
        if best.length != 0 {
            varint::write(&mut output, best.distance as u64)?;
        }
        output.push(data[position + best.length]);

        let next = position + best.length + 1;
        // +позиции внутри ссылки => схран. ближайшее совпадение
        for index in position..next {
            matcher.insert(data, index);
        }
        position = next;
    }
    Ok(output)
}

pub(crate) fn decode(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.is_empty() || data.len() > MAX_ENCODED_SIZE {
        return Err(invalid("Переполнение потока LZ77"));
    }
    let mut input = data;
    let size = varint::read_bounded(&mut input, BLOCK_SIZE)?;
    let mut output = Vec::with_capacity(size);
    while !input.is_empty() {
        let mut byte = [0];
        input.read_exact(&mut byte)?;
        let length = byte[0] as usize;
        let distance = if length == 0 {
            0
        } else {
            varint::read_bounded(&mut input, WINDOW)?
        };
        input.read_exact(&mut byte)?;
        if (length != 0 && distance == 0)
            || distance > output.len()
            || length + 1 > size - output.len()
        {
            return Err(invalid("Некорректная ссылка LZ77"));
        }
        // побайтово копируем для перекрывающихся ссылок, типа aaaaa
        for _ in 0..length {
            output.push(output[output.len() - distance]);
        }
        output.push(byte[0]);
    }
    if output.len() != size {
        return Err(invalid("Неверная длина результата LZ77"));
    }
    Ok(output)
}
