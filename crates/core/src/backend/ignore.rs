pub mod mapper;
pub use mapper::LocalSourceSaveOptions;
use mapper::RootDevice;

use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
};

use bytesize::ByteSize;
use derive_setters::Setters;
use ignore::{Walk, WalkBuilder};
use log::warn;
use serde_with::{DisplayFromStr, serde_as};

#[cfg(not(windows))]
use std::num::TryFromIntError;

use crate::{
    Excludes,
    backend::{ReadSource, ReadSourceEntry, ReadSourceOpen},
    error::{ErrorKind, RusticError, RusticResult},
};

/// [`IgnoreErrorKind`] describes the errors that can be returned by a Ignore action in Backends
#[derive(thiserror::Error, Debug, displaydoc::Display)]
pub enum IgnoreErrorKind {
    #[cfg(all(not(windows), not(target_os = "openbsd")))]
    /// Error getting xattrs for `{path:?}`: `{source:?}`
    ErrorXattr {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Error reading link target for `{path:?}`: `{source:?}`
    ErrorLink {
        path: PathBuf,
        source: std::io::Error,
    },
    #[cfg(not(windows))]
    /// Error converting ctime `{ctime}` and `ctime_nsec` `{ctime_nsec}` to Utc Timestamp: `{source:?}`
    CtimeConversionToTimestampFailed {
        ctime: i64,
        ctime_nsec: i64,
        source: TryFromIntError,
    },
    /// Error acquiring metadata for `{name}`: `{source:?}`
    AcquiringMetadataFailed { name: String, source: ignore::Error },
    /// time error
    JiffError(#[from] jiff::Error),
}

pub(crate) type IgnoreResult<T> = Result<T, IgnoreErrorKind>;

/// A [`LocalSource`] is a source from local paths which is used to be read from (i.e. to backup it).
#[derive(Debug)]
pub struct LocalSource {
    /// The walk builder.
    builder: WalkBuilder,
    /// Unsorted traversal avoids collecting a huge directory before its admission limit.
    inventory_builder: WalkBuilder,
    /// The save options to use.
    save_opts: LocalSourceSaveOptions,
}

#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(serde::Deserialize, serde::Serialize, Default, Clone, Debug, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[non_exhaustive]
/// [`LocalSourceFilterOptions`] allow to filter a local source by various criteria.
pub struct LocalSourceFilterOptions {
    /// Ignore files based on .gitignore files
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub git_ignore: bool,

    /// Do not require a git repository to apply git-ignore rule
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub no_require_git: bool,

    /// Treat the provided filename like a .gitignore file (can be specified multiple times)
    #[cfg_attr(
        feature = "clap",
        clap(long = "custom-ignorefile", value_name = "FILE")
    )]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::vec::overwrite_empty))]
    pub custom_ignorefiles: Vec<String>,

    /// Exclude contents of directories containing this filename (can be specified multiple times)
    #[cfg_attr(feature = "clap", clap(long, value_name = "FILE"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::vec::overwrite_empty))]
    pub exclude_if_present: Vec<String>,

    /// Exclude files/directories having the given extended attribute set (can be specified multiple times)
    #[cfg_attr(feature = "clap", clap(long, value_name = "XATTR"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::vec::overwrite_empty))]
    pub exclude_if_xattr: Vec<String>,

    /// Exclude other file systems, don't cross filesystem boundaries and subvolumes
    #[cfg_attr(feature = "clap", clap(long, short = 'x'))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub one_file_system: bool,

    /// Maximum size of files to be backed up. Larger files will be excluded.
    #[cfg_attr(feature = "clap", clap(long, value_name = "SIZE"))]
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub exclude_larger_than: Option<ByteSize>,
}

