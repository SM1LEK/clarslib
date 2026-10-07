use std::collections::VecDeque;
use std::io::{self, Read};

use super::{ARCHIVE_HEADER_SIZE, BLOCK_HEADER_SIZE, BlockStats, SIGNATURE, header_crc};
use crate::{checksum::crc32, ecc, invalid, lz77, parallel::Executor, rans};

struct StoredBlock {
    original_size: usize,
    stored_size: usize,
    checksum: u32,
    header_corrections: u64,
    payload: Vec<u8>,
}

struct DecodedBlock {
    data: Vec<u8>,
    compressed: bool,
    corrections: u64,
}

fn decode_block(
    block: StoredBlock,
    options: crate::archive::EncodingOptions,
) -> io::Result<DecodedBlock> {
    let (stored, corrections) = if options.protected {
        ecc::decode(&block.payload, block.stored_size)?
    } else {
        (block.payload, 0)
    };
    let compressed = block.stored_size < block.original_size;
    let mut data = stored;
    if compressed {
        if options.rans {
            data = rans::decode(&data)?;
        }
        if options.lz77 {
            data = lz77::decode(&data)?;
        }
    }
    if data.len() != block.original_size || crc32(&data) != block.checksum {
        return Err(invalid("Данные повреждены"));
    }
    Ok(DecodedBlock {
        data,
        compressed,
        corrections: corrections + block.header_corrections,
    })
}

pub(crate) struct BlockReader<R: Read> {
    input: R,
    sequence: u64,
    buffer: Vec<u8>,
    position: usize,
    stats: BlockStats,
    pending: VecDeque<io::Result<DecodedBlock>>,
    executor: Executor,
    eof: bool,
}

impl<R: Read> BlockReader<R> {
    pub fn with_threads(mut input: R, threads: Option<usize>) -> io::Result<Self> {
        let executor = Executor::new(threads)?;
        let mut header = [0; ARCHIVE_HEADER_SIZE];
        input.read_exact(&mut header)?;
        if &header[..8] != SIGNATURE {
            return Err(invalid("Неизвестный формат"));
        }
        if crc32(&header[..9]) != u32::from_le_bytes(header[9..13].try_into().unwrap()) {
            return Err(invalid("Повреждён внешний заголовок архива"));
        }
        let options = crate::archive::EncodingOptions::from_flags(header[8])?;
        Ok(Self {
            input,
            sequence: 0,
            buffer: Vec::new(),
            position: 0,
            stats: BlockStats {
                options,
                ..BlockStats::default()
            },
            pending: VecDeque::new(),
            executor,
            eof: false,
        })
    }

    fn read_stored(&mut self) -> io::Result<Option<StoredBlock>> {
        let protected = self.stats.options.protected;
        let length = if protected {
            ecc::encoded_size(BLOCK_HEADER_SIZE)
        } else {
            BLOCK_HEADER_SIZE
        };
        let mut header = vec![0; length];
        // EOF допустим только до первого байта нового блока
        loop {
            match self.input.read(&mut header[..1]) {
                Ok(0) => return Ok(None),
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        self.input.read_exact(&mut header[1..])?;
        // Восстановления заголовка + проверки CRC => размеры
        let (header, header_corrections) = if protected {
            ecc::decode(&header, BLOCK_HEADER_SIZE)?
        } else {
            (header, 0)
        };
        let number =
            |offset: usize| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
        if header_crc(self.sequence, &header[..8]) != number(8) {
            return Err(invalid(
                "Неверная CRC32 заголовка или нарушен порядок блоков",
            ));
        }
        let original_size = usize::from(u16::from_le_bytes([header[0], header[1]])) + 1;
        let stored_size = usize::from(u16::from_le_bytes([header[2], header[3]])) + 1;
        if stored_size > original_size
            || (stored_size < original_size
                && ((!self.stats.options.lz77 && !self.stats.options.rans)
                    || (self.stats.options.rans && stored_size < rans::MIN_HEADER_SIZE)))
        {
            return Err(invalid("Неверный размер блока"));
        }
        let mut payload = vec![
            0;
            if protected {
                ecc::encoded_size(stored_size)
            } else {
                stored_size
            }
        ];
        self.input.read_exact(&mut payload)?;
        self.sequence += 1;
        Ok(Some(StoredBlock {
            original_size,
            stored_size,
            checksum: number(4),
            header_corrections,
            payload,
        }))
    }

    fn load_batch(&mut self) -> io::Result<()> {
        let mut stored = Vec::new();
        let mut read_error = None;
        for _ in 0..self.executor.batch_size() {
            if self.eof {
                break;
            }
            match self.read_stored() {
                Ok(Some(block)) => stored.push(block),
                Ok(None) => self.eof = true,
                Err(error) => {
                    read_error = Some(error);
                    self.eof = true;
                    break;
                }
            }
        }
        let options = self.stats.options;
        self.pending = self
            .executor
            .map(stored, |block| decode_block(block, options))?
            .into();
        if let Some(error) = read_error {
            self.pending.push_back(Err(error));
        }
        Ok(())
    }

    fn load_block(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            self.load_batch()?;
        }
        let block = self.pending.pop_front().ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "Обрыв блочного потока")
        })??;
        self.stats.corrected_bytes += block.corrections;
        if block.compressed {
            self.stats.compressed += 1;
        } else {
            self.stats.stored += 1;
        }
        self.buffer = block.data;
        self.position = 0;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<BlockStats> {
        if self.position != self.buffer.len() || !self.pending.is_empty() {
            return Err(invalid("Лишние данные"));
        }
        if !self.eof {
            let mut extra = [0];
            loop {
                match self.input.read(&mut extra) {
                    Ok(0) => break,
                    Ok(_) => return Err(invalid("Лишние данные")),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(self.stats)
    }
}

impl<R: Read> Read for BlockReader<R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.position == self.buffer.len() {
            self.load_block()?;
        }
        let count = output.len().min(self.buffer.len() - self.position);
        output[..count].copy_from_slice(&self.buffer[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}
