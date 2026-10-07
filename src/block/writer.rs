use std::io::{self, Write};

use super::{BLOCK_HEADER_SIZE, SIGNATURE, header_crc};
use crate::{BLOCK_SIZE, checksum::crc32, ecc, lz77, parallel::Executor, rans};

pub(crate) struct BlockWriter<W: Write> {
    output: W,
    options: crate::archive::EncodingOptions,
    sequence: u64,
    buffer: Vec<u8>,
    pending: Vec<Vec<u8>>,
    executor: Executor,
}

// +опции
// если сжатие невыгодно, оставляем как было
fn encode_block(
    raw: Vec<u8>,
    options: crate::archive::EncodingOptions,
    sequence: u64,
) -> io::Result<Vec<u8>> {
    let mut compressed = if options.lz77 {
        lz77::encode(&raw)?
    } else {
        raw.clone()
    };
    if options.rans {
        compressed = rans::encode(&compressed)?;
    }
    let raw_size = raw.len();
    let checksum = crc32(&raw);
    let stored = if compressed.len() < raw_size {
        compressed
    } else {
        raw
    };
    let mut header = Vec::with_capacity(BLOCK_HEADER_SIZE);
    header.extend_from_slice(&((raw_size - 1) as u16).to_le_bytes());
    header.extend_from_slice(&((stored.len() - 1) as u16).to_le_bytes());
    header.extend_from_slice(&checksum.to_le_bytes());
    header.extend_from_slice(&header_crc(sequence, &header).to_le_bytes());
    let mut output = if options.protected {
        ecc::encode(&header)
    } else {
        header
    };
    if options.protected {
        output.extend_from_slice(&ecc::encode(&stored));
    } else {
        output.extend_from_slice(&stored);
    }
    Ok(output)
}

impl<W: Write> BlockWriter<W> {
    pub fn with_threads(
        mut output: W,
        options: crate::archive::EncodingOptions,
        threads: Option<usize>,
    ) -> io::Result<Self> {
        let executor = Executor::new(threads)?;
        let mut header = SIGNATURE.to_vec();
        header.push(options.flags());
        header.extend_from_slice(&crc32(&header).to_le_bytes());
        output.write_all(&header)?;
        Ok(Self {
            output,
            options,
            sequence: 0,
            buffer: Vec::with_capacity(BLOCK_SIZE),
            pending: Vec::new(),
            executor,
        })
    }

    fn queue_block(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            self.pending.push(std::mem::replace(
                &mut self.buffer,
                Vec::with_capacity(BLOCK_SIZE),
            ));
        }
        if self.pending.len() == self.executor.batch_size() {
            self.flush_batch()?;
        }
        Ok(())
    }

    fn flush_batch(&mut self) -> io::Result<()> {
        let options = self.options;
        let sequence = self.sequence;
        let batch = std::mem::take(&mut self.pending)
            .into_iter()
            .enumerate()
            .collect();
        let packed = self.executor.map(batch, |(index, raw)| {
            encode_block(raw, options, sequence + index as u64)
        })?;
        for block in packed {
            self.output.write_all(&block?)?;
            self.sequence += 1;
        }
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.flush()?;
        Ok(self.output)
    }
}

impl<W: Write> Write for BlockWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let count = data.len().min(BLOCK_SIZE - self.buffer.len());
        self.buffer.extend_from_slice(&data[..count]);
        if self.buffer.len() == BLOCK_SIZE {
            self.queue_block()?;
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.queue_block()?;
        self.flush_batch()?;
        self.output.flush()
    }
}