impl LocalSource {
    /// Create a local source from [`LocalSourceSaveOptions`], [`LocalSourceFilterOptions`] and backup path(s).
    ///
    /// # Arguments
    ///
    /// * `save_opts` - The [`LocalSourceSaveOptions`] to use.
    /// * `filter_opts` - The [`LocalSourceFilterOptions`] to use.
    /// * `backup_paths` - The backup path(s) to use.
    ///
    /// # Returns
    ///
    /// The created local source.
    ///
    /// # Errors
    ///
    /// * If the a glob pattern could not be added to the override builder.
    /// * If a glob file could not be read.
    #[allow(clippy::too_many_lines)]
    pub fn new(
        save_opts: LocalSourceSaveOptions,
        excludes: &Excludes,
        filter_opts: &LocalSourceFilterOptions,
        backup_paths: &[impl AsRef<Path>],
    ) -> RusticResult<Self> {
        let mut walk_builder = WalkBuilder::new(&backup_paths[0]);

        for path in &backup_paths[1..] {
            _ = walk_builder.add(path);
        }

        let overrides = excludes.as_override()?;

        for file in &filter_opts.custom_ignorefiles {
            _ = walk_builder.add_custom_ignore_filename(file);
        }

        _ = walk_builder
            .follow_links(false)
            .hidden(false)
            .ignore(false)
            .git_ignore(filter_opts.git_ignore)
            .git_exclude(filter_opts.git_ignore)
            .require_git(!filter_opts.no_require_git)
            .same_file_system(filter_opts.one_file_system)
            .max_filesize(filter_opts.exclude_larger_than.map(|s| s.as_u64()))
            .overrides(overrides);

        let exclude_if_present = filter_opts.exclude_if_present.clone();
        let exclude_if_xattr: Vec<OsString> = filter_opts
            .exclude_if_xattr
            .iter()
            .map(OsString::from)
            .collect();

        if !exclude_if_xattr.is_empty() {
            #[cfg(any(windows, target_os = "openbsd"))]
            warn!("exclude-if-xattr is not supported on this platform");
            #[cfg(not(any(windows, target_os = "openbsd")))]
            if !xattr::SUPPORTED_PLATFORM {
                warn!("exclude-if-xattr is not supported on this platform");
            }
        }

        let needs_entry_filter = !exclude_if_present.is_empty() || !exclude_if_xattr.is_empty();

        if needs_entry_filter {
            _ = walk_builder.filter_entry(move |entry| {
                // exclude-if-present: skip directories containing a marker file
                if !exclude_if_present.is_empty()
                    && let Some(tpe) = entry.file_type()
                    && tpe.is_dir()
                    && exclude_if_present
                        .iter()
                        .any(|file| entry.path().join(file).exists())
                {
                    return false;
                }

                // exclude-if-xattr: skip entries that have a matching xattr
                #[cfg(not(any(windows, target_os = "openbsd")))]
                if xattr::SUPPORTED_PLATFORM && !exclude_if_xattr.is_empty() {
                    match xattr::list(entry.path()) {
                        Ok(mut attrs) => {
                            if attrs.any(|attr| exclude_if_xattr.contains(&attr)) {
                                return false;
                            }
                        }
                        Err(err) => {
                            warn!(
                                "Error reading xattrs for {}, not excluding: {err}",
                                entry.path().display()
                            );
                        }
                    }
                }

                true
            });
        }

        let inventory_builder = walk_builder.clone();
        _ = walk_builder.sort_by_file_path(Path::cmp);
        let builder = walk_builder;

        Ok(Self {
            builder,
            inventory_builder,
            save_opts,
        })
    }
}

#[derive(Debug)]
/// Describes an open file from the local backend.
pub struct OpenFile(PathBuf);

impl ReadSourceOpen for OpenFile {
    type Reader = File;

    fn open_with_holes(self) -> RusticResult<super::source_reader::SourceReader<File>> {
        super::source_reader::SourceReader::file(self.open()?).map_err(|err| {
            RusticError::with_source(
                ErrorKind::InputOutput,
                "Failed to initialize local file reader.",
                err,
            )
        })
    }

