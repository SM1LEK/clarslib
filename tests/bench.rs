#![allow(dead_code, unused_imports)]

#[path = "../src/archive/mod.rs"]
mod archive;
#[path = "../src/block/mod.rs"]
mod block;
#[path = "../src/checksum.rs"]
mod checksum;
#[path = "../src/ecc.rs"]
mod ecc;
#[path = "../src/lz77/mod.rs"]
mod lz77;
#[path = "../src/parallel.rs"]
mod parallel;
#[path = "../src/rans/mod.rs"]
mod rans;
#[path = "../src/varint.rs"]
mod varint;

use archive::EncodingOptions;
use block::{BlockReader, BlockWriter};
use std::fs;
use std::hint::black_box;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const BLOCK_SIZE: usize = 64 * 1024;
const MIB: usize = 1024 * 1024;
const WARMUP: usize = 1;
static NEXT: AtomicU64 = AtomicU64::new(0);

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn setting(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .map(|value| value.parse::<usize>().expect("Нужно целое число"))
        .filter(|&value| value != 0)
        .unwrap_or(default)
}

fn input_size() -> usize {
    setting("CLARSLIB_BENCH_BYTES", MIB)
}

fn runs() -> usize {
    setting("CLARSLIB_BENCH_RUNS", 5)
}

fn random_bytes(size: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_u32;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect()
}

fn repeated_bytes(pattern: &[u8], size: usize) -> Vec<u8> {
    pattern.iter().copied().cycle().take(size).collect()
}

fn mixed_bytes(size: usize) -> Vec<u8> {
    let mut bytes = random_bytes(size);
    for (number, part) in bytes.chunks_mut(BLOCK_SIZE).enumerate() {
        if number.is_multiple_of(2) {
            part.fill(b'A');
        }
    }
    bytes
}

fn pack(input: &[u8], options: EncodingOptions, threads: usize) -> Vec<u8> {
    let mut writer = BlockWriter::with_threads(Vec::new(), options, Some(threads)).unwrap();
    writer.write_all(black_box(input)).unwrap();
    writer.finish().unwrap()
}

fn unpack(input: &[u8], original_size: usize, threads: usize) -> Vec<u8> {
    let mut reader = BlockReader::with_threads(black_box(input), Some(threads)).unwrap();
    let mut output = vec![0; original_size];
    reader.read_exact(&mut output).unwrap();
    reader.finish().unwrap();
    output
}

fn inputs() -> Vec<(&'static str, Vec<u8>)> {
    let size = input_size();
    let compressed = pack(
        &repeated_bytes(b"ABABABA\n", size),
        EncodingOptions::default(),
        1,
    );
    vec![
        ("Одинаковые байты", vec![b'A'; size]),
        ("Повторяющиеся строки", repeated_bytes(b"ABABABA\n", size)),
        (
            "Текст и исходники",
            repeated_bytes(
                b"fn main() { let value = 42; println!(\"value={value}\"); }\n",
                size,
            ),
        ),
        ("Все значения байта", (0..=255).cycle().take(size).collect()),
        ("Псевдослучайные данные", random_bytes(size)),
        ("Смешанные блоки", mixed_bytes(size)),
        ("Уже сжатый поток", compressed),
    ]
}

fn measure<T>(mut operation: impl FnMut() -> T, mut verify: impl FnMut(&T)) -> Duration {
    for _ in 0..WARMUP {
        let result = black_box(operation());
        verify(&result);
    }
    let mut times = Vec::with_capacity(runs());
    for _ in 0..runs() {
        let start = Instant::now();
        let result = black_box(operation());
        times.push(start.elapsed());
        verify(&result);
    }
    times.sort_unstable();
    let middle = times.len() / 2;
    if times.len().is_multiple_of(2) {
        times[middle - 1] / 2 + times[middle] / 2
    } else {
        times[middle]
    }
}

fn speed(bytes: usize, time: Duration) -> f64 {
    bytes as f64 / MIB as f64 / time.as_secs_f64().max(f64::MIN_POSITIVE)
}

