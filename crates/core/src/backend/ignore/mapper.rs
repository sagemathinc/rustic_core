#[cfg(not(windows))]
pub mod nix_mapper;

use std::{ffi::OsStr, path::Path};

use derive_setters::Setters;
use ignore::DirEntry;
use jiff::Timestamp;
use log::warn;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::{IgnoreErrorKind, IgnoreResult, OpenFile};
use crate::backend::{
    ReadSourceEntry,
    node::{
        ExtendedAttribute, Metadata, Node, NodeType,
        modification::{DevIdOption, TimeOption, XattrOption},
    },
};

#[cfg(not(windows))]
use std::os::unix::fs::{FileTypeExt, MetadataExt};

#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockdevOption {
    #[default]
    Special,
    File,
}

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(serde::Deserialize, serde::Serialize, Default, Clone, Copy, Debug, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[non_exhaustive]
/// [`LocalSourceSaveOptions`] describes how entries from a local source will be saved in the repository.
pub struct LocalSourceSaveOptions {
    /// Set access time [default: mtime]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_atime: Option<TimeOption>,

    /// Set changed time [default: yes]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_ctime: Option<TimeOption>,

    /// Set device ID [default: hardlink]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_devid: Option<DevIdOption>,

    /// How block devices should be stored [default: special]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_blockdev: Option<BlockdevOption>,

    /// Set extended attributes [default: yes]
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub set_xattrs: Option<XattrOption>,
}

impl LocalSourceSaveOptions {
    /// Maps a [`DirEntry`] to a [`ReadSourceEntry`].
    ///
    /// # Arguments
    ///
    /// * `entry` - The [`DirEntry`] to map.
    /// * `options` - options for saving entries
    ///
    /// # Errors
    ///
    /// * If metadata could not be read.
    /// * If the xattr of the entry could not be read.
    pub fn map_entry(self, entry: DirEntry) -> IgnoreResult<ReadSourceEntry<OpenFile>> {
        self.map_entry_with_metadata_strictness(entry, false, RootDevice::NoDevices)
    }

    /// Maps a directory entry; an entry that `root_dev` (its backup root's
    /// device) marks is flagged
    /// ([`crate::repofile::Node::set_other_filesystem`]).
    pub(crate) fn map_entry_with_metadata_strictness(
        self,
        entry: DirEntry,
        strict_metadata: bool,
        root_dev: RootDevice,
    ) -> IgnoreResult<ReadSourceEntry<OpenFile>> {
        let name = entry.file_name();
        let m = entry
            .metadata()
            .map_err(|err| IgnoreErrorKind::AcquiringMetadataFailed {
                name: name.to_string_lossy().to_string(),
                source: err,
            })?;

        let mtime = m.modified().ok().and_then(|t| Timestamp::try_from(t).ok());
        let atime = || m.accessed().ok().and_then(|t| Timestamp::try_from(t).ok());
        let atime = self
            .set_atime
            .unwrap_or(TimeOption::Mtime)
            .map_or_else(atime, mtime);
        let ctime = || Self::ctime(&m);
        let ctime = self
            .set_ctime
            .unwrap_or(TimeOption::Yes)
            .map_or_else(ctime, mtime);

        let (uid, user, gid, group) = Self::user_group(&m);
        let size = if m.is_dir() { 0 } else { m.len() };
        let device_id = self
            .set_devid
            .unwrap_or_default()
            .map_or_else(|| Self::device_id(&m), Self::hardlink(&m));
        let extended_attributes =
            read_xattrs(self.set_xattrs.unwrap_or_default(), strict_metadata, || {
                Self::xattrs(entry.path())
            })?;
        let (mode, inode, links) = Self::nix_infos(&m);

        let meta = Metadata {
            mode,
            mtime,
            atime,
            ctime,
            uid,
            gid,
            user,
            group,
            inode,
            device_id,
            size,
            links,
            extended_attributes,
        };

        let mut node = self.to_node(&entry, &m, meta)?;
        node.set_other_filesystem(root_dev.marks(&m));
        let path = entry.into_path();
        let open = Some(OpenFile(path.clone()));
        Ok(ReadSourceEntry { path, node, open })
    }

    fn to_node(
        self,
        entry: &DirEntry,
        m: &std::fs::Metadata,
        meta: Metadata,
    ) -> IgnoreResult<Node> {
        let name = entry.file_name();
        let node = if m.is_dir() {
            Node::new_node(name, NodeType::Dir, meta)
        } else if m.is_symlink() {
            let path = entry.path();
            let target = std::fs::read_link(path).map_err(|err| IgnoreErrorKind::ErrorLink {
                path: path.to_path_buf(),
                source: err,
            })?;
            let node_type = NodeType::from_link(&target);
            Node::new_node(name, node_type, meta)
        } else {
            self.to_node_other(name, m, meta)
        };
        Ok(node)
    }
}

fn read_xattrs(
    option: XattrOption,
    strict: bool,
    read: impl FnOnce() -> IgnoreResult<Vec<ExtendedAttribute>>,
) -> IgnoreResult<Vec<ExtendedAttribute>> {
    if option == XattrOption::No {
        return Ok(Vec::new());
    }
    match read() {
        Ok(attributes) => Ok(attributes),
        Err(error) if strict => Err(error),
        Err(error) => {
            warn!("ignoring error obtaining xattrs: {error}");
            Ok(Vec::new())
        }
    }
}

#[cfg(all(test, not(any(windows, target_os = "openbsd"))))]
mod strict_metadata_tests {
    use super::*;

    fn denied() -> IgnoreResult<Vec<ExtendedAttribute>> {
        Err(IgnoreErrorKind::ErrorXattr {
            path: "file".into(),
            source: std::io::ErrorKind::PermissionDenied.into(),
        })
    }

    #[test]
    fn strict_metadata_does_not_substitute_empty_xattrs_on_read_failure() {
        assert!(read_xattrs(XattrOption::Yes, true, denied).is_err());
        assert_eq!(
            read_xattrs(XattrOption::Yes, false, denied).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn intentional_scope_exclusion_does_not_read_attributes() {
        assert_eq!(
            read_xattrs(XattrOption::No, true, || panic!("must not read")).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn strict_metadata_preserves_empty_and_nonempty_attributes() {
        assert_eq!(
            read_xattrs(XattrOption::Yes, true, || Ok(Vec::new())).unwrap(),
            Vec::new()
        );
        let attributes = vec![ExtendedAttribute {
            name: "user.test".into(),
            value: Some(vec![1, 2, 3]),
        }];
        assert_eq!(
            read_xattrs(XattrOption::Yes, true, || Ok(attributes.clone())).unwrap(),
            attributes
        );
    }
}

#[cfg(not(windows))]
impl LocalSourceSaveOptions {
    fn ctime(m: &std::fs::Metadata) -> Option<Timestamp> {
        #[allow(clippy::cast_possible_truncation)]
        Timestamp::new(m.ctime(), m.ctime_nsec() as i32).ok()
    }

    fn device_id(m: &std::fs::Metadata) -> u64 {
        m.dev()
    }

    fn hardlink(m: &std::fs::Metadata) -> bool {
        m.nlink() > 1 && !m.is_dir()
    }

    fn user_group(
        m: &std::fs::Metadata,
    ) -> (Option<u32>, Option<String>, Option<u32>, Option<String>) {
        let uid = m.uid();
        let gid = m.gid();
        let user = nix_mapper::get_user_by_uid(uid);
        let group = nix_mapper::get_group_by_gid(gid);
        (Some(uid), user, Some(gid), group)
    }

    fn nix_infos(m: &std::fs::Metadata) -> (Option<u32>, u64, u64) {
        let mode = nix_mapper::map_mode_to_go(m.mode());
        let inode = m.ino();
        let links = if m.is_dir() { 0 } else { m.nlink() };
        (Some(mode), inode, links)
    }

    /// List [`ExtendedAttribute`] for a [`Node`] located at `path`
    ///
    /// # Argument
    ///
    /// * `path` to the [`Node`] for which to list attributes
    ///
    /// # Errors
    ///
    /// * If Xattr couldn't be listed or couldn't be read
    #[cfg(not(target_os = "openbsd"))]
    fn xattrs(path: &Path) -> IgnoreResult<Vec<ExtendedAttribute>> {
        xattr::list(path)
            .map_err(|err| IgnoreErrorKind::ErrorXattr {
                path: path.to_path_buf(),
                source: err,
            })?
            .map(|name| {
                Ok(ExtendedAttribute {
                    name: name.to_string_lossy().to_string(),
                    value: xattr::get(path, name).map_err(|err| IgnoreErrorKind::ErrorXattr {
                        path: path.to_path_buf(),
                        source: err,
                    })?,
                })
            })
            .collect::<IgnoreResult<Vec<ExtendedAttribute>>>()
    }

    #[cfg(target_os = "openbsd")]
    fn xattrs(_path: &Path) -> IgnoreResult<Vec<ExtendedAttribute>> {
        Ok(Vec::new())
    }

    fn to_node_other(self, name: &OsStr, m: &std::fs::Metadata, meta: Metadata) -> Node {
        let filetype = m.file_type();
        if filetype.is_block_device() {
            if matches!(self.set_blockdev.unwrap_or_default(), BlockdevOption::File) {
                Node::new_node(name, NodeType::File, meta)
            } else {
                let node_type = NodeType::Dev { device: m.rdev() };
                Node::new_node(name, node_type, meta)
            }
        } else if filetype.is_char_device() {
            let node_type = NodeType::Chardev { device: m.rdev() };
            Node::new_node(name, node_type, meta)
        } else if filetype.is_fifo() {
            Node::new_node(name, NodeType::Fifo, meta)
        } else if filetype.is_socket() {
            Node::new_node(name, NodeType::Socket, meta)
        } else {
            Node::new_node(name, NodeType::File, meta)
        }
    }
}

#[cfg(windows)]
impl LocalSourceSaveOptions {
    fn ctime(m: &std::fs::Metadata) -> Option<Timestamp> {
        m.created().ok().and_then(|t| Timestamp::try_from(t).ok())
    }
    fn device_id(_m: &std::fs::Metadata) -> u64 {
        0
    }
    fn hardlink(m: &std::fs::Metadata) -> bool {
        false
    }
    fn user_group(
        _m: &std::fs::Metadata,
    ) -> (Option<u32>, Option<String>, Option<u32>, Option<String>) {
        (None, None, None, None)
    }

    fn nix_infos(_m: &std::fs::Metadata) -> (Option<u32>, u64, u64) {
        (None, 0, 0)
    }

    fn xattrs(_path: &Path) -> IgnoreResult<Vec<ExtendedAttribute>> {
        Ok(Vec::new())
    }

    fn to_node_other(self, name: &OsStr, _m: &std::fs::Metadata, meta: Metadata) -> Node {
        Node::new_node(name, NodeType::File, meta)
    }
}

/// The device of a backup root, which the entries below it are compared
/// with (see [`crate::repofile::Node::set_other_filesystem`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RootDevice {
    /// No device ids on this platform: nothing is marked.
    NoDevices,
    /// The root's device: entries on another one are marked.
    Known(u64),
    /// The root's device couldn't be read: every entry is marked, so nothing
    /// below this root gets anything from the parent snapshot.
    Unknown,
}

impl RootDevice {
    /// The device of a backup root, from the result of reading its metadata.
    pub(crate) fn of<E>(metadata: Result<std::fs::Metadata, E>) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            metadata.map_or(Self::Unknown, |m| Self::Known(m.dev()))
        }
        #[cfg(not(unix))]
        {
            _ = metadata;
            Self::NoDevices
        }
    }

    /// Whether an entry with this metadata is on another filesystem than the
    /// root (or must be treated as such).
    pub(crate) fn marks(self, m: &std::fs::Metadata) -> bool {
        match self {
            Self::NoDevices => false,
            Self::Unknown => true,
            Self::Known(root) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    m.dev() != root
                }
                #[cfg(not(unix))]
                {
                    _ = (m, root);
                    false
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod root_device_tests {
    use super::RootDevice;

    #[test]
    fn an_unreadable_root_marks_everything() {
        let err: Result<std::fs::Metadata, std::io::Error> = Err(std::io::Error::other("x"));
        let root = RootDevice::of(err);
        assert_eq!(root, RootDevice::Unknown);
        let m = std::fs::metadata(std::env::temp_dir()).unwrap();
        assert!(root.marks(&m));
    }

    #[test]
    fn a_known_root_marks_only_other_devices() {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(std::env::temp_dir()).unwrap();
        let root = RootDevice::of::<std::io::Error>(Ok(m.clone()));
        assert_eq!(root, RootDevice::Known(m.dev()));
        assert!(!root.marks(&m));
        assert!(RootDevice::Known(m.dev().wrapping_add(1)).marks(&m));
        assert!(!RootDevice::NoDevices.marks(&m));
    }
}
