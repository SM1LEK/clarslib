//! Файлы и каталоги поверх блочного потока CLARSLIB

use std::fs;
use std::io;
use std::path::Path;
mod pack;
mod paths;
mod unpack;
pub use pack::{create, create_with_options, create_with_threads};
pub use unpack::{extract, extract_with_threads, inspect, inspect_with_threads};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodingOptions {
    pub lz77: bool,
    pub rans: bool,
    pub protected: bool,
}

impl Default for EncodingOptions {
    fn default() -> Self {
        Self {
            lz77: true,
            rans: true,
            protected: false,
        }
    }
}

impl EncodingOptions {
    pub(crate) fn flags(self) -> u8 {
        u8::from(self.lz77) | (u8::from(self.rans) << 1) | (u8::from(self.protected) << 2)
    }

    pub(crate) fn from_flags(flags: u8) -> io::Result<Self> {
        if flags & !7 != 0 {
            return Err(crate::invalid("Неизвестные флаги кодирования"));
        }
        Ok(Self {
            lz77: flags & 1 != 0,
            rans: flags & 2 != 0,
            protected: flags & 4 != 0,
        })
    }
}

#[derive(Debug)]
pub struct Entry {
    pub path: String,
    pub is_directory: bool,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct Statistics {
    pub files: u64,
    pub directories: u64,
    pub original_bytes: u64,
    pub archive_bytes: u64,
    pub protected: bool,
    pub lz77: bool,
    pub rans: bool,
    pub compressed_blocks: u64,
    pub stored_blocks: u64,
    pub corrected_bytes: u64,
}

fn cleanup_error(path: &Path, directory: bool, original: io::Error) -> io::Error {
    let result = if directory {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match result {
        Ok(()) => original,
        Err(error) => io::Error::new(
            original.kind(),
            format!(
                "{original}, очистка не завершена {}: {error}",
                path.display()
            ),
        ),
    }
}