fn print_time(name: &str, bytes: usize, time: Duration) {
    println!(
        "  {name} | исходных байт={bytes} | мс={:.3} | МиБ/с={:.3}",
        time.as_secs_f64() * 1000.0,
        speed(bytes, time)
    );
}

fn print_size(name: &str, original: usize, packed: usize) {
    if original == 0 {
        println!("  {name} | исходных байт=0 | архив={packed} | отношение=н/д | экономия=н/д");
    } else {
        let ratio = packed as f64 / original as f64;
        println!(
            "  {name} | исходных байт={original} | архив={packed} | архив/исходное={ratio:.6} | экономия={:.3}%",
            (1.0 - ratio) * 100.0
        );
    }
}

fn block_pack_speed() {
    for (name, input) in inputs() {
        let time = measure(
            || pack(&input, EncodingOptions::default(), 1),
            |output| assert_eq!(unpack(output, input.len(), 1), input),
        );
        print_time(name, input.len(), time);
    }
}

fn block_unpack_speed() {
    for (name, input) in inputs() {
        let packed = pack(&input, EncodingOptions::default(), 1);
        let time = measure(
            || unpack(&packed, input.len(), 1),
            |output| assert_eq!(output, &input),
        );
        print_time(name, input.len(), time);
    }
}

fn compression_sizes() {
    for (name, input) in inputs() {
        let output = pack(&input, EncodingOptions::default(), 1);
        assert_eq!(unpack(&output, input.len(), 1), input);
        print_size(name, input.len(), output.len());
    }
}

fn stage_speed() {
    for (name, input) in inputs() {
        for (stage, options) in [
            (
                "LZ77",
                EncodingOptions {
                    lz77: true,
                    rans: false,
                    protected: false,
                },
            ),
            (
                "rANS",
                EncodingOptions {
                    lz77: false,
                    rans: true,
                    protected: false,
                },
            ),
            ("LZ77+rANS", EncodingOptions::default()),
        ] {
            let encode = || {
                input
                    .chunks(BLOCK_SIZE)
                    .map(|part| {
                        let mut bytes = if options.lz77 {
                            lz77::encode(black_box(part)).unwrap()
                        } else {
                            part.to_vec()
                        };
                        if options.rans {
                            bytes = rans::encode(&bytes).unwrap();
                        }
                        bytes
                    })
                    .collect::<Vec<_>>()
            };
            let decode = |parts: &[Vec<u8>]| {
                let mut output = Vec::with_capacity(input.len());
                for part in parts {
                    let mut bytes = if options.rans {
                        rans::decode(black_box(part)).unwrap()
                    } else {
                        part.clone()
                    };
                    if options.lz77 {
                        bytes = lz77::decode(&bytes).unwrap();
                    }
                    output.extend_from_slice(&bytes);
                }
                output
            };
            let parts = encode();
            let encoded_size: usize = parts.iter().map(Vec::len).sum();
            let encode_time = measure(encode, |parts| assert_eq!(decode(parts), input));
            let decode_time = measure(|| decode(&parts), |output| assert_eq!(output, &input));
            println!("  {name} | этап={stage} | выходных байт={encoded_size}");
            print_time("Кодирование", input.len(), encode_time);
            print_time("Декодирование", input.len(), decode_time);
        }
    }
}

fn thread_scaling() {
    let input = mixed_bytes(input_size().max(BLOCK_SIZE * 32));
    let reference = pack(&input, EncodingOptions::default(), 1);
    let mut baseline = None;
    for threads in [1, 2, 4, 8] {
        let pack_time = measure(
            || pack(&input, EncodingOptions::default(), threads),
            |output| assert_eq!(output, &reference),
        );
        let unpack_time = measure(
            || unpack(&reference, input.len(), threads),
            |output| assert_eq!(output, &input),
        );
        let (first_pack, first_unpack) = *baseline.get_or_insert((pack_time, unpack_time));
        println!(
            "  потоков={threads} | ускорение упаковки={:.3} | ускорение распаковки={:.3}",
            first_pack.as_secs_f64() / pack_time.as_secs_f64().max(f64::MIN_POSITIVE),
            first_unpack.as_secs_f64() / unpack_time.as_secs_f64().max(f64::MIN_POSITIVE)
        );
        print_time("Упаковка", input.len(), pack_time);
        print_time("Распаковка", input.len(), unpack_time);
    }
}

