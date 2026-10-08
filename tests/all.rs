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
use block::{ARCHIVE_HEADER_SIZE, BLOCK_HEADER_SIZE, BlockReader, BlockWriter};
use lz77::matcher::{Matcher, hash_pair};
use parallel::Executor;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
const BLOCK_SIZE: usize = 64 * 1024;
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
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

fn length_prefix_size(bytes: &[u8]) -> usize {
    let mut rest = bytes;
    crate::varint::read(&mut rest).unwrap();
    bytes.len() - rest.len()
}

fn crc_standard_vectors() {
    assert_eq!(checksum::crc32(b""), 0);
    assert_eq!(checksum::crc32(b"123456789"), 0xCBF4_3926);
    assert_eq!(checksum::crc32(b"ABABABA"), 0xDBC2_50ED);
    println!("  input_len=0,9,7 | crc=00000000,CBF43926,DBC250ED");
}

fn crc_table_matches_bit_trace() {
    let mut inputs = vec![vec![], (0..=255).collect(), random_bytes(BLOCK_SIZE)];
    inputs.extend((0..=255).map(|byte| vec![byte]));
    inputs.extend((0..64).map(random_bytes));
    for input in inputs {
        let mut steps = 0;
        let bitwise = bitwise_crc(&input, &mut steps);
        assert_eq!(checksum::crc32(&input), bitwise);
        assert_eq!(steps, input.len() * 9);
    }
    println!("  inputs=322 | sizes=0,1,64..127,256,65536 | table_crc=bitwise_crc");
}

fn bitwise_crc(data: &[u8], steps: &mut usize) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte);
        *steps += 1;
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xEDB8_8320
            };
            *steps += 1;
        }
    }
    !crc
}

// naive_lz независимо проверяет все расстояния без хеш-цепочек
fn lz_hash_matches_reference_for_all_short_ternary_inputs() {
    for size in 0..=8 {
        for mut value in 0..3_usize.pow(size) {
            let input: Vec<_> = (0..size)
                .map(|_| {
                    let byte = (value % 3) as u8;
                    value /= 3;
                    byte
                })
                .collect();
            let encoded = lz77::encode(&input).unwrap();
            assert_eq!(encoded, naive_lz(&input), "{input:?}");
            assert_eq!(lz77::decode(&encoded).unwrap(), input);
        }
    }
    println!("  alphabet=3 | max_len=8 | inputs=9841 | hash_lz=naive_lz | decode=input");
}

fn lz_hash_matches_reference_for_repeated_prefixes() {
    let input: Vec<u8> = (0..=255).flat_map(|byte| [0, byte, 255]).collect();
    let input = [input.repeat(3), vec![0, 42, 254, 1]].concat();
    let encoded = lz77::encode(&input).unwrap();
    assert_eq!(encoded, naive_lz(&input));
    assert_eq!(lz77::decode(&encoded).unwrap(), input);
    println!("  input_len=2308 | hash_lz=naive_lz | decode=input");
}

fn lz_paper_example_and_overlap() {
    let encoded = lz77::encode(b"ABABABA").unwrap();
    assert_eq!(encoded, [7, 0, 65, 0, 66, 4, 2, 65]);
    assert_eq!(lz77::decode(&encoded).unwrap(), b"ABABABA");
    assert_eq!(lz77::encode(b"AAAAAA").unwrap(), [6, 0, 65, 4, 1, 65]);
    println!("  ABABABA=07 00 41 00 42 04 02 41 | AAAAAA=06 00 41 04 01 41");
}

fn naive_lz(data: &[u8]) -> Vec<u8> {
    let mut result = Vec::new();
    varint::write(&mut result, data.len() as u64).unwrap();
    let mut position = 0;
    while position < data.len() {
        let mut best = (0, 0);
        for distance in 1..=4096.min(position) {
            let mut length = 0;
            while length < 255.min(data.len() - position - 1)
                && data[position - distance + length] == data[position + length]
            {
                length += 1;
            }
            if length > best.1 {
                best = (distance, length);
            }
        }
        result.push(best.1 as u8);
        if best.1 != 0 {
            varint::write(&mut result, best.0 as u64).unwrap();
        }
        result.push(data[position + best.1]);
        position += best.1 + 1;
    }
    result
}

