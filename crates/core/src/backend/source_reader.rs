//! Optional hole discovery for local files; all other sources remain ordinary readers.

use std::{
    fs::File,
    io::{self, Read, Seek},
};

/// An opened backup source with optional filesystem hole discovery.
#[derive(Debug)]
pub struct SourceReader<R> {
    inner: Source<R>,
}

#[derive(Debug)]
enum Source<R> {
    Plain(R),
    File {
        file: File,
        offset: u64,
        holes: bool,
    },
}

impl<R: Read> SourceReader<R> {
    /// Wrap a reader without hole discovery.
    pub fn plain(reader: R) -> Self {
        Self {
            inner: Source::Plain(reader),
        }
    }

    /// Wrap an opened local file, preserving its current position.
    ///
    /// # Errors
    /// Returns metadata or seek errors for regular files.
    pub fn file(mut file: File) -> io::Result<Self> {
        let regular = file.metadata()?.is_file();
        let offset = if regular { file.stream_position()? } else { 0 };
        Ok(Self {
            inner: Source::File {
                file,
                offset,
                holes: regular && cfg!(target_os = "linux"),
            },
        })
    }

    /// The opened local file, if this reader is one.
    pub(crate) const fn as_file(&self) -> Option<&File> {
        match &self.inner {
            Source::File { file, .. } => Some(file),
            Source::Plain(_) => None,
        }
    }

    /// Skip only if the entire requested range is a real hole before EOF.
    pub(crate) fn skip_hole(&mut self, length: usize) -> io::Result<bool> {
        #[cfg(target_os = "linux")]
        if let Source::File {
            file,
            offset,
            holes,
        } = &mut self.inner
        {
            use nix::{
                errno::Errno,
                libc::off_t,
                unistd::{Whence, lseek},
            };
            if !*holes || length == 0 {
                return Ok(false);
            }
            let overflow = || io::Error::new(io::ErrorKind::InvalidInput, "hole offset overflow");
            // off_t is 32 bits on some targets: convert with checks, never truncate.
            let end_offset = offset.checked_add(length as u64).ok_or_else(overflow)?;
            let end = off_t::try_from(end_offset).map_err(|_| overflow())?;
            let next_data = lseek(
                &*file,
                off_t::try_from(*offset).map_err(|_| overflow())?,
                Whence::SeekData,
            );
            let is_hole = match next_data {
                Ok(data) => data >= end,
                Err(Errno::ENXIO) => file.metadata()?.len() >= end_offset,
                Err(Errno::EINVAL | Errno::EOPNOTSUPP | Errno::ENOSYS) => {
                    *holes = false;
                    false
                }
                Err(err) => return Err(io::Error::from_raw_os_error(err as i32)),
            };
            let position = if is_hole { end_offset } else { *offset };
            _ = file.seek(io::SeekFrom::Start(position))?;
            *offset = position;
            return Ok(is_hole);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = length;
        Ok(false)
    }
}

impl<R: Read> Read for SourceReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            Source::Plain(reader) => reader.read(buf),
            Source::File { file, offset, .. } => {
                let read = file.read(buf)?;
                *offset += read as u64;
                Ok(read)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }

    #[test]
    fn plain_reader_never_infers_holes_or_swallows_errors() {
        let mut reader = SourceReader::plain(Cursor::new([0; 8192]));
        assert!(!reader.skip_hole(4096).unwrap());
        let mut bytes = Vec::new();
        _ = reader.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![0; 8192]);

        let mut reader = SourceReader::plain(Broken);
        assert!(!reader.skip_hole(4096).unwrap());
        assert_eq!(
            reader.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied,
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn holes_stop_at_data_and_current_eof() {
        use std::io::{SeekFrom, Write};
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(65536).unwrap();
        _ = file.seek(SeekFrom::Start(32768)).unwrap();
        file.write_all(&[7; 4096]).unwrap();
        file.sync_all().unwrap();
        file.rewind().unwrap();
        let mut reader = SourceReader::<File>::file(file.try_clone().unwrap()).unwrap();
        assert!(reader.skip_hole(32768).unwrap());
        assert!(!reader.skip_hole(4096).unwrap());
        let mut data = [0; 4096];
        reader.read_exact(&mut data).unwrap();
        assert_eq!(data, [7; 4096]);
        assert!(reader.skip_hole(4096).unwrap());
        // No cached extent map may invent zeros after a concurrent truncation.
        file.set_len(40960).unwrap();
        assert!(!reader.skip_hole(4096).unwrap());
        assert_eq!(reader.read(&mut data).unwrap(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn overflow_and_nonzero_initial_position() {
        use std::io::SeekFrom;
        let mut file = tempfile::tempfile().unwrap();
        file.set_len(65536).unwrap();
        _ = file.seek(SeekFrom::Start(4096)).unwrap();
        let mut reader = SourceReader::<File>::file(file).unwrap();
        assert!(reader.skip_hole(65536 - 4096).unwrap());
        assert!(!reader.skip_hole(1).unwrap());
        assert!(!reader.skip_hole(0).unwrap());
        // Not every filesystem can seek this far (ext4 returns EINVAL), so
        // place the offset directly instead of seeking the file there.
        let mut reader = SourceReader::<File> {
            inner: Source::File {
                file: tempfile::tempfile().unwrap(),
                offset: i64::MAX.cast_unsigned(),
                holes: true,
            },
        };
        assert_eq!(
            reader.skip_hole(1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
        );
    }
}