fn protection_cost() {
    for (name, input) in inputs() {
        let mut baseline = None;
        for protected in [false, true] {
            let options = EncodingOptions {
                protected,
                ..Default::default()
            };
            let packed = pack(&input, options, 1);
            let pack_time = measure(
                || pack(&input, options, 1),
                |output| assert_eq!(unpack(output, input.len(), 1), input),
            );
            let unpack_time = measure(
                || unpack(&packed, input.len(), 1),
                |output| assert_eq!(output, &input),
            );
            let (base_size, base_pack, base_unpack) =
                *baseline.get_or_insert((packed.len(), pack_time, unpack_time));
            println!("  {name} | RS={protected} | архив={}", packed.len());
            print_time("Упаковка", input.len(), pack_time);
            print_time("Распаковка", input.len(), unpack_time);
            if protected {
                println!(
                    "  добавлено байт={} | прирост размера={:.3}% | время упаковки x{:.3} | время распаковки x{:.3}",
                    packed.len() - base_size,
                    (packed.len() as f64 / base_size as f64 - 1.0) * 100.0,
                    pack_time.as_secs_f64() / base_pack.as_secs_f64().max(f64::MIN_POSITIVE),
                    unpack_time.as_secs_f64() / base_unpack.as_secs_f64().max(f64::MIN_POSITIVE)
                );
            }
        }
    }
}

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "archiver-bench-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("cleanup bench sandbox");
    }
}

fn compare_tree(left: &Path, right: &Path) {
    let names = |path: &Path| {
        let mut names: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|item| item.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(left), names(right));
    for name in names(left) {
        let a = left.join(&name);
        let b = right.join(&name);
        if a.is_dir() {
            assert!(b.is_dir());
            compare_tree(&a, &b);
        } else {
            assert_eq!(fs::read(a).unwrap(), fs::read(b).unwrap());
        }
    }
}

fn files_size(path: &Path) -> u64 {
    let metadata = fs::symlink_metadata(path).unwrap();
    assert!(
        !metadata.file_type().is_symlink(),
        "Ссылки в корпусе запрещены"
    );
    if metadata.is_dir() {
        fs::read_dir(path)
            .unwrap()
            .map(|entry| files_size(&entry.unwrap().path()))
            .sum()
    } else {
        assert!(metadata.is_file(), "Необычный файл в корпусе");
        metadata.len()
    }
}

fn file_case(name: &str, source: &Path, sandbox: &Sandbox) {
    let original = usize::try_from(files_size(source)).unwrap();
    let sources = [source.to_path_buf()];
    let destination = sandbox.path("result.mrx");
    let output = sandbox.path("out");
    let options = EncodingOptions::default();
    let mut pack_times = Vec::new();
    let mut unpack_times = Vec::new();
    let mut archive_size = None;
    for number in 0..WARMUP + runs() {
        let start = Instant::now();
        let actual =
            archive::create_with_options(&destination, &sources, options, Some(1)).unwrap();
        let pack_time = start.elapsed();
        let size = fs::metadata(&actual).unwrap().len();
        assert_eq!(size, *archive_size.get_or_insert(size));
        let start = Instant::now();
        let stats = archive::extract_with_threads(&actual, &output, Some(1)).unwrap();
        let unpack_time = start.elapsed();
        assert_eq!(stats.original_bytes, original as u64);
        let restored = output.join(source.file_name().unwrap());
        if source.is_dir() {
            compare_tree(source, &restored);
        } else {
            assert_eq!(fs::read(source).unwrap(), fs::read(restored).unwrap());
        }
        if number >= WARMUP {
            pack_times.push(pack_time);
            unpack_times.push(unpack_time);
        }
        fs::remove_file(actual).unwrap();
        fs::remove_dir_all(&output).unwrap();
    }
    let median = |times: &mut Vec<Duration>| {
        times.sort_unstable();
        let middle = times.len() / 2;
        if times.len().is_multiple_of(2) {
            times[middle - 1] / 2 + times[middle] / 2
        } else {
            times[middle]
        }
    };
    println!("  Набор: {name}");
    print_size(
        "Файловый архив",
        original,
        usize::try_from(archive_size.unwrap()).unwrap(),
    );
    print_time("Создание файла архива", original, median(&mut pack_times));
    print_time("Извлечение на диск", original, median(&mut unpack_times));
}