    /// Open the file from the local backend.
    ///
    /// # Returns
    ///
    /// The read handle to the file from the local backend.
    ///
    /// # Errors
    ///
    /// * If the file could not be opened.
    fn open(self) -> RusticResult<Self::Reader> {
        let path = self.0;
        File::open(&path).map_err(|err| {
            RusticError::with_source(
                ErrorKind::InputOutput,
                "Failed to open file at `{path}`. Please make sure the file exists and is accessible.",
                err,
            )
            .attach_context("path", path.display().to_string())
        })
    }
}

impl ReadSource for LocalSource {
    type Open = OpenFile;
    type Iter = LocalSourceWalker;

    /// Get the size of the local source.
    ///
    /// # Returns
    ///
    /// The size of the local source or `None` if the size could not be determined.
    ///
    /// # Errors
    ///
    /// * If the size could not be determined.
    fn size(&self) -> RusticResult<Option<u64>> {
        let mut size = 0;
        for entry in self.builder.build() {
            if let Err(err) = entry.and_then(|e| e.metadata()).map(|m| {
                size += if m.is_dir() { 0 } else { m.len() };
            }) {
                warn!("ignoring error {err}");
            }
        }
        Ok(Some(size))
    }

    /// Iterate over the entries of the local source.
    ///
    /// # Returns
    ///
    /// An iterator over the entries of the local source.
    fn entries(&self) -> Self::Iter {
        LocalSourceWalker::new(self.builder.build(), self.save_opts, false)
    }

    fn strict_entries(&self) -> Self::Iter {
        LocalSourceWalker::new(self.builder.build(), self.save_opts, true)
    }

    fn inventory_entries(&self) -> Option<Self::Iter> {
        Some(LocalSourceWalker::new(
            self.inventory_builder.build(),
            self.save_opts,
            true,
        ))
    }
}

fn ignore_error_path(err: &ignore::Error) -> Option<String> {
    match err {
        ignore::Error::WithPath { path, .. } => Some(path.display().to_string()),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            ignore_error_path(err)
        }
        ignore::Error::Loop { child, .. } => Some(child.display().to_string()),
        _ => None,
    }
}

// Walk doesn't implement Debug
#[allow(missing_debug_implementations)]
pub struct LocalSourceWalker {
    /// The walk iterator.
    walker: Walk,
    /// The save options to use.
    save_opts: LocalSourceSaveOptions,
    strict_metadata: bool,
    /// The device of the backup root being walked.
    root_dev: RootDevice,
    /// A walk item held back while an error about its backup root is
    /// returned first (the end of the walk needs no holding back: the walker
    /// keeps returning `None`).
    pending: Option<Result<ignore::DirEntry, ignore::Error>>,
    /// Tests only: reading a backup root's metadata fails.
    #[cfg(test)]
    fail_root_metadata: bool,
}

impl LocalSourceWalker {
    fn new(walker: Walk, save_opts: LocalSourceSaveOptions, strict_metadata: bool) -> Self {
        Self {
            walker,
            save_opts,
            strict_metadata,
            root_dev: RootDevice::NoDevices,
            pending: None,
            #[cfg(test)]
            fail_root_metadata: false,
        }
    }

    /// Starts a backup root (an entry of depth 0): its device is what the
    /// entries below it are compared with. If its metadata can't be read,
    /// everything below the root is treated as on another filesystem, and an
    /// error is returned to be reported.
    fn enter_root<E: std::fmt::Display>(
        &mut self,
        path: &Path,
        metadata: Result<std::fs::Metadata, E>,
    ) -> Option<Box<RusticError>> {
        let message = metadata.as_ref().err().map(ToString::to_string);
        self.root_dev = RootDevice::of(metadata);
        (self.root_dev == RootDevice::Unknown).then(|| {
            RusticError::new(
                ErrorKind::InputOutput,
                "Failed to read the metadata of backup root `{path}`: nothing below it is matched with the parent snapshot. {message}",
            )
            .attach_context("path", path.display().to_string())
            .attach_context("message", message.unwrap_or_default())
        })
    }