fn lz_search_matches_exhaustive_reference() {
    for input in [
        b"abXabYabZ".to_vec(),
        random_bytes(1024),
        random_bytes(1024).into_iter().map(|b| b % 4).collect(),
        vec![b'A'; 1024],
    ] {
        assert_eq!(lz77::encode(&input).unwrap(), naive_lz(&input));
    }
    let mut steps = Vec::new();
    encode_steps(b"abXabYabZ", |s| steps.push(s)).unwrap();
    let step = steps.iter().find(|s| s.position == 6).unwrap();
    assert_eq!((step.distance, step.length), (3, 2));
    println!("  inputs=4 | probe=abXabYabZ | pos=6 | distance=3 | length=2");
}

fn lz_match_and_window_boundaries() {
    let mut steps = Vec::new();
    let input = vec![b'A'; 258];
    assert_eq!(
        encode_steps(&input, |s| steps.push(s)).unwrap(),
        lz77::encode(&input).unwrap()
    );
    assert_eq!(steps[1].length, 255);
    assert_eq!(steps[2].length, 0); // Последний байт обязан остаться литералом.
    for gap in [4094, 4095] {
        let input = [b"AB".as_slice(), &vec![0; gap], b"AB!"].concat();
        let mut steps = Vec::new();
        let encoded = encode_steps(&input, |s| steps.push(s)).unwrap();
        assert_eq!(lz77::decode(&encoded).unwrap(), input);
        let last = steps.iter().find(|s| s.position == gap + 3).unwrap();
        assert_eq!(last.distance, if gap == 4094 { 4096 } else { 0 });
    }
    println!("  run_len=258 | match_len=255 | gaps=4094,4095 | max_distance=4096");
}

struct Step {
    position: usize,
    distance: usize,
    length: usize,
}

fn encode_steps(data: &[u8], mut inspect: impl FnMut(Step)) -> std::io::Result<Vec<u8>> {
    let encoded = lz77::encode(data)?;
    let mut input = encoded.as_slice();
    let size = varint::read(&mut input)? as usize;
    let mut position = 0;
    while position < size {
        let length = input[0] as usize;
        input = &input[1..];
        let distance = if length == 0 {
            0
        } else {
            varint::read(&mut input)? as usize
        };
        input = &input[1..];
        inspect(Step {
            position,
            distance,
            length,
        });
        position += length + 1;
    }
    assert!(input.is_empty());
    Ok(encoded)
}

fn hash_collision_is_checked_bytewise() {
    let mut seen = vec![None; 4096];
    let (first, second) = (0..=u16::MAX)
        .find_map(|value| {
            let pair = value.to_le_bytes();
            let hash = hash_pair(&pair, 0);
            seen[hash].replace(pair).map(|previous| (previous, pair))
        })
        .unwrap();
    assert_ne!(first, second);
    let input = [first.as_slice(), &[255], second.as_slice(), &[254]].concat();
    let mut matcher = Matcher::new(input.len());
    for position in 0..3 {
        matcher.insert(&input, position);
    }
    let best = matcher.find_best(&input, 3, 2);
    let expected = (0..3)
        .rev()
        .find_map(|candidate| (input[candidate] == input[3]).then_some((3 - candidate, 1)))
        .unwrap_or((0, 0));
    assert_eq!((best.distance, best.length), expected);
    assert!(best.candidates > 0);
    println!("  hash_collision=true | positions=0,1,2 | byte_check=true | selected=expected");
}

fn pair_chain_filters_candidates_and_preserves_closest_match() {
    let input: Vec<u8> = (0..=255).flat_map(|byte| [0, byte, 255]).collect();
    let position = input.len();
    let input = [input, vec![0, 42, 254, 1]].concat();
    let mut matcher = Matcher::new(input.len());
    for index in 0..position {
        matcher.insert(&input, index);
    }
    let best = matcher.find_best(&input, position, 3);
    assert_eq!((best.distance, best.length), (position - 42 * 3, 2));
    let first_byte_candidates = input[..position].iter().filter(|&&b| b == 0).count();
    assert!(best.candidates < first_byte_candidates);
    println!(
        "  alphabet=256 | prefix_len=772 | target_distance=126 | target_length=2 | candidates_filtered=true"
    );
}

