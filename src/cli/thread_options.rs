//! Парсим --threads без изменения путей OsString

use std::ffi::OsString;
use std::io;

pub fn parse_count(value: &str) -> io::Result<usize> {
    value
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=64).contains(n))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Потоков 1<=>64"))
}

pub fn parse(args: &[OsString]) -> io::Result<(Vec<OsString>, Option<usize>)> {
    let mut positional = Vec::new();
    let mut threads = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            if positional.first().is_some_and(|arg| arg == "create") {
                positional.push(arg.clone());
            }
            positional.extend(args.cloned());
            break;
        }
        if arg == "--threads" {
            if threads.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Лишний --threads",
                ));
            }
            let value = args
                .next()
                .and_then(|value| value.to_str())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Не указано --threads")
                })?;
            threads = Some(parse_count(value)?);
        } else {
            positional.push(arg.clone());
        }
    }
    Ok((positional, threads))
}
