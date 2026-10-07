//! Создание записей и нового архива

use super::{
    EncodingOptions, cleanup_error,
    paths::{Names, archive_path, file_name, is_link},
};
use crate::{BLOCK_SIZE, block::BlockWriter, invalid, varint};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

fn write_name(output: &mut impl Write, name: &str) -> io::Result<()> {
    varint::write(output, name.len() as u64)?;
    output.write_all(name.as_bytes())
}

fn pack_file(output: &mut impl Write, path: &Path) -> io::Result<()> {
    let mut input = File::open(path)?;
    let size = input.metadata()?.len();
    varint::write(output, size)?;
    let mut remaining = size;
    let mut buffer = vec![0; BLOCK_SIZE];
    while remaining != 0 {
        let count = remaining.min(BLOCK_SIZE as u64) as usize;
        input.read_exact(&mut buffer[..count])?;
        output.write_all(&buffer[..count])?;
        remaining -= count as u64;
    }
    if input.read(&mut buffer[..1])? != 0 {
        return Err(invalid(&format!(
            "Данные были изменены: {}",
            path.display()
        )));
    }
    Ok(())
}

fn pack_path(
    output: &mut impl Write,
    path: &Path,
    name: &str,
    destination: &Path,
    names: &mut Names,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if is_link(&metadata) {
        return Err(invalid(&format!("Не путь: {}", path.display())));
    }
    // Нужен пропуск самого себя
    if fs::canonicalize(path)? == destination {
        return Ok(());
    }
    let directory = metadata.is_dir();
    if !directory && !metadata.is_file() {
        return Err(invalid(&format!("Нетипичный файл: {}", path.display())));
    }
    names.add(name, directory)?;
    output.write_all(&[if directory { 2 } else { 1 }])?;
    write_name(output, name)?;
    if directory {
        let mut children: Vec<_> = fs::read_dir(path)?.collect::<io::Result<_>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let child_path = child.path();
            let child_name = format!("{name}/{}", file_name(&child_path)?);
            pack_path(output, &child_path, &child_name, destination, names)?;
        }
    } else {
        pack_file(output, path)?;
    }
    Ok(())
}

/// Только новый архив, исключение сбрасывает операцию
pub fn create(path: &Path, sources: &[PathBuf], protected: bool) -> io::Result<PathBuf> {
    create_with_threads(path, sources, protected, None)
}

/// Чанки
/// None => число доступных процессоров
/// Some(1) последовательно
/// Явное число рабочих потоков от 1 до 64
pub fn create_with_threads(
    path: &Path,
    sources: &[PathBuf],
    protected: bool,
    threads: Option<usize>,
) -> io::Result<PathBuf> {
    create_with_options(
        path,
        sources,
        EncodingOptions {
            protected,
            ..EncodingOptions::default()
        },
        threads,
    )
}

pub fn create_with_options(
    path: &Path,
    sources: &[PathBuf],
    options: EncodingOptions,
    threads: Option<usize>,
) -> io::Result<PathBuf> {
    let destination = archive_path(path)?;
    if sources.is_empty() {
        return Err(invalid("Нет данных"));
    }
    let sources: Vec<_> = sources
        .iter()
        .map(std::path::absolute)
        .collect::<io::Result<_>>()?;
    for source in &sources {
        file_name(source)?;
        fs::symlink_metadata(source)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)?;
    let result = (|| {
        let absolute_destination = fs::canonicalize(&destination)?;
        let mut writer = BlockWriter::with_threads(BufWriter::new(file), options, threads)?;
        let mut names = Names::default();
        for source in &sources {
            pack_path(
                &mut writer,
                source,
                file_name(source)?,
                &absolute_destination,
                &mut names,
            )?;
        }
        if names.entries.is_empty() {
            return Err(invalid("Нет данных"));
        }
        writer.write_all(&[0])?;
        let output = writer.finish()?;
        output
            .into_inner()
            .map_err(|error| error.into_error())?
            .sync_all()
    })();
    if let Err(error) = result {
        return Err(cleanup_error(&destination, false, error));
    }
    Ok(destination)
}