fn one_byte_fallback_and_window_boundaries() {
    for gap in [4095, 4096] {
        let input = [vec![b'A'], vec![0; gap], b"A!?".to_vec()].concat();
        let position = gap + 1;
        let mut matcher = Matcher::new(input.len());
        for index in 0..position {
            matcher.insert(&input, index);
        }
        for limit in [1, 2] {
            let best = matcher.find_best(&input, position, limit);
            assert_eq!(
                (best.distance, best.length),
                if gap == 4095 { (4096, 1) } else { (0, 0) }
            );
        }
        let best = matcher.find_best(&input, position, 0);
        assert_eq!((best.distance, best.length, best.candidates), (0, 0, 0));
    }
    println!("  gaps=4095,4096 | limits=1,2 | fallback=1_byte | outside_window=no_match");
}

fn rans_normalization_rounding_and_paper_example() {
    let frequencies = rans::frequencies(b"ABC");
    assert_eq!(
        (frequencies[65], frequencies[66], frequencies[67]),
        (1366, 1365, 1365)
    );
    assert_eq!(frequencies.iter().map(|&x| u32::from(x)).sum::<u32>(), 4096);
    let encoded = rans::encode(b"ABAB").unwrap();
    // Проверяем состояния кодирования на каждом суффиксе.
    for (text, expected) in [
        (b"B".as_slice(), 16779264_u32),
        (b"AB", 33558528),
        (b"BAB", 67119104),
        (b"ABAB", 134238208),
    ] {
        let mut state = 8388608_u32;
        for &symbol in text.iter().rev() {
            state = (state / 2048) * 4096 + state % 2048 + if symbol == b'B' { 2048 } else { 0 };
        }
        assert_eq!(state, expected);
    }
    assert_eq!(encoded, [4, 1, 65, 66, 0x80, 0x10, 0, 80, 0, 8]);
    assert_eq!(rans::decode(&encoded).unwrap(), b"ABAB");
    let input = random_bytes(4096);
    let encoded = rans::encode(&input).unwrap();
    let header = rans::read_header(&encoded).unwrap();
    assert!(encoded.len() > header.state_offset + 4);
    assert_eq!(rans::decode(&encoded).unwrap(), input);
    println!("  freq_sum=4096 | input=ABAB | encoded=04 01 41 42 80 10 00 50 00 08 | decode=ABAB");
}

fn rans_sparse_bitmap_single_and_empty_models() {
    assert_eq!(rans::encode(&[]).unwrap(), [0]);
    assert_eq!(rans::decode(&[0]).unwrap(), []);
    for distinct in [1, 2, 31, 32, 33, 127, 128, 255, 256] {
        let input: Vec<u8> = (0..distinct).map(|s| s as u8).collect();
        let encoded = rans::encode(&input).unwrap();
        let header = rans::read_header(&encoded).unwrap();
        assert_eq!(header.frequencies, rans::frequencies(&input));
        assert_eq!(rans::decode(&encoded).unwrap(), input);
        let alphabet_size = distinct.min(32);
        let frequency_bytes: usize = header
            .frequencies
            .iter()
            .take(distinct - 1)
            .map(|&f| if f < 128 { 1 } else { 2 })
            .sum();
        assert_eq!(
            header.state_offset - length_prefix_size(&encoded),
            1 + alphabet_size + frequency_bytes
        );
        if distinct == 1 {
            assert_eq!(encoded.len(), 7);
        }
        if distinct == 256 {
            assert_eq!(header.state_offset - length_prefix_size(&encoded), 288);
        }
    }
    let input = [vec![0; 8192], vec![255]].concat();
    let encoded = rans::encode(&input).unwrap();
    assert_eq!(rans::decode(&encoded).unwrap(), input);
    assert_eq!(rans::read_header(&encoded).unwrap().frequencies[255], 1);
    println!("  input_len=0,1,4096,8193 | distinct=1,2,31,32,33,127,128,255,256 | decode=input");
}

