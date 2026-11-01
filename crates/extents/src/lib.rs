//! Content identity by btrfs extents (`--reuse-extents`).
//!
//! Two regular files on the same btrfs filesystem whose data are the same
//! immutable extents have the same bytes. A file's *extent signature* is an
//! exact, canonical encoding of its complete list of file extent items; files
//! with equal signatures may share archived content without being read.
//!
//! Soundness rests on:
//! - the backup root is a pinned, read-only btrfs subvolume, re-checked at the
//!   end of the run (same subvolume generation, still read-only);
//! - a per-file writeback barrier (`FIEMAP_FLAG_SYNC`) before the first scan,
//!   so dirty page-cache data can't hide behind unchanged extent items;
//! - two identical scans per signature (tree search pagination is not a
//!   snapshot);
//! - the item generation in the signature: a freed `disk_bytenr` is only
//!   reallocated in a later transaction, so new data never matches old.
//!
//! Anything unusual (inline, prealloc, encoded or NOCOW data, holes, nested
//! subvolumes, errors) yields no signature, and the file is read normally.

use std::fmt;

/// Domain tag of the canonical encoding.
const SIGNATURE_TAG: &[u8] = b"rustic-extent-sig/v1";

/// `BTRFS_FILE_EXTENT_*` types.
const EXTENT_INLINE: u8 = 0;
const EXTENT_REG: u8 = 1;

/// One file extent item, as stored on disk (little-endian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileExtent {
    /// Logical offset in the file (the item key's offset).
    pub file_offset: u64,
    /// Transaction that wrote the extent.
    pub generation: u64,
    /// Decoded size of the whole extent.
    pub ram_bytes: u64,
    /// Compression type (0 = none).
    pub compression: u8,
    /// Encryption type (0 = none).
    pub encryption: u8,
    /// Other encoding (0 = none).
    pub other_encoding: u16,
    /// `BTRFS_FILE_EXTENT_*` type.
    pub kind: u8,
    /// Disk address of the extent (0 = hole).
    pub disk_bytenr: u64,
    /// Size of the extent on disk.
    pub disk_num_bytes: u64,
    /// Offset of the file data within the (decoded) extent.
    pub offset: u64,
    /// Bytes of the file this item covers.
    pub num_bytes: u64,
}

/// Size of a non-inline `btrfs_file_extent_item`.
const FILE_EXTENT_ITEM_LEN: usize = 53;
/// Size of the inline header part (everything before `disk_bytenr`).
const FILE_EXTENT_INLINE_HEADER_LEN: usize = 21;

fn le_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

impl FileExtent {
    /// Parse an on-disk `btrfs_file_extent_item`. Inline items are parsed only
    /// far enough to be recognized (and rejected by the signature).
    #[must_use]
    pub fn parse(file_offset: u64, item: &[u8]) -> Option<Self> {
        if item.len() < FILE_EXTENT_INLINE_HEADER_LEN {
            return None;
        }
        let kind = item[20];
        let mut extent = Self {
            file_offset,
            generation: le_u64(item, 0),
            ram_bytes: le_u64(item, 8),
            compression: item[16],
            encryption: item[17],
            other_encoding: u16::from_le_bytes([item[18], item[19]]),
            kind,
            disk_bytenr: 0,
            disk_num_bytes: 0,
            offset: 0,
            num_bytes: 0,
        };
        if kind != EXTENT_INLINE {
            if item.len() < FILE_EXTENT_ITEM_LEN {
                return None;
            }
            extent.disk_bytenr = le_u64(item, 21);
            extent.disk_num_bytes = le_u64(item, 29);
            extent.offset = le_u64(item, 37);
            extent.num_bytes = le_u64(item, 45);
        }
        Some(extent)
    }
}

/// Why a file has no signature (it is then read normally).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoSignature {
    /// Empty file or no extent items.
    Empty,
    /// Inline data in the metadata tree.
    Inline,
    /// Preallocated or unknown extent type.
    NotRegular,
    /// Encrypted or otherwise encoded data.
    Encoded,
    /// A hole (explicit or implied by a gap).
    Hole,
    /// The extents end before the file does.
    Short,
    /// Items out of order or overlapping.
    Unordered,
}