fn file_archive_speed_and_overhead() {
    let sandbox = Sandbox::new();
    let empty = sandbox.path("empty");
    fs::create_dir(&empty).unwrap();
    for number in 0..128 {
        fs::write(empty.join(format!("{number:04}.bin")), []).unwrap();
    }
    file_case("128 пустых файлов", &empty, &sandbox);
    let small = sandbox.path("small");
    fs::create_dir(&small).unwrap();
    let small_data = random_bytes(128 * 1024);
    for (number, bytes) in small_data.chunks(128).enumerate() {
        fs::write(small.join(format!("{number:04}.bin")), bytes).unwrap();
    }
    file_case("1024 файла по 128 байт", &small, &sandbox);
    let large = sandbox.path("large.bin");
    fs::write(&large, mixed_bytes(input_size().max(BLOCK_SIZE * 32))).unwrap();
    file_case("Один большой файл", &large, &sandbox);
    let mixed = sandbox.path("mixed");
    fs::create_dir(&mixed).unwrap();
    for (number, (_, input)) in inputs().into_iter().enumerate() {
        fs::write(mixed.join(format!("{number:02}.bin")), input).unwrap();
    }
    file_case("Смешанные данные", &mixed, &sandbox);
    if let Some(path) = std::env::var_os("CLARSLIB_BENCH_INPUT") {
        let path = fs::canonicalize(path).unwrap();
        assert!(path.is_dir(), "CLARSLIB_BENCH_INPUT должна быть папкой");
        let temporary = fs::canonicalize(&sandbox.0).unwrap();
        assert!(
            !temporary.starts_with(&path),
            "Корпус не должен включать временную папку бенчмарка"
        );
        file_case("Пользовательский корпус", &path, &sandbox);
    }
}

#[cfg(target_os = "linux")]
fn memory_peak_kib() -> u64 {
    let status = fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[cfg(target_os = "windows")]
fn memory_peak_kib() -> u64 {
    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    let size = std::mem::size_of::<ProcessMemoryCounters>() as u32;
    let mut counters = ProcessMemoryCounters {
        cb: size,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
    };
    let result = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) };
    assert!(
        result != 0,
        "Не удалось измерить пиковую память: {}",
        io::Error::last_os_error()
    );
    counters.peak_working_set_size as u64 / 1024
}