fn paper_rs_word_and_single_error() {
    let expected = [
        1, 0x74, 0x40, 0x34, 0xAE, 0x36, 0x7E, 0x10, 0xC2, 0xA2, 0x21, 0x21, 0x9D, 0xB0, 0xC5,
        0xE1, 0x0C, 0x3B, 0x37, 0xFD, 0xE4, 0x94, 0x2F, 0xB3, 0xB9, 0x18, 0x8A, 0xFD, 0x14, 0x8E,
        0x37, 0xAC, 0x58,
    ];
    assert_eq!(ecc::encode(&[1]), expected);
    let mut damaged = expected;
    damaged[0] ^= 0x55;
    assert_eq!(syndrome(&damaged, 1), 0x55);
    assert_eq!(syndrome(&damaged, 2), 0x62);
    assert_eq!(multiply(0x55, 0x9D), 0x62);
    let mut power = 1;
    for i in 0..255 {
        if i == 32 {
            assert_eq!(power, 0x9D);
        }
        if i < 32 {
            assert_eq!(syndrome(&expected, power), 0);
        }
        power = multiply(power, 2);
    }
    assert_eq!(ecc::decode(&damaged, 1).unwrap(), (vec![1], 1));
    println!("  data=01 | codeword_len=33 | error_pos=7 | syndromes=55,62 | corrected=1");
}

fn multiply(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0;
    for _ in 0..8 {
        if b & 1 != 0 {
            product ^= a;
        }
        let high = a & 0x80 != 0;
        a <<= 1;
        if high {
            a ^= 0x1D;
        }
        b >>= 1;
    }
    product
}

fn syndrome(word: &[u8], x: u8) -> u8 {
    word.iter()
        .fold(0, |value, &byte| multiply(value, x) ^ byte)
}

fn paper_rans_renormalization_bytes() {
    let input = b"ABABABABABABABAB";
    let encoded = rans::encode(input).unwrap();
    // После двух выдач байтов: состояние 8389290 = 008002AA, поток A8 00.
    let expected = [
        16, 1, 65, 66, 0x80, 0x10, 0xAA, 0x02, 0x80, 0x00, 0xA8, 0x00,
    ];
    assert_eq!(encoded, expected);
    assert_eq!(rans::decode(&expected).unwrap(), input);
    assert_eq!(rans::decode(&encoded).unwrap(), input);
    println!("  input=ABABABABABABABAB | encoded_len=14 | renorm_bytes=verified | decode=input");
}

