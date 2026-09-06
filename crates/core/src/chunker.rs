use std::io::Read;

mod fixed_size;
pub mod rabin;

use fixed_size::ChunkIter as FixedSizeChunkIter;
use rabin::ChunkIter as RabinChunkIter;
use rustic_cdc::Rabin64;

use crate::{
    RusticResult,
    backend::source_reader::SourceReader,
    repofile::{ConfigFile, configfile::Chunker},
};

/// A logical chunk. Zero chunks do not require a buffer or repeated hashing.
pub(crate) enum Chunk {
    Data(Vec<u8>),
    Zeros(usize),
}

#[cfg(test)]
impl Chunk {
    pub(super) fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Data(data) => data,
            Self::Zeros(size) => vec![0; size],
        }
    }
}

/// `ChunkIter` is an iterator that chunks data.
pub(crate) enum ChunkIter<R: Read + Send> {
    Rabin(Box<RabinChunkIter<R>>),
    FixedSize(FixedSizeChunkIter<R>),
}

impl<R: Read + Send> ChunkIter<R> {
    pub(crate) fn from_config(
        config: &ConfigFile,
        reader: SourceReader<R>,
        size_hint: usize,
    ) -> RusticResult<Self> {
        let iter = match config.chunker() {
            Chunker::Rabin => {
                let poly = config.poly()?;
                let rabin = Rabin64::new_with_polynom(6, &poly);
                Self::Rabin(Box::new(RabinChunkIter::new_with_holes(
                    rabin,
                    config.chunk_size(),
                    config.chunk_min_size(),
                    config.chunk_max_size(),
                    reader,
                    size_hint,
                )?))
            }
            Chunker::FixedSize => Self::FixedSize(FixedSizeChunkIter::new_with_holes(
                config.chunk_size(),
                reader,
                size_hint,
            )),
        };
        Ok(iter)
    }
}

impl<R: Read + Send> Iterator for ChunkIter<R> {
    type Item = RusticResult<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Rabin(rabin) => rabin.next_sparse_chunk(),
            Self::FixedSize(fixed_size) => fixed_size.next_sparse_chunk(),
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod sparse_tests {
    use super::*;
    use crate::crypto::hasher::hash;
    use proptest::prelude::*;
    use std::{
        fs::File,
        io::{Cursor, Seek, SeekFrom, Write},
    };
    use tempfile::NamedTempFile;

    fn config(fixed: bool, small: bool) -> ConfigFile {
        ConfigFile {
            chunker: Some(if fixed {
                Chunker::FixedSize
            } else {
                Chunker::Rabin
            }),
            chunker_polynomial: "3da3358b4dc173".into(),
            chunk_size: Some(if fixed { 65533 } else { 131_072 }),
            chunk_min_size: Some(if small { 65536 } else { 98304 }),
            chunk_max_size: Some(1_048_576),
            ..ConfigFile::default()
        }
    }

    fn check(config: &ConfigFile, length: usize, islands: &[(usize, Vec<u8>)]) {
        let mut file = NamedTempFile::new().unwrap();
        file.as_file().set_len(length as u64).unwrap();
        let mut expected = vec![0; length];
        for (offset, bytes) in islands {
            let offset = offset % length.max(1);
            let count = bytes.len().min(length - offset);
            _ = file.seek(SeekFrom::Start(offset as u64)).unwrap();
            file.write_all(&bytes[..count]).unwrap();
            expected[offset..offset + count].copy_from_slice(&bytes[..count]);
        }
        file.as_file().sync_all().unwrap();
        let ordinary: Vec<_> =
            ChunkIter::from_config(config, SourceReader::plain(Cursor::new(expected)), length)
                .unwrap()
                .map(|chunk| {
                    let bytes = chunk.unwrap().into_bytes();
                    (bytes.len(), hash(&bytes))
                })
                .collect();
        let source: SourceReader<File> =
            SourceReader::file(File::open(file.path()).unwrap()).unwrap();
        let sparse: Vec<_> = ChunkIter::from_config(config, source, length)
            .unwrap()
            .map(|chunk| {
                let bytes = chunk.unwrap().into_bytes();
                (bytes.len(), hash(&bytes))
            })
            .collect();
        assert_eq!(sparse, ordinary);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn sparse_matches_ordinary(
            length in 0usize..4*1024*1024,
            islands in prop::collection::vec((any::<usize>(), prop::collection::vec(any::<u8>(), 0..16384)), 0..24),
            fixed in any::<bool>(), small in any::<bool>(),
        ) {
            check(&config(fixed, small), length, &islands);
        }
    }

    #[test]
    fn sparse_boundaries_and_fragmentation() {
        for fixed in [false, true] {
            for length in [
                0,
                1,
                4095,
                4096,
                65532,
                65533,
                65534,
                131_071,
                131_072,
                131_073,
                4 * 1024 * 1024,
            ] {
                let islands: Vec<_> = (0..length)
                    .step_by(65536)
                    .map(|start| (start, vec![7; 4096]))
                    .collect();
                check(&config(fixed, true), length, &islands);
            }
        }
    }

    #[test]
    fn one_gib_hole_uses_zero_chunks_not_content_buffers() {
        let file = NamedTempFile::new().unwrap();
        file.as_file().set_len(1024 * 1024 * 1024).unwrap();
        let config = ConfigFile {
            chunker_polynomial: "3da3358b4dc173".into(),
            ..ConfigFile::default()
        };
        let source: SourceReader<File> =
            SourceReader::file(File::open(file.path()).unwrap()).unwrap();
        let mut count = 0;
        for chunk in ChunkIter::from_config(&config, source, 1024 * 1024 * 1024).unwrap() {
            assert!(matches!(chunk.unwrap(), Chunk::Zeros(524_288)));
            count += 1;
        }
        assert_eq!(count, 2048);
    }
}
