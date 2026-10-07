//! Параллелизм. Ограниченные блоки. Локальный пул Rayon с упорядоченным результатом

use std::io;

use rayon::{ThreadPool, ThreadPoolBuilder, prelude::*};

pub(crate) const MAX_THREADS: usize = 64;

pub(crate) struct Executor {
    threads: usize,
    pool: Option<ThreadPool>,
}

impl Executor {
    pub fn new(threads: Option<usize>) -> io::Result<Self> {
        let threads = threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
                .min(MAX_THREADS)
        });
        if !(1..=MAX_THREADS).contains(&threads) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Число потоков 1<=>64",
            ));
        }
        Ok(Self {
            threads,
            pool: None,
        })
    }

    pub fn batch_size(&self) -> usize {
        if self.threads == 1 {
            1
        } else {
            (2 * self.threads).max(4)
        }
    }

    pub fn map<T: Send, U: Send>(
        &mut self,
        items: Vec<T>,
        operation: impl Fn(T) -> U + Sync + Send,
    ) -> io::Result<Vec<U>> {
        if self.threads == 1 || items.len() <= 1 {
            return Ok(items.into_iter().map(operation).collect());
        }
        // Один блок без пула, для нескольких партий пул переиспользуется
        if self.pool.is_none() {
            self.pool = Some(
                ThreadPoolBuilder::new()
                    .num_threads(self.threads)
                    .build()
                    .map_err(io::Error::other)?,
            );
        }
        Ok(self
            .pool
            .as_ref()
            .unwrap()
            .install(|| items.into_par_iter().map(operation).collect()))
    }
}