fn block_stream(input: &[u8], protected: bool) -> Vec<u8> {
    let mut writer = BlockWriter::with_threads(
        Vec::new(),
        crate::archive::EncodingOptions {
            protected,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    // Небольшие порции проверяют Write, а не только кодирование одним вызовом.
    for part in input.chunks(137) {
        writer.write_all(part).unwrap();
    }
    writer.finish().unwrap()
}

fn block_composition_compressed_stored_and_boundary() {
    for protected in [false, true] {
        for input in [vec![b'A'; BLOCK_SIZE + 1], random_bytes(BLOCK_SIZE + 123)] {
            let stream = block_stream(&input, protected);
            let mut reader = BlockReader::with_threads(stream.as_slice(), None).unwrap();
            let mut output = vec![0; input.len()];
            reader.read_exact(&mut output).unwrap();
            let stats = reader.finish().unwrap();
            assert_eq!(output, input);
            assert_eq!(stats.options.protected, protected);
            assert_eq!(stats.compressed + stats.stored, 2);
            assert_eq!(stats.corrected_bytes, 0);
            if input[0] == b'A' {
                assert_eq!(stats.compressed, 1);
            } else {
                assert_eq!(stats.stored, 2);
            }
        }
    }
    println!(
        "  protect=false,true | inputs=65537,65659 | blocks=2 | compressed+stored=2 | restored=input"
    );
}

fn every_stage_combination_stores_the_selected_encoding() {
    for flags in 0..8 {
        let options = EncodingOptions::from_flags(flags).unwrap();
        for input in [
            b"ABABABA".repeat(2000),
            vec![42; BLOCK_SIZE],
            random_bytes(BLOCK_SIZE),
            vec![7],
        ] {
            let mut expected = if options.lz77 {
                lz77::encode(&input).unwrap()
            } else {
                input.clone()
            };
            if options.rans {
                expected = rans::encode(&expected).unwrap();
            }
            let compressed = expected.len() < input.len();
            if !compressed {
                expected = input.clone();
            }
            let mut writer = BlockWriter::with_threads(Vec::new(), options, Some(2)).unwrap();
            writer.write_all(&input).unwrap();
            let bytes = writer.finish().unwrap();
            assert_eq!(bytes[8], flags);
            let stored = usize::from(u16::from_le_bytes([bytes[15], bytes[16]])) + 1;
            assert_eq!(stored, expected.len());
            let offset = if options.protected { 13 + 44 } else { 13 + 12 };
            let payload = if options.protected {
                ecc::decode(&bytes[offset..], stored).unwrap().0
            } else {
                bytes[offset..].to_vec()
            };
            assert_eq!(payload, expected, "flags={flags}");
            let mut reader = BlockReader::with_threads(bytes.as_slice(), Some(1)).unwrap();
            let mut restored = vec![0; input.len()];
            reader.read_exact(&mut restored).unwrap();
            let stats = reader.finish().unwrap();
            assert_eq!(restored, input);
            assert_eq!(stats.compressed, u64::from(compressed));
            assert_eq!(stats.options, options);
        }
    }
    println!("  flags=0..7 | inputs=repeat,random | payload=selected_stage | restored=input");
}

fn two_workers_run_concurrently_and_results_keep_order() {
    let mut executor = Executor::new(Some(2)).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let ids = Mutex::new(Vec::new());
    let result = executor
        .map(vec![10, 20], |value| {
            ids.lock().unwrap().push(std::thread::current().id());
            barrier.wait();
            value * 2
        })
        .unwrap();
    assert_eq!(result, [20, 40]);
    let ids = ids.into_inner().unwrap();
    assert_ne!(ids[0], ids[1]);
    println!("  workers=2 | jobs=10,20 | results=20,40 | order=kept");
}

fn data(size: usize) -> Vec<u8> {
    let mut state = 0x12345678_u32;
    (0..size)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if (i / BLOCK_SIZE).is_multiple_of(2) {
                b'A'
            } else {
                state as u8
            }
        })
        .collect()
}

fn stream(input: &[u8], protected: bool, threads: usize) -> Vec<u8> {
    let mut writer = BlockWriter::with_threads(
        Vec::new(),
        crate::archive::EncodingOptions {
            protected,
            ..Default::default()
        },
        Some(threads),
    )
    .unwrap();
    for part in input.chunks(137) {
        writer.write_all(part).unwrap();
    }
    writer.finish().unwrap()
}

fn parallel_batches_are_identical_and_roundtrip_across_boundaries() {
    for protected in [false, true] {
        for size in [
            0,
            1,
            BLOCK_SIZE,
            BLOCK_SIZE + 1,
            4 * BLOCK_SIZE,
            4 * BLOCK_SIZE + 17,
            9 * BLOCK_SIZE + 17,
        ] {
            let input = data(size);
            let reference = stream(&input, protected, 1);
            for threads in [2, 3] {
                let packed = stream(&input, protected, threads);
                assert_eq!(
                    packed, reference,
                    "size={size}, threads={threads}, RS={protected}"
                );
                let mut reader =
                    BlockReader::with_threads(packed.as_slice(), Some(threads)).unwrap();
                let mut restored = vec![0; size];
                reader.read_exact(&mut restored).unwrap();
                assert_eq!(restored, input);
                let stats = reader.finish().unwrap();
                assert_eq!(
                    stats.compressed + stats.stored,
                    size.div_ceil(BLOCK_SIZE) as u64
                );
                assert_eq!(stats.corrected_bytes, 0);
            }
        }
    }
    println!(
        "  protected=false,true | threads=2,3 | batch_boundaries=checked | stream=single_thread | restored=input"
    );
}

