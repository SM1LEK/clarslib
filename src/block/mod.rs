//! Сжатие, контрольные суммы, защита

mod reader;
mod writer;
pub(crate) use reader::BlockReader;
pub(crate) use writer::BlockWriter;

use crate::checksum::crc32;

const SIGNATURE: &[u8; 8] = b"CLARSLIB";
pub(crate) const ARCHIVE_HEADER_SIZE: usize = 13;
pub(crate) const BLOCK_HEADER_SIZE: usize = 12;

// Номер блока известен rw. Связываем его с CRC и не храним u64 в архиве
fn header_crc(sequence: u64, fields: &[u8]) -> u32 {
    let mut bytes = sequence.to_le_bytes().to_vec();
    bytes.extend_from_slice(fields);
    crc32(&bytes)
}

#[derive(Debug, Default)]
pub(crate) struct BlockStats {
    pub options: crate::archive::EncodingOptions,
    pub compressed: u64,
    pub stored: u64,
    pub corrected_bytes: u64,
}