    fn map_item(
        &self,
        item: Option<Result<ignore::DirEntry, ignore::Error>>,
    ) -> Option<RusticResult<ReadSourceEntry<OpenFile>>> {
        let root_dev = self.root_dev;
        item.map(|e| {
            let entry = e.map_err(|err| {
                let path = ignore_error_path(&err);
                let rustic_err = if path.is_some() {
                    RusticError::with_source(
                        ErrorKind::InputOutput,
                        "Failed to read source path `{path}`.",
                        err,
                    )
                } else {
                    RusticError::with_source(
                        ErrorKind::InputOutput,
                        "Failed to read source path.",
                        err,
                    )
                };
                match path {
                    Some(path) => rustic_err.attach_context("path", path),
                    None => rustic_err,
                }
            })?;
            let path = entry.path().display().to_string();
            self.save_opts
                .map_entry_with_metadata_strictness(entry, self.strict_metadata, root_dev)
                .map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::InputOutput,
                        "Failed to map directory entry `{path}` to a backup source entry.",
                        err,
                    )
                    .attach_context("path", path)
                })
        })
    }

    /// Enters a backup root if `item` is one; returns the error about it, if
    /// any.
    fn check_root(
        &mut self,
        item: Option<&Result<ignore::DirEntry, ignore::Error>>,
    ) -> Option<Box<RusticError>> {
        match item {
            Some(Ok(entry)) if entry.depth() == 0 => {
                let path = entry.path().to_path_buf();
                let metadata = entry.metadata().map_err(|err| err.to_string());
                #[cfg(test)]
                let metadata = if self.fail_root_metadata {
                    Err("injected".to_string())
                } else {
                    metadata
                };
                self.enter_root(&path, metadata)
            }
            _ => None,
        }
    }
}

impl Iterator for LocalSourceWalker {
    type Item = RusticResult<ReadSourceEntry<OpenFile>>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.pending.take() {
            return self.map_item(Some(item));
        }
        let mut item = self.walker.next();
        // a backup root (depth 0) sets the device its entries are compared with
        let mut root_err = self.check_root(item.as_ref());
        // ignore root dir, i.e. an entry with depth 0 of type dir
        if let Some(Ok(entry)) = &item
            && entry.depth() == 0
            && entry.file_type().unwrap().is_dir()
        {
            item = self.walker.next();
            root_err = self.check_root(item.as_ref()).or(root_err);
        }
        if let Some(err) = root_err {
            self.pending = item;
            return Some(Err(err));
        }
        self.map_item(item)
    }
}

#[cfg(all(test, unix))]
mod root_device_walk_tests {
    use super::*;

    fn walk(dir: &Path, fail_root_metadata: bool) -> Vec<Result<(String, bool), String>> {
        let source = LocalSource::new(
            LocalSourceSaveOptions::default(),
            &Excludes::default(),
            &LocalSourceFilterOptions::default(),
            &[dir],
        )
        .unwrap();
        let mut walker = source.entries();
        walker.fail_root_metadata = fail_root_metadata;
        walker
            .map(|item| {
                item.map(|entry| {
                    let name = entry
                        .path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    (name, entry.node.is_other_filesystem())
                })
                .map_err(|err| err.to_string())
            })
            .collect()
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::fs::write(dir.path().join("d/f"), b"x").unwrap();
        std::fs::write(dir.path().join("g"), b"y").unwrap();
        dir
    }

    #[test]
    fn on_the_root_filesystem_nothing_is_marked() {
        let dir = tree();
        let items = walk(dir.path(), false);
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|item| matches!(item, Ok((_, false)))));
    }

    #[test]
    fn an_unreadable_root_is_reported_and_everything_below_it_is_marked() {
        let dir = tree();
        let items = walk(dir.path(), true);
        // the error comes first, then every entry, all marked
        assert!(
            matches!(&items[0], Err(err) if err.contains("backup root")),
            "{items:?}"
        );
        assert_eq!(items.len(), 4);
        assert!(items[1..].iter().all(|item| matches!(item, Ok((_, true)))));
    }
}
