//! Командный блок архивера

use archiver::archive::EncodingOptions;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

mod commands;
mod thread_options;

use commands::{create, extract, info};

fn argument_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub(crate) fn run(args: &[OsString]) -> io::Result<()> {
    let (args, threads) = thread_options::parse(args)?;
    let command = args
        .first()
        .ok_or_else(|| argument_error("Не указана команда"))?;
    match command.to_str() {
        Some("create") => {
            let mut options = EncodingOptions::default();
            let mut start = 1;
            while let Some(arg) = args.get(start).and_then(|arg| arg.to_str()) {
                match arg {
                    "--no-lz77" => options.lz77 = false,
                    "--no-rans" => options.rans = false,
                    "--protect" => options.protected = true,
                    "--" => {
                        start += 1;
                        break;
                    }
                    _ if arg.starts_with("--") => {
                        return Err(argument_error(&format!("Неизвестный параметр: {arg}")));
                    }
                    _ => break,
                }
                start += 1;
            }
            if args.len() < start + 2 {
                return Err(argument_error("create => два параметра"));
            }
            let sources: Vec<_> = args[start + 1..].iter().map(PathBuf::from).collect();
            create(Path::new(&args[start]), &sources, options, threads)
        }
        Some("extract") => {
            if args.len() != 3 {
                return Err(argument_error("extract => два параметра"));
            }
            extract(Path::new(&args[1]), Path::new(&args[2]), threads)
        }
        Some("info") => {
            if args.len() != 2 {
                return Err(argument_error("info => один параметр"));
            }
            info(Path::new(&args[1]), threads)
        }
        _ => Err(argument_error("Неизвестная команда")),
    }
}
