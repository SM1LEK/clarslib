//! Формат CLARSLIB. Классический LZ77, байтовый rANS и Reed–Solomon

pub mod archive;
mod block;
mod checksum;
mod ecc;
mod lz77;
mod parallel;
mod rans;
mod varint;

use std::io;

const BLOCK_SIZE: usize = 64 * 1024;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