fn memory_child(args: &[std::ffi::OsString]) {
    let source = PathBuf::from(&args[2]);
    let destination = PathBuf::from(&args[3]);
    let protected = args[4] == "true";
    if args[1] == "pack" {
        archive::create_with_options(
            &destination,
            &[source],
            EncodingOptions {
                protected,
                ..Default::default()
            },
            Some(1),
        )
        .unwrap();
    } else {
        assert_eq!(args[1], "unpack");
        archive::extract_with_threads(&source, &destination, Some(1)).unwrap();
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    println!("{}", memory_peak_kib());
}

fn memory_peak() {
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        let sandbox = Sandbox::new();
        let source = sandbox.path("input.bin");
        fs::write(&source, mixed_bytes(input_size().max(BLOCK_SIZE * 32))).unwrap();
        for protected in [false, true] {
            let mut pack_peaks = Vec::new();
            let mut unpack_peaks = Vec::new();
            for number in 0..WARMUP + runs() {
                let packed = sandbox.path("memory.mrx");
                let restored = sandbox.path("memory-out");
                for (operation, input, output, peaks) in [
                    ("pack", &source, &packed, &mut pack_peaks),
                    ("unpack", &packed, &restored, &mut unpack_peaks),
                ] {
                    let result = Command::new(std::env::current_exe().unwrap())
                        .arg("--memory-child")
                        .arg(operation)
                        .arg(input)
                        .arg(output)
                        .arg(protected.to_string())
                        .stdin(Stdio::null())
                        .output()
                        .unwrap();
                    assert!(
                        result.status.success(),
                        "{}",
                        String::from_utf8_lossy(&result.stderr)
                    );
                    let peak = String::from_utf8(result.stdout)
                        .unwrap()
                        .trim()
                        .parse::<u64>()
                        .unwrap();
                    if number >= WARMUP {
                        peaks.push(peak);
                    }
                }
                assert_eq!(
                    fs::read(&source).unwrap(),
                    fs::read(restored.join("input.bin")).unwrap()
                );
                fs::remove_file(packed).unwrap();
                fs::remove_dir_all(restored).unwrap();
            }
            let median = |values: &mut Vec<u64>| {
                values.sort_unstable();
                let middle = values.len() / 2;
                if values.len().is_multiple_of(2) {
                    values[middle - 1] as f64 / 2.0 + values[middle] as f64 / 2.0
                } else {
                    values[middle] as f64
                }
            };
            let metric = if cfg!(target_os = "windows") {
                "пик рабочего набора"
            } else {
                "пик RSS"
            };
            println!(
                "  RS={protected} | потоков=1 | {metric} упаковки={:.3} МиБ | {metric} распаковки={:.3} МиБ",
                median(&mut pack_peaks) / 1024.0,
                median(&mut unpack_peaks) / 1024.0
            );
        }
        #[cfg(target_os = "windows")]
        println!("  PeakWorkingSetSize отдельного процесса; включает код, библиотеки и буферы");
        #[cfg(target_os = "linux")]
        println!("  VmHWM отдельного процесса; включает код, библиотеки и буферы");
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    println!("  Измерение пиковой памяти поддерживается на Windows и Linux");
}

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--memory-child") {
        assert_eq!(args.len(), 5);
        memory_child(&args);
        return;
    }
    assert!(args.is_empty(), "Настройки задаются через CLARSLIB_BENCH_*");
    println!("CLARSLIB: бенчмарки");
    println!(
        "ОС={} | архитектура={} | доступных процессоров={} | размер набора={} | прогрев={} | измерений={} | результат=медиана",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        input_size(),
        WARMUP,
        runs()
    );
    if cfg!(debug_assertions) {
        println!("ВНИМАНИЕ: debug-сборка. Для измерений нужен --release");
    }
    println!("Скорость: МиБ исходных данных/с. Файловый ввод-вывод: прогретый кеш ОС, без fsync.");
    let benches: &[(&str, &str, fn())] = &[
        ("Блочный поток", "Скорость упаковки", block_pack_speed),
        ("Блочный поток", "Скорость распаковки", block_unpack_speed),
        ("Блочный поток", "Размеры сжатых данных", compression_sizes),
        ("Алгоритмы", "Скорость отдельных этапов", stage_speed),
        (
            "Многопоточность",
            "Масштабирование по потокам",
            thread_scaling,
        ),
        ("Reed–Solomon", "Цена защиты", protection_cost),
        (
            "Команды и файлы",
            "Скорость файлового архива и накладные расходы",
            file_archive_speed_and_overhead,
        ),
        ("Память", "Пиковая память отдельного процесса", memory_peak),
    ];
    let mut previous_group = "";
    for &(group, name, bench) in benches {
        if group != previous_group {
            println!("\n{group}");
            previous_group = group;
        }
        println!("{name}");
        bench();
    }
    println!("\nБенчмарков завершено: {}", benches.len());
}
