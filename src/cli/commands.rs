use archiver::archive::{self, EncodingOptions, Statistics};
use std::io;
use std::path::{Path, PathBuf};

fn print_statistics(stats: &Statistics) {
    println!("Файлов: {}, папок: {}", stats.files, stats.directories);
    println!("Исходные файлы: {} байт", stats.original_bytes);
    println!("Архив: {}", stats.archive_bytes);
    println!(
        "LZ77: {}, rANS: {}",
        if stats.lz77 { "+" } else { "-" },
        if stats.rans { "+" } else { "-" }
    );
    println!("Сжатых блоков: {}", stats.compressed_blocks);
    println!("Блоков без сжатия: {}", stats.stored_blocks);
    println!(
        "Защита: {}",
        if stats.protected { "RS(255,223)" } else { "-" }
    );
    println!("Исправлено байт: {}", stats.corrected_bytes);
}

pub(super) fn create(
    path: &Path,
    sources: &[PathBuf],
    options: EncodingOptions,
    threads: Option<usize>,
) -> io::Result<()> {
    let path = archive::create_with_options(path, sources, options, threads)?;
    println!("Сжато => {}", path.display());
    Ok(())
}

pub(super) fn extract(path: &Path, destination: &Path, threads: Option<usize>) -> io::Result<()> {
    let stats = archive::extract_with_threads(path, destination, threads)?;
    print_statistics(&stats);
    println!("Распакованно => {}", destination.display());
    Ok(())
}

pub(super) fn info(path: &Path, threads: Option<usize>) -> io::Result<()> {
    let stats = archive::inspect_with_threads(path, threads, |entry| {
        if entry.is_directory {
            println!("Папка | {}/", entry.path);
        } else {
            println!("{} байт | {}", entry.size, entry.path);
        }
    })?;
    print_statistics(&stats);
    Ok(())
}
