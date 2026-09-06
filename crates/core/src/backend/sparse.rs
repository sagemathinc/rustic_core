//! Sparse writes must clear existing bytes, not merely skip writes of zeros.

use std::{
    fs::File,
    io::{self, Seek, SeekFrom, Write},
    ops::Range,
};

/// Avoid a syscall for every short zero run in ordinary binary data.
const MIN_HOLE: usize = 4096;

/// Compute once per repository blob, not once per logical occurrence of that blob.
pub(crate) struct ZeroRanges(Vec<Range<usize>>);

impl ZeroRanges {
    pub(crate) fn new(data: &[u8]) -> Self {
        let mut ranges = Vec::new();
        let mut cursor = 0;
        while cursor < data.len() {
            if data[cursor] != 0 {
                cursor += 1;
                continue;
            }
            let start = cursor;
            while cursor < data.len() && data[cursor] == 0 {
                cursor += 1;
            }
            if cursor - start >= MIN_HOLE || (start == 0 && cursor == data.len()) {
                ranges.push(start..cursor);
            }
        }
        Self(ranges)
    }
}

pub(super) fn write_sparse(
    file: &mut File,
    offset: u64,
    data: &[u8],
    required: bool,
    ranges: &ZeroRanges,
) -> io::Result<()> {
    write_ranges(file, offset, data, required, ranges, punch_hole)
}

fn write_ranges(
    file: &mut File,
    offset: u64,
    data: &[u8],
    required: bool,
    ranges: &ZeroRanges,
    mut punch: impl FnMut(&File, u64, u64) -> io::Result<()>,
) -> io::Result<()> {
    _ = offset
        .checked_add(data.len() as u64)
        .filter(|end| i64::try_from(*end).is_ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "sparse range overflow"))?;
    let mut written = 0;
    for range in &ranges.0 {
        let start = range.start;
        let cursor = range.end;
        if start > written {
            write_range(file, offset + written as u64, &data[written..start])?;
        }
        match punch(file, offset + start as u64, (cursor - start) as u64) {
            Ok(()) => (),
            Err(err) if !required && err.kind() == io::ErrorKind::Unsupported => {
                // Portable best effort must write the zeros, including on existing files.
                write_range(file, offset + start as u64, &data[start..cursor])?;
            }
            Err(err) => return Err(err),
        }
        written = cursor;
    }
    if written < data.len() {
        write_range(file, offset + written as u64, &data[written..])?;
    }
    Ok(())
}

fn write_range(file: &mut File, offset: u64, data: &[u8]) -> io::Result<()> {
    _ = file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)
}

#[cfg(target_os = "linux")]
fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    use nix::{
        errno::Errno,
        fcntl::{FallocateFlags, fallocate},
    };
    let offset = i64::try_from(offset).map_err(io::Error::other)?;
    let length = i64::try_from(length).map_err(io::Error::other)?;
    fallocate(
        file,
        FallocateFlags::FALLOC_FL_PUNCH_HOLE | FallocateFlags::FALLOC_FL_KEEP_SIZE,
        offset,
        length,
    )
    .map_err(|err| match err {
        Errno::EOPNOTSUPP | Errno::ENOSYS => io::Error::new(io::ErrorKind::Unsupported, err),
        _ => io::Error::from_raw_os_error(err as i32),
    })
}

#[cfg(not(target_os = "linux"))]
fn punch_hole(_file: &File, _offset: u64, _length: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "sparse hole punching is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use tempfile::tempfile;

    fn contents(file: &mut File) -> Vec<u8> {
        file.rewind().unwrap();
        let mut bytes = Vec::new();
        _ = file.read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn unsupported_fallback_writes_actual_zeros() {
        let mut file = tempfile().unwrap();
        file.write_all(&[0xa5; 16384]).unwrap();
        write_ranges(
            &mut file,
            4096,
            &[0; 8192],
            false,
            &ZeroRanges::new(&[0; 8192]),
            |_, _, _| Err(io::ErrorKind::Unsupported.into()),
        )
        .unwrap();
        let mut expected = vec![0xa5; 16384];
        expected[4096..12288].fill(0);
        assert_eq!(contents(&mut file), expected);
    }

    #[test]
    fn required_or_io_error_never_falls_back() {
        for (required, kind) in [
            (true, io::ErrorKind::Unsupported),
            (false, io::ErrorKind::PermissionDenied),
            (false, io::ErrorKind::StorageFull),
        ] {
            let mut file = tempfile().unwrap();
            file.write_all(&[0xa5; 8192]).unwrap();
            let err = write_ranges(
                &mut file,
                0,
                &[0; 8192],
                required,
                &ZeroRanges::new(&[0; 8192]),
                |_, _, _| Err(kind.into()),
            )
            .unwrap_err();
            assert_eq!(err.kind(), kind);
            assert_eq!(contents(&mut file), vec![0xa5; 8192]);
        }
    }

    #[test]
    fn range_overflow_does_not_modify_file() {
        let mut file = tempfile().unwrap();
        assert!(
            write_sparse(
                &mut file,
                u64::MAX,
                &[0; 4096],
                true,
                &ZeroRanges::new(&[0; 4096])
            )
            .is_err()
        );
        assert!(contents(&mut file).is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn punch_existing_mixed_unaligned_and_trailing_ranges() {
        let mut file = tempfile().unwrap();
        file.write_all(&vec![0xa5; 128 * 1024]).unwrap();
        let mut data = vec![0; 96 * 1024];
        data[123..125].fill(7);
        data[48 * 1024..49 * 1024].fill(9);
        write_sparse(&mut file, 17, &data, true, &ZeroRanges::new(&data)).unwrap();
        let mut expected = vec![0xa5; 128 * 1024];
        expected[17..17 + data.len()].copy_from_slice(&data);
        assert_eq!(contents(&mut file), expected);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn new_file_does_not_allocate_zero_interiors() {
        use std::os::unix::fs::MetadataExt;
        let mut file = tempfile().unwrap();
        let mut data = vec![0; 16 * 1024 * 1024];
        for offset in (0..data.len()).step_by(65536) {
            data[offset..offset + 4096].fill(7);
        }
        file.set_len(data.len() as u64).unwrap();
        write_sparse(&mut file, 0, &data, true, &ZeroRanges::new(&data)).unwrap();
        file.sync_all().unwrap();
        assert_eq!(contents(&mut file), data);
        assert!(file.metadata().unwrap().blocks() * 512 < data.len() as u64 / 2);
    }
}
