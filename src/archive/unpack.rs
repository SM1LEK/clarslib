//! Проверка потока записей и извлечение файлов

use super::{
    Entry, Statistics, cleanup_error,
    paths::{MAX_PATH_BYTES, Names, archive_path},
};
use crate::{BLOCK_SIZE, block::BlockReader, invalid, varint};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::Path;

fn read_name(input: &mut impl Read) -> io::Result<String> {
    let length = varint::read_bounded(input, MAX_PATH_BYTES)?;
    if !(1..=MAX_PATH_BYTES).contains(&length) {
        return Err(invalid("Превышение длины пути"));
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|_| invalid("Путь не UTF8"))
}

fn process(
    path: &Path,
    destination: Option<&Path>,
    threads: Option<usize>,
    mut on_entry: impl FnMut(&Entry),
) -> io::Result<Statistics> {
    let file = File::open(archive_path(path)?)?;
    let mut stats = Statistics {
        archive_bytes: file.metadata()?.len(),
        ..Statistics::default()
    };
    let mut reader = BlockReader::with_threads(BufReader::new(file), threads)?;
    let mut names = Names::default();
    let mut buffer = vec![0; BLOCK_SIZE];
    loop {
        let mut kind = [0];
        reader.read_exact(&mut kind)?;
        if kind[0] == 0 {
            break;
        }
        if !matches!(kind[0], 1 | 2) {
            return Err(invalid("Неизвестный тип записи"));
        }
        let name = read_name(&mut reader)?;
        let directory = kind[0] == 2;
        names.add(&name, directory)?;
        let target = destination.map(|root| root.join(&name));
        let mut size = 0;
        if directory {
            stats.directories += 1;
            if let Some(target) = &target {
                fs::create_dir(target)?;
            }
        } else {
            stats.files += 1;
            size = varint::read(&mut reader)?;
            stats.original_bytes = stats
                .original_bytes
                .checked_add(size)
                .ok_or_else(|| invalid("Превышение лимита данных"))?;
            let mut output = target
                .as_ref()
                .map(|target| OpenOptions::new().write(true).create_new(true).open(target))
                .transpose()?;
            let mut remaining = size;
            while remaining != 0 {
                let count = remaining.min(BLOCK_SIZE as u64) as usize;
                reader.read_exact(&mut buffer[..count])?;
                if let Some(output) = &mut output {
                    output.write_all(&buffer[..count])?;
                }
                remaining -= count as u64;
            }
            if let Some(output) = output {
                output.sync_all()?;
            }
        }
        on_entry(&Entry {
            path: name,
            is_directory: directory,
            size,
        });
    }
    if names.entries.is_empty() {
        return Err(invalid("Пустой архив"));
    }
    let blocks = reader.finish()?;
    stats.protected = blocks.options.protected;
    stats.lz77 = blocks.options.lz77;
    stats.rans = blocks.options.rans;
    stats.compressed_blocks = blocks.compressed;
    stats.stored_blocks = blocks.stored;
    stats.corrected_bytes = blocks.corrected_bytes;
    Ok(stats)
}

/// Статистика
pub fn inspect(path: &Path, on_entry: impl FnMut(&Entry)) -> io::Result<Statistics> {
    inspect_with_threads(path, None, on_entry)
}

/// Статистика архива с выбранным числом потоков None => авто
pub fn inspect_with_threads(
    path: &Path,
    threads: Option<usize>,
    on_entry: impl FnMut(&Entry),
) -> io::Result<Statistics> {
    process(path, None, threads, on_entry)
}

/// Разархивация со сбросом при исключении
pub fn extract(path: &Path, destination: &Path) -> io::Result<Statistics> {
    extract_with_threads(path, destination, None)
}

/// Разархивация с выбранным числом рабочих потоков None => авто
pub fn extract_with_threads(
    path: &Path,
    destination: &Path,
    threads: Option<usize>,
) -> io::Result<Statistics> {
    fs::create_dir(destination)?; // Только новая папка
    match process(path, Some(destination), threads, |_| {}) {
        Ok(stats) => Ok(stats),
        Err(error) => Err(cleanup_error(destination, true, error)),
    }
}
