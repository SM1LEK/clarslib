//! Проверка имён и порядка записей

use crate::invalid;
use std::collections::HashSet;
use std::fs::Metadata;
use std::io;
use std::path::{Path, PathBuf};

pub(super) const MAX_PATH_BYTES: usize = 4096;
const MAX_DEPTH: usize = 128;
const MAX_ENTRIES: usize = 100_000;

#[derive(Default)]
pub(super) struct Names {
    pub(super) entries: HashSet<String>,
    directories: HashSet<String>,
}

impl Names {
    pub(super) fn add(&mut self, name: &str, directory: bool) -> io::Result<()> {
        if name.is_empty() || name.len() > MAX_PATH_BYTES || name.split('/').count() > MAX_DEPTH {
            return Err(invalid("Неверный путь"));
        }
        for component in name.split('/') {
            check_component(component)?;
        }
        let key = name.to_lowercase();
        if let Some((parent, _)) = key.rsplit_once('/')
            && !self.directories.contains(parent)
        {
            return Err(invalid("Неверный путь"));
        }
        if self.entries.len() >= MAX_ENTRIES {
            return Err(invalid("Превышен лимит записей в архиве"));
        }
        if !self.entries.insert(key.clone()) {
            return Err(invalid(&format!("Повторяется путь: {name}")));
        }
        if directory {
            self.directories.insert(key);
        }
        Ok(())
    }
}

fn check_component(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 255
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|ch| ch.is_control() || "/\\:*?\"<>|".contains(ch))
    {
        return Err(invalid(&format!("Недопустимое имя файла/папки: {name:?}")));
    }
    let base = name.split('.').next().unwrap().trim_end().to_lowercase();
    let device = ["con", "prn", "aux", "nul", "conin$", "conout$"].contains(&base.as_str());
    if device {
        return Err(invalid(&format!("Занятое имя устройства: {name}")));
    }
    Ok(())
}

pub(super) fn file_name(path: &Path) -> io::Result<&str> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("Путь не UTF8"))?;
    check_component(name)?;
    Ok(name)
}

pub(super) fn is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // В том числе junction, т.к. is_symlink() не покрывает все reparse points
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(super) fn archive_path(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(invalid("Нет имени архива"));
    }
    if path.extension().is_none() {
        return Ok(path.with_extension("mrx"));
    }
    if !path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mrx"))
    {
        return Err(invalid("Неизвестное расширение"));
    }
    Ok(path.to_owned())
}