impl fmt::Display for NoSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty file",
            Self::Inline => "inline extent",
            Self::NotRegular => "prealloc or unknown extent type",
            Self::Encoded => "encrypted or otherwise encoded extent",
            Self::Hole => "hole",
            Self::Short => "extents don't cover the file size",
            Self::Unordered => "extent items out of order or overlapping",
        })
    }
}

/// The exact canonical encoding of a file's extents, or why there is none.
///
/// The extents must be regular, unencoded, contiguous from offset 0 and cover
/// at least `size` bytes; a hole anywhere means no signature (first version).
///
/// # Errors
///
/// The reason the file has no signature (it is then read normally).
pub fn signature(
    fsid: &[u8; 16],
    size: u64,
    extents: &[FileExtent],
) -> Result<Vec<u8>, NoSignature> {
    if size == 0 || extents.is_empty() {
        return Err(NoSignature::Empty);
    }
    let mut next = 0_u64;
    for e in extents {
        if e.kind == EXTENT_INLINE {
            return Err(NoSignature::Inline);
        }
        if e.kind != EXTENT_REG {
            return Err(NoSignature::NotRegular);
        }
        if e.encryption != 0 || e.other_encoding != 0 {
            return Err(NoSignature::Encoded);
        }
        if e.disk_bytenr == 0 {
            // an explicit hole extent (filesystems without NO_HOLES)
            return Err(NoSignature::Hole);
        }
        match e.file_offset.cmp(&next) {
            std::cmp::Ordering::Greater => return Err(NoSignature::Hole),
            std::cmp::Ordering::Less => return Err(NoSignature::Unordered),
            std::cmp::Ordering::Equal => {}
        }
        next = e
            .file_offset
            .checked_add(e.num_bytes)
            .ok_or(NoSignature::Unordered)?;
    }
    if next < size {
        return Err(NoSignature::Short);
    }
    let mut out = Vec::with_capacity(SIGNATURE_TAG.len() + 28 + extents.len() * 61);
    out.extend_from_slice(SIGNATURE_TAG);
    out.extend_from_slice(fsid);
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(extents.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for e in extents {
        out.extend_from_slice(&e.file_offset.to_le_bytes());
        out.push(e.kind);
        out.push(e.compression);
        out.push(e.encryption);
        out.extend_from_slice(&e.other_encoding.to_le_bytes());
        out.extend_from_slice(&e.disk_bytenr.to_le_bytes());
        out.extend_from_slice(&e.disk_num_bytes.to_le_bytes());
        out.extend_from_slice(&e.offset.to_le_bytes());
        out.extend_from_slice(&e.num_bytes.to_le_bytes());
        out.extend_from_slice(&e.ram_bytes.to_le_bytes());
        out.extend_from_slice(&e.generation.to_le_bytes());
    }
    Ok(out)
}

/// One result of `BTRFS_IOC_TREE_SEARCH_V2`: header (native endian, kernel
/// ABI) followed by the item (on-disk, little-endian).
const SEARCH_HEADER_LEN: usize = 32;
/// `BTRFS_EXTENT_DATA_KEY`.
const EXTENT_DATA_KEY: u32 = 108;

/// Parse a tree search result buffer holding `nr_items` items. Returns the
/// extent items of `objectid` and the key offset of the last item seen (for
/// pagination), or `None` if the buffer is malformed.
#[must_use]
pub fn parse_search_buffer(
    buf: &[u8],
    nr_items: u32,
    objectid: u64,
) -> Option<(Vec<FileExtent>, Option<u64>)> {
    let mut extents = Vec::new();
    let mut at = 0_usize;
    let mut last_offset = None;
    for _ in 0..nr_items {
        let header = buf.get(at..at + SEARCH_HEADER_LEN)?;
        let item_objectid = u64::from_ne_bytes(header[8..16].try_into().ok()?);
        let item_offset = u64::from_ne_bytes(header[16..24].try_into().ok()?);
        let item_type = u32::from_ne_bytes(header[24..28].try_into().ok()?);
        let len = u32::from_ne_bytes(header[28..32].try_into().ok()?) as usize;
        at += SEARCH_HEADER_LEN;
        let item = buf.get(at..at + len)?;
        at += len;
        last_offset = Some(item_offset);
        if item_objectid == objectid && item_type == EXTENT_DATA_KEY {
            extents.push(FileExtent::parse(item_offset, item)?);
        }
    }
    Some((extents, last_offset))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod linux {
    //! The ioctls. Every failure means "no signature" for that file, or "no-op"
    //! for the whole run; nothing here fails a backup except the end check.

    use super::{EXTENT_DATA_KEY, FileExtent, parse_search_buffer, signature};
    use std::{
        fs::File,
        io,
        os::{fd::AsRawFd, unix::fs::MetadataExt},
    };

    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;
    const BTRFS_ROOT_SUBVOL_RDONLY: u64 = 1 << 0;
    const FS_NOCOW_FL: libc::c_long = 0x0080_0000;
    const FIEMAP_FLAG_SYNC: u32 = 0x1;

    // _IOC encoding (asm-generic, used by x86_64 and aarch64).
    const fn ioc(dir: u64, ty: u64, nr: u64, size: u64) -> u64 {
        (dir << 30) | (size << 16) | (ty << 8) | nr
    }
    const READ: u64 = 2;
    const WRITE: u64 = 1;
    const BTRFS: u64 = 0x94;
    const GET_SUBVOL_INFO: u64 = ioc(READ, BTRFS, 60, 504);
    const FS_INFO: u64 = ioc(READ, BTRFS, 31, 1024);
    const TREE_SEARCH_V2: u64 = ioc(READ | WRITE, BTRFS, 17, 112);
    const FS_IOC_GETFLAGS: u64 = ioc(READ, b'f' as u64, 1, 8);
    const FS_IOC_FIEMAP: u64 = ioc(READ | WRITE, b'f' as u64, 11, 32);

    fn ioctl<T>(fd: &impl AsRawFd, request: u64, arg: *mut T) -> io::Result<()> {
        // SAFETY: each request is paired with an argument of the size encoded in it.
        let ret = unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Two consecutive `u32` struct fields as one `u64` word of an argument.
    fn two_u32(first: u32, second: u32) -> u64 {
        let mut b = [0_u8; 8];
        b[..4].copy_from_slice(&first.to_ne_bytes());
        b[4..].copy_from_slice(&second.to_ne_bytes());
        u64::from_ne_bytes(b)
    }

    /// Identity of the pinned read-only subvolume.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SubvolState {
        /// Subvolume (tree) id.
        pub tree_id: u64,
        /// Generation of the subvolume root.
        pub generation: u64,
        /// Subvolume UUID.
        pub uuid: [u8; 16],
        /// The read-only root item flag.
        pub read_only: bool,
    }

    /// The pinned backup root and filesystem identity for one run.
    #[derive(Debug)]
    pub struct ExtentScanner {
        root: File,
        dev: u64,
        fsid: [u8; 16],
        start: SubvolState,
        skip_barrier: bool,
    }

    /// Why `--reuse-extents` does nothing for this run.
    pub type NoOp = &'static str;

    /// `BTRFS_IOC_GET_SUBVOL_INFO` works on any directory of the subvolume
    /// (unlike `SUBVOL_GETFLAGS`, which needs the subvolume root), so a backup
    /// of a directory inside a read-only snapshot is pinned too.
    fn subvol_state(root: &File) -> io::Result<SubvolState> {
        // struct btrfs_ioctl_get_subvol_info_args: treeid @0, generation @280,
        // flags (root item flags) @288, uuid @296
        let mut info = [0_u8; 504];
        ioctl(root, GET_SUBVOL_INFO, info.as_mut_ptr())?;
        let field = |at: usize| u64::from_ne_bytes(info[at..at + 8].try_into().unwrap());
        let mut uuid = [0_u8; 16];
        uuid.copy_from_slice(&info[296..312]);
        Ok(SubvolState {
            tree_id: field(0),
            generation: field(280),
            uuid,
            read_only: field(288) & BTRFS_ROOT_SUBVOL_RDONLY != 0,
        })
    }

    impl ExtentScanner {
        /// Pin `root` (an open directory) or explain why reuse is a no-op.
        ///
        /// # Errors
        ///
        /// The no-op reason: not btrfs, source not read-only, or unprivileged.
        pub fn pin(root: File, skip_barrier: bool) -> Result<Self, NoOp> {
            // SAFETY: `statfs` is plain data; all-zero is a valid value.
            let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
            // SAFETY: a valid fd and a pointer to a live `statfs` out-parameter.
            if unsafe { libc::fstatfs(root.as_raw_fd(), &raw mut sfs) } != 0
                || sfs.f_type as i64 != BTRFS_SUPER_MAGIC
            {
                return Err("not btrfs");
            }
            let start = subvol_state(&root).map_err(|_| "not btrfs")?;
            if !start.read_only {
                return Err("source not read-only");
            }
            let mut fs_info = [0_u8; 1024];
            ioctl(&root, FS_INFO, fs_info.as_mut_ptr()).map_err(|_| "not btrfs")?;
            let mut fsid = [0_u8; 16];
            fsid.copy_from_slice(&fs_info[16..32]);
            let dev = root.metadata().map_err(|_| "not btrfs")?.dev();
            let scanner = Self {
                root,
                dev,
                fsid,
                start,
                skip_barrier,
            };
            // TREE_SEARCH needs CAP_SYS_ADMIN: probe once on the root directory.
            let probe = scanner.root.metadata().map_err(|_| "not btrfs")?.ino();
            match scanner.search(probe) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EPERM) => return Err("unprivileged"),
                Err(_) => return Err("tree search failed"),
            }
            Ok(scanner)
        }

        /// The extent items of inode `ino` in the pinned subvolume.
        fn search(&self, ino: u64) -> io::Result<Vec<FileExtent>> {
            const BUF: usize = 64 * 1024;
            let mut extents = Vec::new();
            let mut min_offset = 0_u64;
            loop {
                // btrfs_ioctl_search_args_v2: 104-byte key, u64 buf_size, buffer
                let mut args = vec![0_u64; (112 + BUF) / 8];
                let key = &mut args[..13];
                key[0] = self.start.tree_id; // tree_id
                key[1] = ino; // min_objectid
                key[2] = ino; // max_objectid
                key[3] = min_offset; // min_offset
                key[4] = u64::MAX; // max_offset
                key[5] = 0; // min_transid
                key[6] = u64::MAX; // max_transid
                key[7] = two_u32(EXTENT_DATA_KEY, EXTENT_DATA_KEY); // min_type, max_type
                key[8] = two_u32(4096, 0); // nr_items, unused
                args[13] = BUF as u64; // buf_size
                ioctl(&self.root, TREE_SEARCH_V2, args.as_mut_ptr())?;
                let nr_items = u32::from_ne_bytes(args[8].to_ne_bytes()[..4].try_into().unwrap());
                if nr_items == 0 {
                    return Ok(extents);
                }
                // the result buffer follows the 112-byte header
                let buf: Vec<u8> = args[14..].iter().flat_map(|w| w.to_ne_bytes()).collect();
                let (mut page, last) = parse_search_buffer(&buf, nr_items, ino)
                    .ok_or_else(|| io::Error::other("malformed tree search result"))?;
                extents.append(&mut page);
                match last.and_then(|o| o.checked_add(1)) {
                    Some(next) => min_offset = next,
                    None => return Ok(extents),
                }
            }
        }

        fn barrier(&self, file: &File) -> io::Result<()> {
            if self.skip_barrier {
                return Ok(());
            }
            // struct fiemap with fm_extent_count = 0: sync, then count only.
            let mut fiemap = [0_u64; 4];
            fiemap[1] = u64::MAX; // fm_length
            fiemap[2] = two_u32(FIEMAP_FLAG_SYNC, 0); // fm_flags, fm_mapped_extents
            ioctl(file, FS_IOC_FIEMAP, fiemap.as_mut_ptr())
        }

        fn is_nocow(file: &File) -> bool {
            let mut flags: libc::c_long = 0;
            ioctl(file, FS_IOC_GETFLAGS, &raw mut flags).is_err() || flags & FS_NOCOW_FL != 0
        }

        /// One scan of an open file: its signature, or `None`.
        fn scan(&self, file: &File) -> Option<Vec<u8>> {
            let meta = file.metadata().ok()?;
            if !meta.is_file() || meta.dev() != self.dev || Self::is_nocow(file) {
                return None;
            }
            let extents = self.search(meta.ino()).ok()?;
            signature(&self.fsid, meta.len(), &extents).ok()
        }

        /// The signature of an open regular file, after the writeback barrier,
        /// from two identical scans; `None` means "read the file normally".
        #[must_use]
        pub fn signature_of(&self, file: &File) -> Option<Vec<u8>> {
            self.barrier(file).ok()?;
            let first = self.scan(file)?;
            let second = self.scan(file)?;
            (first == second).then_some(first)
        }

        /// A post-read check for an owner: the file still has `expected`.
        #[must_use]
        pub fn still(&self, file: &File, expected: &[u8]) -> bool {
            self.scan(file).is_some_and(|s| s == expected)
        }

        /// End of run: the root must still be read-only with the same
        /// generation; otherwise the snapshot must not be saved.
        ///
        /// # Errors
        ///
        /// What changed, if anything did.
        pub fn verify_end(&self) -> Result<(), String> {
            let end = subvol_state(&self.root).map_err(|e| e.to_string())?;
            if end == self.start {
                Ok(())
            } else {
                Err(format!(
                    "backup source changed during --reuse-extents (read-only {} -> {}, generation {} -> {})",
                    self.start.read_only, end.read_only, self.start.generation, end.generation
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FSID: [u8; 16] = [7; 16];

    fn reg(file_offset: u64, bytenr: u64, num_bytes: u64, generation: u64) -> FileExtent {
        FileExtent {
            file_offset,
            generation,
            ram_bytes: num_bytes,
            compression: 0,
            encryption: 0,
            other_encoding: 0,
            kind: EXTENT_REG,
            disk_bytenr: bytenr,
            disk_num_bytes: num_bytes,
            offset: 0,
            num_bytes,
        }
    }

    fn item_bytes(e: &FileExtent) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&e.generation.to_le_bytes());
        b.extend_from_slice(&e.ram_bytes.to_le_bytes());
        b.push(e.compression);
        b.push(e.encryption);
        b.extend_from_slice(&e.other_encoding.to_le_bytes());
        b.push(e.kind);
        if e.kind != EXTENT_INLINE {
            b.extend_from_slice(&e.disk_bytenr.to_le_bytes());
            b.extend_from_slice(&e.disk_num_bytes.to_le_bytes());
            b.extend_from_slice(&e.offset.to_le_bytes());
            b.extend_from_slice(&e.num_bytes.to_le_bytes());
        }
        b
    }

    fn search_buffer(items: &[(u64, u32, Vec<u8>)], objectid: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        for (offset, ty, item) in items {
            buf.extend_from_slice(&1_u64.to_ne_bytes()); // transid
            buf.extend_from_slice(&objectid.to_ne_bytes());
            buf.extend_from_slice(&offset.to_ne_bytes());
            buf.extend_from_slice(&ty.to_ne_bytes());
            buf.extend_from_slice(&u32::try_from(item.len()).unwrap().to_ne_bytes());
            buf.extend_from_slice(item);
        }
        buf
    }

    #[test]
    fn parses_items_from_a_search_buffer() {
        let a = reg(0, 1 << 20, 4096, 9);
        let b = FileExtent {
            compression: 3,
            disk_num_bytes: 4096,
            ram_bytes: 131_072,
            num_bytes: 131_072,
            ..reg(4096, 2 << 20, 131_072, 9)
        };
        let buf = search_buffer(
            &[
                (0, EXTENT_DATA_KEY, item_bytes(&a)),
                (4096, EXTENT_DATA_KEY, item_bytes(&b)),
                (0, 1, vec![0; 160]), // another key type is skipped
            ],
            257,
        );
        let (extents, last) = parse_search_buffer(&buf, 3, 257).unwrap();
        assert_eq!(extents, vec![a, b]);
        assert_eq!(last, Some(0));
        // a truncated buffer is malformed, not a shorter list
        assert!(parse_search_buffer(&buf[..buf.len() - 1], 3, 257).is_none());
    }

    #[test]
    fn equal_extents_give_equal_signatures() {
        let extents = [reg(0, 1 << 20, 4096, 9), reg(4096, 2 << 20, 8192, 9)];
        let s1 = signature(&FSID, 12_288, &extents).unwrap();
        let s2 = signature(&FSID, 12_288, &extents.clone()).unwrap();
        assert_eq!(s1, s2);
        assert!(s1.starts_with(SIGNATURE_TAG));
    }

    #[test]
    fn every_field_is_part_of_the_signature() {
        let base = reg(0, 1 << 20, 8192, 9);
        let s = signature(&FSID, 8192, &[base]).unwrap();
        let variants = [
            FileExtent {
                generation: 10,
                ..base
            },
            FileExtent {
                disk_bytenr: 3 << 20,
                ..base
            },
            FileExtent {
                disk_num_bytes: 4096,
                ..base
            },
            FileExtent {
                offset: 4096,
                ..base
            },
            FileExtent {
                ram_bytes: 16_384,
                ..base
            },
            FileExtent {
                compression: 3,
                ..base
            },
        ];
        for v in variants {
            assert_ne!(signature(&FSID, 8192, &[v]).unwrap(), s, "{v:?}");
        }
        assert_ne!(signature(&[8; 16], 8192, &[base]).unwrap(), s);
        assert_ne!(signature(&FSID, 8191, &[base]).unwrap(), s);
    }

    #[test]
    fn a_shared_prefix_with_a_different_tail_differs() {
        let shared = reg(0, 1 << 20, 4096, 9);
        let a = signature(&FSID, 8192, &[shared, reg(4096, 5 << 20, 4096, 11)]).unwrap();
        let b = signature(&FSID, 8192, &[shared, reg(4096, 6 << 20, 4096, 12)]).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn unusual_files_have_no_signature() {
        let r = reg(0, 1 << 20, 4096, 9);
        assert_eq!(signature(&FSID, 0, &[r]), Err(NoSignature::Empty));
        assert_eq!(signature(&FSID, 4096, &[]), Err(NoSignature::Empty));
        let inline = FileExtent {
            kind: EXTENT_INLINE,
            ..r
        };
        assert_eq!(signature(&FSID, 100, &[inline]), Err(NoSignature::Inline));
        let prealloc = FileExtent { kind: 2, ..r };
        assert_eq!(
            signature(&FSID, 4096, &[prealloc]),
            Err(NoSignature::NotRegular)
        );
        let encrypted = FileExtent { encryption: 1, ..r };
        assert_eq!(
            signature(&FSID, 4096, &[encrypted]),
            Err(NoSignature::Encoded)
        );
        let encoded = FileExtent {
            other_encoding: 1,
            ..r
        };
        assert_eq!(
            signature(&FSID, 4096, &[encoded]),
            Err(NoSignature::Encoded)
        );
        let hole_extent = FileExtent {
            disk_bytenr: 0,
            ..r
        };
        assert_eq!(
            signature(&FSID, 4096, &[hole_extent]),
            Err(NoSignature::Hole)
        );
        // a gap between extents (NO_HOLES filesystems)
        assert_eq!(
            signature(&FSID, 12_288, &[r, reg(8192, 2 << 20, 4096, 9)]),
            Err(NoSignature::Hole)
        );
        // a leading gap
        assert_eq!(
            signature(&FSID, 8192, &[reg(4096, 2 << 20, 4096, 9)]),
            Err(NoSignature::Hole)
        );
        // a trailing hole: extents stop before the size
        assert_eq!(signature(&FSID, 8192, &[r]), Err(NoSignature::Short));
        // overlapping items
        assert_eq!(
            signature(
                &FSID,
                8192,
                &[reg(0, 1 << 20, 8192, 9), reg(4096, 2 << 20, 4096, 9)]
            ),
            Err(NoSignature::Unordered)
        );
    }

    #[test]
    fn inline_items_parse_without_the_disk_fields() {
        let inline = FileExtent {
            kind: EXTENT_INLINE,
            disk_bytenr: 0,
            disk_num_bytes: 0,
            offset: 0,
            num_bytes: 0,
            ..reg(0, 0, 0, 9)
        };
        let mut bytes = item_bytes(&inline);
        bytes.extend_from_slice(b"inline data");
        assert_eq!(FileExtent::parse(0, &bytes), Some(inline));
        assert_eq!(FileExtent::parse(0, &bytes[..10]), None);
    }
}
