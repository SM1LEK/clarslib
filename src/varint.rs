//! Канонический unsigned LEB128 (7 бит на байт, бит 7 на продолжение)

use std::io::{self, Read, Write};

use crate::invalid;

pub(crate) fn write(output: &mut impl Write, mut value: u64) -> io::Result<()> {
    loop {
        let byte = (value as u8 & 0x7F) | if value >= 128 { 0x80 } else { 0 };
        output.write_all(&[byte])?;
        value >>= 7;
        if value == 0 {
            return Ok(());
        }
    }
}

pub(crate) fn read(input: &mut impl Read) -> io::Result<u64> {
    let mut value = 0_u64;
    // u64 вмещает 64 бита по 7 бит на байт максимум (9*7 = 63 бита + 1 бит в десятом байте)
    for index in 0..10 {
        let mut byte = [0];
        input.read_exact(&mut byte)?;
        let byte = byte[0];
        // Десятый байт u64 содержит только один бит; продолжение тоже запрещено.
        if index == 9 && byte > 1 {
            return Err(invalid("Переполнение varint"));
        }
        value |= u64::from(byte & 0x7F) << (index * 7);
        if byte & 0x80 == 0 {
            if index != 0 && byte == 0 {
                return Err(invalid("Неканоническая запись varint"));
            }
            return Ok(value);
        }
    }
    Err(invalid("Слишком длинный varint"))
}

pub(crate) fn read_bounded(input: &mut impl Read, maximum: usize) -> io::Result<usize> {
    let value = read(input)?;
    if value > maximum as u64 {
        return Err(invalid("переполнение значения varint"));
    }
    Ok(value as usize)
}
