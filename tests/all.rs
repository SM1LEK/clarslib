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

fn varint_boundaries_canonical_encoding_and_errors() {
    for value in [0, 1, 127, 128, 255, 4096, 16383, 16384, 65536, u64::MAX] {
        let mut bytes = Vec::new();
        varint::write(&mut bytes, value).unwrap();
        let expected_size = if value == 0 {
            1
        } else {
            (64 - value.leading_zeros()).div_ceil(7) as usize
        };
        assert_eq!(bytes.len(), expected_size);
        bytes.push(0x42); // Reader должен остановиться точно на границе varint.
        let mut input = bytes.as_slice();
        assert_eq!(varint::read(&mut input).unwrap(), value);
        assert_eq!(input, [0x42]);
    }
    for bytes in [
        vec![],
        vec![0x80],
        vec![0x80, 0],
        vec![0x81, 0],
        vec![0xff; 10],
        [vec![0xff; 9], vec![2]].concat(),
        vec![0x80; 11],
    ] {
        assert!(varint::read(&mut bytes.as_slice()).is_err(), "{bytes:?}");
    }
    assert!(varint::read_bounded(&mut [0x80, 0x20].as_slice(), 4095).is_err());
    assert_eq!(
        varint::read_bounded(&mut [0x80, 0x20].as_slice(), 4096).unwrap(),
        4096
    );
    println!("  values=0,1,127,128,255,4096,16383,16384,65536,u64::MAX | invalid=7 | bounded=4096");
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

fn rs_zero_through_sixteen_errors_in_each_word() {
    for size in [1, 22, 222, 223, 224, 446, 447] {
        let input = random_bytes(size);
        let good = ecc::encode(&input);
        assert_eq!(good.len(), ecc::encoded_size(size));
        for errors in 0..=16 {
            let mut damaged = good.clone();
            let mut total = 0;
            for word in damaged.chunks_mut(255) {
                // Включая чётность; позиции различны даже в укороченном слове.
                for index in 0..errors {
                    word[index * 2] ^= (index + 1) as u8;
                }
                total += errors;
            }
            let (restored, count) = ecc::decode(&damaged, size).unwrap();
            assert_eq!(restored, input);
            assert_eq!(count, total as u64);
        }
    }
    assert_eq!(ecc::decode(&[], 0).unwrap(), (vec![], 0));
    assert!(ecc::decode(&[0; 32], 1).is_err());
    assert!(ecc::decode(&[], BLOCK_SIZE + 1).is_err());
    println!("  sizes=1,22,222,223,224,446,447 | errors_per_word=0..16 | restored=input");
}

fn algorithms_roundtrip_edge_cases_and_composition() {
    for input in [
        vec![],
        vec![0],
        vec![255; BLOCK_SIZE],
        (0..=255).collect(),
        random_bytes(BLOCK_SIZE),
        "Привет, архиватор!".as_bytes().to_vec(),
    ] {
        let lz = lz77::encode(&input).unwrap();
        let encoded = rans::encode(&lz).unwrap();
        assert_eq!(
            lz77::decode(&rans::decode(&encoded).unwrap()).unwrap(),
            input
        );
        assert_eq!(rans::decode(&rans::encode(&input).unwrap()).unwrap(), input);
    }
    assert!(lz77::encode(&vec![0; BLOCK_SIZE + 1]).is_err());
    assert!(rans::encode(&vec![0; lz77::MAX_ENCODED_SIZE + 1]).is_err());
    println!("  inputs=empty,single,repeat,random,max_block | LZ77+rANS | reverse=input");
}

fn malformed_lz_and_rans_return_errors() {
    for bytes in [
        vec![],
        vec![0; 3],
        vec![1, 0, 0, 0],
        vec![1, 0, 0, 0, 1, 0, 1, 65],
        vec![1, 0, 0, 0, 0, 0, 0, 65, 0, 0, 0, 66],
        vec![0xff; 8],
    ] {
        assert!(lz77::decode(&bytes).is_err(), "{bytes:?}");
    }
    let good = rans::encode(&random_bytes(100)).unwrap();
    let header = rans::read_header(&good).unwrap();
    for length in 0..good.len() {
        assert!(
            rans::decode(&good[..length]).is_err(),
            "усечение rANS {length}"
        );
    }
    let mut bad = good.clone();
    bad[length_prefix_size(&good)..header.state_offset].fill(0);
    assert!(rans::decode(&bad).is_err());
    let mut bad = good.clone();
    bad[header.state_offset..header.state_offset + 4].fill(0);
    assert!(rans::decode(&bad).is_err());
    let mut bad = good;
    bad.push(0);
    assert!(rans::decode(&bad).is_err());
    println!("  LZ_truncated=all_prefixes | rANS_bad_state=3 | result=error");
}

fn compact_models_reject_invalid_values_without_panics() {
    for bytes in [
        vec![0, 0],                                           // Пустой rANS имеет ровно один байт.
        vec![0x80, 0],                                        // Неканоническая длина.
        vec![1, 1, 65, 66],                                   // D > длины результата.
        vec![4, 1, 65, 65, 1, 0, 0, 0x80, 0],                 // Повтор символа.
        vec![4, 1, 66, 65, 1, 0, 0, 0x80, 0],                 // Нарушение порядка.
        vec![4, 1, 65, 66, 0, 0, 0, 0x80, 0],                 // Нулевая частота.
        vec![4, 1, 65, 66, 0x80, 0x20, 0, 0, 0x80, 0],        // 4096 при D > 1.
        vec![4, 2, 65, 66, 67, 0xff, 0x1f, 1, 0, 0, 0x80, 0], // Нет места третьему.
        [vec![33, 32], vec![0; 32], vec![1; 32], vec![0, 0, 0x80, 0]].concat(), // Пустая bitmap.
    ] {
        assert!(rans::decode(&bytes).is_err(), "{bytes:?}");
    }
    for bytes in [
        vec![0x80, 0],
        vec![0x81, 0x80, 4],               // length > 65536
        vec![3, 0, 65, 1, 0, 66],          // Нулевая ссылка.
        vec![3, 0, 65, 1, 0x81, 0, 66],    // Неканоническое расстояние 1.
        vec![3, 0, 65, 1, 0x81, 0x20, 66], // distance=4097
        vec![1, 0, 65, 0, 66],
    ] {
        // Выход за размер результата.
        assert!(lz77::decode(&bytes).is_err(), "{bytes:?}");
    }
    // Детерминированная выборка произвольных некорректных потоков: отсутствие паник.
    for size in 0..256 {
        let bytes = random_bytes(size);
        let _ = lz77::decode(&bytes);
        let _ = rans::decode(&bytes);
    }
    println!(
        "  rANS_invalid_models=checked | LZ_invalid_links=checked | panic=false | result=error"
    );
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

fn block_rejects_bad_headers_payload_truncation_and_trailing_data() {
    let original = b"ABABABA";
    let good = block_stream(original, false);
    for offset in 0..good.len() {
        let mut bad = good.clone();
        bad[offset] ^= 0xA5;
        let result = BlockReader::with_threads(bad.as_slice(), None).and_then(|mut reader| {
            reader.read_exact(&mut [0; 7])?;
            reader.finish()
        });
        assert!(result.is_err(), "offset {offset}");
    }
    for length in 0..good.len() {
        let result = BlockReader::with_threads(&good[..length], None).and_then(|mut reader| {
            reader.read_exact(&mut [0; 7])?;
            reader.finish()
        });
        assert!(result.is_err(), "length {length}");
    }
    let mut extra = good;
    extra.push(0);
    let mut reader = BlockReader::with_threads(extra.as_slice(), None).unwrap();
    reader.read_exact(&mut [0; 7]).unwrap();
    assert!(reader.finish().is_err());
    println!("  header_offsets=all | payload_prefixes=all | trailing_data=1 | result=error");
}

fn block_sizes_and_implicit_sequence_reject_reordering() {
    use crate::block::{ARCHIVE_HEADER_SIZE, BLOCK_HEADER_SIZE};
    let input = random_bytes(BLOCK_SIZE).repeat(2);
    for protected in [false, true] {
        let stream = block_stream(&input, protected);
        let block_size = if protected {
            ecc::encoded_size(BLOCK_HEADER_SIZE) + ecc::encoded_size(BLOCK_SIZE)
        } else {
            BLOCK_HEADER_SIZE + BLOCK_SIZE
        };
        assert_eq!(stream.len(), ARCHIVE_HEADER_SIZE + 2 * block_size);
        assert_eq!(stream[8], if protected { 7 } else { 3 });
        assert_eq!(&stream[13..17], &[255; 4]); // 65536-1, два поля u16.
        let first = &stream[ARCHIVE_HEADER_SIZE..ARCHIVE_HEADER_SIZE + block_size];
        let second = &stream[ARCHIVE_HEADER_SIZE + block_size..];
        // Данные одинаковы, но CRC заголовков учитывают разные номера блоков.
        assert_ne!(&first[8..12], &second[8..12]);
        let swapped = [&stream[..ARCHIVE_HEADER_SIZE], second, first].concat();
        let mut reader = BlockReader::with_threads(swapped.as_slice(), None).unwrap();
        assert!(reader.read_exact(&mut [0]).is_err());
        let repeated = [&stream[..ARCHIVE_HEADER_SIZE], first, first].concat();
        let mut reader = BlockReader::with_threads(repeated.as_slice(), None).unwrap();
        reader.read_exact(&mut vec![0; BLOCK_SIZE]).unwrap();
        assert!(reader.read_exact(&mut [0]).is_err());
    }
    // Неизвестный флаг запрещён даже при верной CRC.
    let mut bad = block_stream(b"A", false);
    bad[8] = 0x80;
    let crc = checksum::crc32(&bad[..9]);
    bad[9..13].copy_from_slice(&crc.to_le_bytes());
    assert!(BlockReader::with_threads(bad.as_slice(), None).is_err());
    println!(
        "  input=131072 | blocks=2 | protected=false,true | reordered=error | duplicate=error"
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

fn parallel_read_ahead_keeps_errors_at_their_block_and_rejects_trailing_data() {
    let input = vec![42; BLOCK_SIZE * 2];
    let good = stream(&input, false, 2);
    let first_size = usize::from(u16::from_le_bytes([good[15], good[16]])) + 1;
    let second_start = ARCHIVE_HEADER_SIZE + BLOCK_HEADER_SIZE + first_size;
    for offset in [second_start + 8, second_start + BLOCK_HEADER_SIZE] {
        let mut bad = good.clone();
        bad[offset] ^= 0xA5;
        let mut reader = BlockReader::with_threads(bad.as_slice(), Some(2)).unwrap();
        let mut first = vec![0; BLOCK_SIZE];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(first, &input[..BLOCK_SIZE]);
        assert!(reader.read_exact(&mut [0]).is_err());
    }
    for boundary in [1, 4] {
        let bytes = vec![42; BLOCK_SIZE * boundary];
        let valid = stream(&bytes, false, 2);
        for suffix in [vec![0], valid[ARCHIVE_HEADER_SIZE..].to_vec()] {
            let extra = [valid.clone(), suffix].concat();
            let mut reader = BlockReader::with_threads(extra.as_slice(), Some(2)).unwrap();
            reader.read_exact(&mut vec![0; bytes.len()]).unwrap();
            assert!(reader.finish().is_err());
        }
        let short = &valid[..valid.len() - 1];
        let mut reader = BlockReader::with_threads(short, Some(2)).unwrap();
        assert!(reader.read_exact(&mut vec![0; bytes.len()]).is_err());
    }
    println!(
        "  blocks=2 | corrupt_header=true | corrupt_payload=true | trailing_cases=4 | result=error"
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

fn parallel_writer_propagates_output_errors() {
    struct FailsAfterHeader;
    impl Write for FailsAfterHeader {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() == ARCHIVE_HEADER_SIZE {
                Ok(bytes.len())
            } else {
                Err(io::Error::other("disk error"))
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BlockWriter::with_threads(
        FailsAfterHeader,
        crate::archive::EncodingOptions {
            protected: false,
            ..Default::default()
        },
        Some(2),
    )
    .unwrap();
    writer.write_all(b"abc").unwrap();
    assert!(writer.finish().is_err());
    let mut writer = BlockWriter::with_threads(
        FailsAfterHeader,
        crate::archive::EncodingOptions {
            protected: false,
            ..Default::default()
        },
        Some(2),
    )
    .unwrap();
    assert!(writer.write_all(&data(BLOCK_SIZE * 4)).is_err());
    println!("  writer_limit=archive_header | input_blocks=2 | write_error=returned");
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
        assert!(String::from_utf8_lossy(&info.stdout).contains("Проверка целостности пройдена"));
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

fn cli_invalid_arguments() {
    let sandbox = Sandbox::new();
    for args in [
        vec![],
        vec!["help"],
        vec!["--help"],
        vec!["-h"],
        vec!["unknown"],
        vec!["create"],
        vec!["create", "--help"],
        vec!["create", "--unknown", "a", "b"],
        vec!["extract", "x"],
        vec!["extract", "a", "b", "c"],
        vec!["info"],
        vec!["info", "a", "b"],
        vec!["--threads", "1"],
    ] {
        let output = sandbox.command(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.starts_with("Ошибка: "), "{args:?}: {error}");
        assert_eq!(error.lines().count(), 1, "{args:?}: {error}");
        assert!(!error.contains("Использование"));
    }
    println!("  invalid_commands=checked | exit=1 | stdout_len=0 | stderr_lines=1 | usage=false");
}

fn preserve_existing_files_and_cleanup_failed_operations() {
    let sandbox = Sandbox::new();
    let source = sandbox.path("source");
    fs::write(&source, b"safe").unwrap();
    let path = archive::create(
        &sandbox.path("safe.mrx"),
        std::slice::from_ref(&source),
        false,
    )
    .unwrap();
    let before = fs::read(&path).unwrap();
    assert!(archive::create(&path, std::slice::from_ref(&source), true).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::create_dir(sandbox.path("existing")).unwrap();
    fs::write(sandbox.path("existing/keep"), b"keep").unwrap();
    assert!(archive::extract(&path, &sandbox.path("existing")).is_err());
    assert_eq!(fs::read(sandbox.path("existing/keep")).unwrap(), b"keep");
    let failed = sandbox.path("duplicate.mrx");
    assert!(archive::create(&failed, &[source.clone(), source], false).is_err());
    assert!(!failed.exists());
    assert!(archive::create(&sandbox.path("empty.mrx"), &[], false).is_err());
    assert!(!sandbox.path("empty.mrx").exists());
    let mut bad = before.clone();
    bad.push(0);
    fs::write(sandbox.path("extra.mrx"), bad).unwrap();
    assert!(archive::extract(&sandbox.path("extra.mrx"), &sandbox.path("bad-out")).is_err());
    assert!(!sandbox.path("bad-out").exists());
    // Все возможные усечения маленького архива должны отклоняться.
    for length in 0..before.len() {
        fs::write(sandbox.path("short.mrx"), &before[..length]).unwrap();
        assert!(
            archive::inspect(&sandbox.path("short.mrx"), |_| {}).is_err(),
            "{length}"
        );
    }
    println!(
        "  existing_archive=unchanged | existing_output=unchanged | partial_archive=removed | partial_output=removed"
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

#[cfg(unix)]
fn symbolic_links_are_rejected() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.path("source"), b"safe").unwrap();
    std::os::unix::fs::symlink(sandbox.path("source"), sandbox.path("link")).unwrap();
    assert!(archive::create(&sandbox.path("bad.mrx"), &[sandbox.path("link")], false).is_err());
    assert!(!sandbox.path("bad.mrx").exists());
    println!("  source=symlink | create=error | archive_exists=false");
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

fn damaged_archives_recover_or_leave_no_output() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.path("a"), b"ABABABA").unwrap();
    assert!(
        sandbox
            .command(&["create", "--protect", "good", "a"])
            .status
            .success()
    );
    let good = fs::read(sandbox.path("good.mrx")).unwrap();
    for (label, offset, errors, recover) in [
        ("header", 13, 16, true),
        ("data", 57, 16, true),
        ("parity", 69, 16, true),
        ("too-many", 57, 17, false),
        ("outer", 8, 1, false),
    ] {
        let mut damaged = good.clone();
        for byte in &mut damaged[offset..offset + errors] {
            *byte ^= 0xA5;
        }
        let name = format!("{label}.mrx");
        let out = format!("out-{label}");
        fs::write(sandbox.path(&name), damaged).unwrap();
        let info = sandbox.command(&["info", &name]);
        let result = sandbox.command(&["extract", &name, &out]);
        assert_eq!(info.status.success(), recover, "{label}");
        assert_eq!(result.status.success(), recover, "{label}");
        if recover {
            assert_eq!(
                fs::read(sandbox.path(&format!("{out}/a"))).unwrap(),
                b"ABABABA"
            );
            assert_eq!(
                archive::inspect(&sandbox.path(&name), |_| {})
                    .unwrap()
                    .corrected_bytes,
                errors as u64
            );
        } else {
            assert!(!sandbox.path(&out).exists());
        }
    }
    println!(
        "  damage_cases=header,payload | recoverable=true,false | info=create_result_match | failed_output=absent"
    );
}

fn parallel_cli_archives_match_and_restore_nested_tree() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.path("input/папка/empty")).unwrap();
    fs::write(sandbox.path("input/папка/zero"), []).unwrap();
    fs::write(sandbox.path("input/папка/text"), b"ABABABA".repeat(100_000)).unwrap();
    let mut state = 0x12345678_u32;
    let binary: Vec<_> = (0..700_000)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    fs::write(sandbox.path("input/binary"), binary).unwrap();
    for protected in [false, true] {
        let mut reference = None;
        for threads in ["1", "2", "3", "auto"] {
            let name = format!("archive-{protected}-{threads}.mrx");
            let mut args = vec!["create"];
            if threads != "auto" {
                args.extend(["--threads", threads]);
            }
            if protected {
                args.push("--protect");
            }
            args.extend([&name, "input"]);
            let output = sandbox.command(&args);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let bytes = fs::read(sandbox.path(&name)).unwrap();
            if let Some(expected) = &reference {
                assert_eq!(&bytes, expected);
            } else {
                reference = Some(bytes);
            }
            let out = format!("out-{protected}-{threads}");
            // Глобальный параметр до команды; чтение с числом потоков, отличным от записи.
            let result = sandbox.command(&["--threads", "2", "extract", &name, &out]);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            compare_tree(
                &sandbox.path("input"),
                &sandbox.path(&format!("{out}/input")),
            );
            assert!(
                sandbox
                    .command(&["info", "--threads", "3", &name])
                    .status
                    .success()
            );
        }
        let mut bad = reference.unwrap();
        bad.truncate(bad.len() - 1);
        fs::write(sandbox.path("broken.mrx"), bad).unwrap();
        assert!(
            !sandbox
                .command(&["extract", "--threads", "2", "broken.mrx", "failed"])
                .status
                .success()
        );
        assert!(!sandbox.path("failed").exists());
    }
    println!("  protected=false,true | threads=1,2,3,auto | archive_bytes=equal | trees=equal");
}

fn parallel_cli_invalid_counts_do_not_create_outputs() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.path("input"), b"safe").unwrap();
    for count in ["0", "65", "-1", "x", "99999999999999999999999"] {
        let result = sandbox.command(&["create", "--threads", count, "bad.mrx", "input"]);
        assert!(!result.status.success());
        assert!(!sandbox.path("bad.mrx").exists());
    }
    for args in [
        vec!["create", "--threads"],
        vec!["--threads", "2", "create", "--threads", "3", "bad", "input"],
    ] {
        assert!(!sandbox.command(&args).status.success());
    }
    assert!(!sandbox.path("bad.mrx").exists());
    println!("  threads=0,65,-1,x,overflow | create=error | extract=error | outputs=absent");
}

fn cli_returns_without_reading_stdin() {
    let sandbox = Sandbox::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_arch"))
        .current_dir(&sandbox.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("Программа ожидает ввод вместо завершения");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Не указана команда")
    );
    println!("  stdin=closed | commands=create,info,extract | completed=true | input_read=false");
}

fn main() {
    let tests: &[(&str, &str, fn())] = &[
        (
            "Переменные целые",
            "Границы и ошибочные числа",
            varint_boundaries_canonical_encoding_and_errors,
        ),
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
            "Reed–Solomon",
            "От нуля до 16 повреждений",
            rs_zero_through_sixteen_errors_in_each_word,
        ),
        (
            "Совместное кодирование",
            "Совместное сжатие и восстановление",
            algorithms_roundtrip_edge_cases_and_composition,
        ),
        (
            "Совместное кодирование",
            "Оборванные потоки",
            malformed_lz_and_rans_return_errors,
        ),
        (
            "Совместное кодирование",
            "Неверные модели и ссылки",
            compact_models_reject_invalid_values_without_panics,
        ),
        (
            "Блоки архива",
            "Сжатые и исходные блоки",
            block_composition_compressed_stored_and_boundary,
        ),
        (
            "Блоки архива",
            "Повреждения и усечения блока",
            block_rejects_bad_headers_payload_truncation_and_trailing_data,
        ),
        (
            "Блоки архива",
            "Размеры и порядок блоков",
            block_sizes_and_implicit_sequence_reject_reordering,
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
            "Ошибки при опережающем чтении",
            parallel_read_ahead_keeps_errors_at_their_block_and_rejects_trailing_data,
        ),
        (
            "Многопоточность",
            "Восстановление нескольких партий",
            parallel_rs_restores_headers_and_payloads_in_multiple_batches,
        ),
        (
            "Многопоточность",
            "Ошибки записи",
            parallel_writer_propagates_output_errors,
        ),
        (
            "Команды и файлы",
            "Имена и несколько источников",
            cli_roundtrip_nested_unicode_empty_and_multiple_sources,
        ),
        (
            "Команды и файлы",
            "Ошибочные команды без справки",
            cli_invalid_arguments,
        ),
        (
            "Команды и файлы",
            "Защита существующих файлов",
            preserve_existing_files_and_cleanup_failed_operations,
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
        #[cfg(unix)]
        (
            "Команды и файлы",
            "Запрет символических ссылок",
            symbolic_links_are_rejected,
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
        (
            "Режимы кодирования",
            "Повреждения настоящего архива",
            damaged_archives_recover_or_leave_no_output,
        ),
        (
            "Команды и потоки",
            "Число потоков и содержимое архива",
            parallel_cli_archives_match_and_restore_nested_tree,
        ),
        (
            "Команды и потоки",
            "Неверное число потоков",
            parallel_cli_invalid_counts_do_not_create_outputs,
        ),
        (
            "Команды и файлы",
            "Отсутствие интерактивного ввода",
            cli_returns_without_reading_stdin,
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