fn parallel_flush_drains_partial_batches_without_changing_order() {
    let input = data(6 * BLOCK_SIZE + 31);
    let encode = |threads| {
        let mut writer = BlockWriter::with_threads(
            Vec::new(),
            crate::archive::EncodingOptions {
                protected: false,
                ..Default::default()
            },
            Some(threads),
        )
        .unwrap();
        writer.write_all(&input[..BLOCK_SIZE + 13]).unwrap();
        writer.flush().unwrap();
        writer.flush().unwrap();
        writer.write_all(&input[BLOCK_SIZE + 13..]).unwrap();
        writer.finish().unwrap()
    };
    assert_eq!(encode(1), encode(2));
    let packed = encode(2);
    let mut reader = BlockReader::with_threads(packed.as_slice(), Some(2)).unwrap();
    let mut restored = vec![0; input.len()];
    reader.read_exact(&mut restored).unwrap();
    reader.finish().unwrap();
    assert_eq!(restored, input);
    println!(
        "  input_len=393247 | threads=1,2 | flush=partial_batch | streams=equal | restored=input"
    );
}

fn parallel_rs_restores_headers_and_payloads_in_multiple_batches() {
    let input = data(BLOCK_SIZE * 9 + 17);
    let mut damaged = stream(&input, true, 2);
    let mut position = ARCHIVE_HEADER_SIZE;
    let mut corrections = 0;
    while position < damaged.len() {
        let stored = usize::from(u16::from_le_bytes([
            damaged[position + 2],
            damaged[position + 3],
        ])) + 1;
        let payload = position + ecc::encoded_size(BLOCK_HEADER_SIZE);
        for byte in &mut damaged[position..position + 16] {
            *byte ^= 0xA5;
        }
        for byte in &mut damaged[payload..payload + 16] {
            *byte ^= 0x5A;
        }
        corrections += 32;
        position = payload + ecc::encoded_size(stored);
    }
    let mut reader = BlockReader::with_threads(damaged.as_slice(), Some(3)).unwrap();
    let mut restored = vec![0; input.len()];
    reader.read_exact(&mut restored).unwrap();
    let stats = reader.finish().unwrap();
    assert_eq!(restored, input);
    assert_eq!(stats.corrected_bytes, corrections);
    println!("  input_len=589841 | damaged_bytes=32 | restored=input | corrected=32");
}

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "archiver-test-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_arch"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("cleanup test sandbox");
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

