//! CRC-32/ISO-HDLC табличный расчёт контрольной суммы

const fn bit_step(crc: u32) -> u32 {
    if crc & 1 == 0 {
        crc >> 1
    } else {
        (crc >> 1) ^ 0xEDB8_8320
    }
}

const fn make_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < table.len() {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = bit_step(crc);
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

const TABLE: [u32; 256] = make_table();

pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        let index = (crc as u8 ^ byte) as usize;
        crc = (crc >> 8) ^ TABLE[index];
    }
    !crc
}