fn cli_roundtrip_nested_unicode_empty_and_multiple_sources() {
    for protect in [false, true] {
        let sandbox = Sandbox::new();
        fs::create_dir_all(sandbox.path("папка/empty-dir")).unwrap();
        fs::write(sandbox.path("папка/empty-file"), []).unwrap();
        fs::write(sandbox.path("папка/данные.txt"), "Пример\n".repeat(20_000)).unwrap();
        fs::write(
            sandbox.path("binary"),
            (0..=255).cycle().take(70_000).collect::<Vec<u8>>(),
        )
        .unwrap();
        let mut args = vec!["create"];
        if protect {
            args.push("--protect");
        }
        args.extend(["result", "папка", "binary"]);
        let output = sandbox.command(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(sandbox.path("result.mrx").is_file());
        let info = sandbox.command(&["info", "result"]);
        assert!(info.status.success());
        let output = sandbox.command(&["extract", "result", "out"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        compare_tree(&sandbox.path("папка"), &sandbox.path("out/папка"));
        assert_eq!(
            fs::read(sandbox.path("binary")).unwrap(),
            fs::read(sandbox.path("out/binary")).unwrap()
        );
    }
    println!(
        "  protect=false,true | sources=3 | nested=true | unicode=true | empty=true | trees=equal"
    );
}

fn archive_inside_source_is_excluded() {
    let sandbox = Sandbox::new();
    fs::create_dir(sandbox.path("input")).unwrap();
    fs::write(sandbox.path("input/a"), b"data").unwrap();
    let path = archive::create(
        &sandbox.path("input/self.mrx"),
        &[sandbox.path("input")],
        false,
    )
    .unwrap();
    let mut names = Vec::new();
    let stats = archive::inspect(&path, |entry| names.push(entry.path.clone())).unwrap();
    assert_eq!(stats.files, 1);
    assert_eq!(names, ["input", "input/a"]);
    println!("  source_entries=input,input/a | archive_inside_source=excluded | files=1");
}

fn varint_file_sizes_and_path_lengths_cross_byte_boundary() {
    let sandbox = Sandbox::new();
    fs::create_dir(sandbox.path("input")).unwrap();
    for length in [121, 122, 127, 128] {
        // С префиксом input/ пути включают длины 127 и 128.
        let name = "a".repeat(length);
        fs::write(sandbox.path(&format!("input/{name}")), vec![0xFE; length]).unwrap();
    }
    for protect in [false, true] {
        let archive = sandbox.path(&format!("sizes-{protect}.mrx"));
        archive::create(&archive, &[sandbox.path("input")], protect).unwrap();
        let out = sandbox.path(&format!("out-{protect}"));
        archive::extract(&archive, &out).unwrap();
        compare_tree(&sandbox.path("input"), &out.join("input"));
    }
    println!("  lengths=121,122,127,128 | protect=false,true | create+extract | bytes=equal");
}

fn documented_archive_matches_real_bytes() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.path("a"), b"ABABABA").unwrap();
    assert!(
        sandbox
            .command(&["create", "paper.mrx", "a"])
            .status
            .success()
    );
    let format = include_str!("../README.md");
    let expected: Vec<u8> = format
        .lines()
        .filter(|line| line.starts_with("000000"))
        .flat_map(|line| line.split_whitespace().skip(1))
        .map(|hex| u8::from_str_radix(hex, 16).unwrap())
        .collect();
    assert_eq!(expected.len(), 37);
    assert_eq!(fs::read(sandbox.path("paper.mrx")).unwrap(), expected);
    println!(
        "  input=ABABABA | expected_archive_len=37 | created_bytes=expected | extracted=input"
    );
}

fn option_separator_preserves_filenames() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.path("--threads"), b"data").unwrap();
    assert!(
        sandbox
            .command(&[
                "create",
                "--threads",
                "1",
                "--",
                "--no-rans.mrx",
                "--threads"
            ])
            .status
            .success()
    );
    let bytes = fs::read(sandbox.path("--no-rans.mrx")).unwrap();
    assert_eq!(bytes[8], 3);
    assert!(
        sandbox
            .command(&["extract", "--no-rans.mrx", "out"])
            .status
            .success()
    );
    assert_eq!(fs::read(sandbox.path("out/--threads")).unwrap(), b"data");
    println!("  filename=--threads | separator=-- | flags=3 | extracted_bytes=data");
}

fn all_eight_modes_work_from_cli() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.path("input/пустая")).unwrap();
    fs::write(sandbox.path("input/empty"), []).unwrap();
    fs::write(sandbox.path("input/text"), b"ABABABA".repeat(12000)).unwrap();
    fs::write(
        sandbox.path("input/bytes"),
        (0..=255).cycle().take(70000).collect::<Vec<u8>>(),
    )
    .unwrap();
    for flags in 0..8 {
        let name = format!("mode-{flags}.mrx");
        let mut args = vec!["create", "--threads", "2"];
        if flags & 1 == 0 {
            args.push("--no-lz77");
        }
        if flags & 2 == 0 {
            args.push("--no-rans");
        }
        if flags & 4 != 0 {
            args.push("--protect");
        }
        args.extend([&name, "input"]);
        let result = sandbox.command(&args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let bytes = fs::read(sandbox.path(&name)).unwrap();
        assert_eq!(bytes[8], flags);
        let stats = archive::inspect(&sandbox.path(&name), |_| {}).unwrap();
        assert_eq!(
            (stats.lz77, stats.rans, stats.protected),
            (flags & 1 != 0, flags & 2 != 0, flags & 4 != 0)
        );
        assert_eq!(stats.files, 3);
        if flags & 3 != 0 {
            assert!(stats.compressed_blocks > 0);
        } else {
            assert_eq!(stats.compressed_blocks, 0);
        }
        let out = format!("out-{flags}");
        assert!(
            sandbox
                .command(&["extract", "--threads", "1", &name, &out])
                .status
                .success()
        );
        compare_tree(
            &sandbox.path("input"),
            &sandbox.path(&format!("{out}/input")),
        );

        let single_name = format!("single-{flags}.mrx");
        args[2] = "1";
        let name_index = args.len() - 2;
        args[name_index] = &single_name;
        let result = sandbox.command(&args);
        assert!(result.status.success());
        assert!(result.stderr.is_empty());
        assert_eq!(fs::read(sandbox.path(&single_name)).unwrap(), bytes);
        assert!(sandbox.command(&["info", &name]).status.success());
    }
    println!("  flags=0..7 | files=3 | create=ok | info=ok | extract=ok | bytes=equal");
}

fn main() {
    let tests: &[(&str, &str, fn())] = &[
        ("CRC32", "Контрольные суммы", crc_standard_vectors),
        ("CRC32", "Побитовый расчёт CRC", crc_table_matches_bit_trace),
        (
            "LZ77",
            "Перебор коротких входов",
            lz_hash_matches_reference_for_all_short_ternary_inputs,
        ),
        (
            "LZ77",
            "Повторяющиеся префиксы",
            lz_hash_matches_reference_for_repeated_prefixes,
        ),
        (
            "LZ77",
            "Пример ABABABA и перекрытия",
            lz_paper_example_and_overlap,
        ),
        (
            "LZ77",
            "Сравнение с прямым поиском",
            lz_search_matches_exhaustive_reference,
        ),
        (
            "LZ77",
            "Границы длины и окна",
            lz_match_and_window_boundaries,
        ),
        ("LZ77", "Коллизии хешей", hash_collision_is_checked_bytewise),
        (
            "LZ77",
            "Ближайшее совпадение",
            pair_chain_filters_candidates_and_preserves_closest_match,
        ),
        (
            "LZ77",
            "Однобайтовые совпадения",
            one_byte_fallback_and_window_boundaries,
        ),
        (
            "rANS",
            "Частоты и пример ABAB",
            rans_normalization_rounding_and_paper_example,
        ),
        (
            "rANS",
            "Представления алфавита",
            rans_sparse_bitmap_single_and_empty_models,
        ),
        (
            "Расчётные примеры",
            "Расчёт слова RS и ошибки",
            paper_rs_word_and_single_error,
        ),
        (
            "Расчётные примеры",
            "Байты перенормировки rANS",
            paper_rans_renormalization_bytes,
        ),
        (
            "Блоки архива",
            "Сжатые и исходные блоки",
            block_composition_compressed_stored_and_boundary,
        ),
        (
            "Режимы кодирования",
            "Содержимое всех восьми режимов",
            every_stage_combination_stores_the_selected_encoding,
        ),
        (
            "Многопоточность",
            "Параллельное выполнение",
            two_workers_run_concurrently_and_results_keep_order,
        ),
        (
            "Многопоточность",
            "Полные и неполные партии",
            parallel_batches_are_identical_and_roundtrip_across_boundaries,
        ),
        (
            "Многопоточность",
            "Принудительная запись",
            parallel_flush_drains_partial_batches_without_changing_order,
        ),
        (
            "Многопоточность",
            "Восстановление нескольких партий",
            parallel_rs_restores_headers_and_payloads_in_multiple_batches,
        ),
        (
            "Команды и файлы",
            "Имена и несколько источников",
            cli_roundtrip_nested_unicode_empty_and_multiple_sources,
        ),
        (
            "Команды и файлы",
            "Архив внутри исходной папки",
            archive_inside_source_is_excluded,
        ),
        (
            "Команды и файлы",
            "Длины имён и размеры файлов",
            varint_file_sizes_and_path_lengths_cross_byte_boundary,
        ),
        (
            "Команды и файлы",
            "Архив из README",
            documented_archive_matches_real_bytes,
        ),
        (
            "Команды и файлы",
            "Имена после разделителя",
            option_separator_preserves_filenames,
        ),
        (
            "Режимы кодирования",
            "Восемь режимов через команды",
            all_eight_modes_work_from_cli,
        ),
    ];
    let mut previous_group = "";
    let mut failures = 0;
    for &(group, name, test) in tests {
        if group != previous_group {
            println!("\n{group}");
            previous_group = group;
        }
        println!("{name}");
        if std::panic::catch_unwind(test).is_ok() {
            println!("  OK");
        } else {
            println!("  ERR");
            failures += 1;
        }
    }
    println!(
        "\nOK: {}\nERR: {failures}",
        tests.len() - failures
    );
    if failures != 0 {
        std::process::exit(1);
    }
}
