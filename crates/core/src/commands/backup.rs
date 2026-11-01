//! `backup` subcommand
use derive_setters::Setters;
use itertools::Itertools;
use jiff::Timestamp;
use log::{info, warn};

use std::path::PathBuf;

use path_dedot::ParseDot;
use serde_derive::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};

use crate::{
    BackupAdmissionOptions, CommandInput, Excludes, ReadSource,
    archiver::{Archiver, parent::Parent},
    backend::{
        childstdout::ChildStdoutSource,
        dry_run::DryRunBackend,
        ignore::{LocalSource, LocalSourceFilterOptions, LocalSourceSaveOptions},
        stdin::StdinSource,
    },
    backup_inventory,
    error::{ErrorKind, RusticError, RusticResult},
    repofile::{
        PathList, SnapshotFile,
        snapshotfile::{
            SnapshotId,
            grouping::{SnapshotGroup, SnapshotGroupCriterion},
        },
    },
    repository::{IndexedIds, IndexedTree, Repository},
};

#[cfg(feature = "clap")]
use clap::ValueHint;

/// `backup` subcommand
#[serde_as]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[allow(clippy::struct_excessive_bools)]
#[non_exhaustive]
/// Options how the backup command uses a parent snapshot.
pub struct ParentOptions {
    /// Group snapshots by any combination of host,label,paths,tags to find a suitable parent (default: host,label,paths)
    #[cfg_attr(feature = "clap", clap(long, short = 'g', value_name = "CRITERION",))]
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub group_by: Option<SnapshotGroupCriterion>,

    /// Snapshot to use as parent (can be specified multiple times)
    #[cfg_attr(
        feature = "clap",
        clap(long = "parent", value_name = "SNAPSHOT", conflicts_with = "force")
    )]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::vec::append))]
    pub parents: Vec<String>,

    /// Skip writing of snapshot if nothing changed w.r.t. the parent snapshot.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub skip_if_unchanged: bool,

    /// Use no parent, read all files
    #[cfg_attr(feature = "clap", clap(long, short))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub force: bool,

    /// Ignore ctime changes when checking for modified files
    #[cfg_attr(feature = "clap", clap(long, conflicts_with = "force"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub ignore_ctime: bool,

    /// Ignore inode number changes when checking for modified files
    #[cfg_attr(feature = "clap", clap(long, conflicts_with = "force"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub ignore_inode: bool,

    /// The backup source is a restore of the (single, explicit) parent
    /// snapshot, and TIME (RFC 3339) is when that restore returned. A file
    /// whose ctime is more than 2 s before TIME (`RESTORED_BEFORE_MARGIN`)
    /// matches its parent node on type, size and mtime alone: its ctime and
    /// inode, new on the restored tree, are not compared. Files changed later,
    /// and those the restore wrote in its last 2 s, are compared as usual.
    /// The margin covers coarse kernel timestamps (a write just after TIME
    /// can get a ctime up to one timer tick before it).
    ///
    /// Only for a tree that nothing but the restore wrote: restored complete
    /// into an empty directory, with no other writer (the project included)
    /// from before the restore started until TIME was taken. TIME is taken on
    /// the host that ran the restore, with the clock that stamps the files'
    /// ctimes: a local filesystem, not a remote one whose server clock may be
    /// skewed by more than the margin. That clock must not step back before
    /// this backup. A change made during
    /// that window with the same size and mtime would be missed. A TIME in
    /// the future is an error; without exactly one `--parent` the option is
    /// ignored with a warning.
    #[cfg_attr(
        feature = "clap",
        clap(long, value_name = "TIME", conflicts_with = "force")
    )]
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub restored_before: Option<Timestamp>,

    /// Find the parent of a moved or renamed directory by its inode: a
    /// directory that has no parent directory with its inode at its path also
    /// looks for its files in the parent directory with that inode. Such a file
    /// must match with full identity, whatever the other options: type, size,
    /// mtime, nonzero equal inode and equal ctime. So only unchanged files are
    /// found at their new path. A renamed file itself gets a new ctime, so
    /// only files inside a moved directory are found. On first use this loads
    /// every tree of the parent snapshot into an index of directories.
    ///
    /// Inode numbers identify a directory only within one filesystem (or
    /// btrfs subvolume), so this needs `--one-file-system` and exactly one
    /// backup path, and every parent must have been made the same way of the
    /// same path (recorded in the snapshot as `single_filesystem`; snapshots
    /// of older versions don't have it). Otherwise it is ignored with a
    /// warning. With a custom `ReadSource` (`Repository::archive`), setting
    /// `--one-file-system` is the caller's assertion that the source stays
    /// within one filesystem.
    #[cfg_attr(feature = "clap", clap(long, conflicts_with = "force"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub detect_moves: bool,

    /// The most entries (parent directories indexed plus trees visited) the
    /// `--detect-moves` index may hold [default: 1000000]. A parent snapshot
    /// that needs more turns move detection off for the backup, with a
    /// warning; files are then matched at their own path only.
    #[cfg_attr(feature = "clap", clap(long, value_name = "N"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub detect_moves_max_entries: Option<usize>,
}

/// How long before `--restored-before` a file's ctime must be.
///
/// Only such a file is taken as written by the restore. This is more than a
/// timer tick of coarse kernel timestamps (up to 10 ms), with room for clock
/// uncertainty.
pub const RESTORED_BEFORE_MARGIN: jiff::SignedDuration = jiff::SignedDuration::from_secs(2);

/// The default of `ParentOptions::detect_moves_max_entries`: about 100 MB
/// of index at most.
pub const DETECT_MOVES_MAX_ENTRIES: usize = 1_000_000;

impl ParentOptions {
    /// Fails if [`ParentOptions::restored_before`] is in the future: every
    /// file would match, including those changed after the restore.
    ///
    /// # Errors
    ///
    /// * If `restored_before` is later than now.
    pub(crate) fn validate(&self) -> RusticResult<()> {
        if let Some(restored_before) = self.restored_before
            && restored_before > Timestamp::now()
        {
            return Err(RusticError::new(
                ErrorKind::InvalidInput,
                "--restored-before `{time}` is in the future.",
            )
            .attach_context("time", restored_before.to_string()));
        }
        Ok(())
    }

    /// The ctime cutoff for restored files (see
    /// [`ParentOptions::restored_before`]): the restore time less
    /// [`RESTORED_BEFORE_MARGIN`], and only with exactly one explicit parent,
    /// which must be the snapshot the source was restored from.
    fn restored_cutoff(&self) -> Option<Timestamp> {
        let restored_before = self.restored_before?;
        if self.force || self.parents.len() != 1 {
            warn!(
                "ignoring --restored-before: it needs exactly one --parent, the restored snapshot"
            );
            return None;
        }
        restored_before.checked_sub(RESTORED_BEFORE_MARGIN).ok()
    }

    /// Get parent snapshot.
    ///
    /// # Type Parameters
    ///
    /// * `R` - The type of the indexed tree (repository).
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository to use
    /// * `snap` - The snapshot to use
    ///
    /// # Returns
    ///
    /// The parent snapshot ids and the parent object.
    pub(crate) fn get_parent<R: IndexedTree>(
        &self,
        repo: &Repository<R>,
        snap: &SnapshotFile,
    ) -> (Vec<SnapshotId>, Parent) {
        let group = SnapshotGroup::from_snapshot(snap, self.group_by.unwrap_or_default());
        let parent = if self.force {
            Vec::new()
        } else if self.parents.is_empty() {
            // get suitable snapshot group from snapshot and opts.group_by. This is used to filter snapshots for the parent detection
            SnapshotFile::latest(
                repo.dbe(),
                |snap| group.matches(snap),
                &repo.progress_counter(""),
            )
            .ok()
            .into_iter()
            .collect()
        } else {
            SnapshotFile::from_strs(
                repo.dbe(),
                &self.parents,
                |snap| group.matches(snap),
                &repo.progress_counter(""),
            )
            .unwrap_or_default()
        };

        // Inode numbers identify a directory only within one filesystem, and
        // a parent of another tree has unrelated ones: move detection needs
        // this snapshot and every parent to be single-filesystem sources of
        // the same paths.
        let detect_moves_ok = snap.single_filesystem
            && parent
                .iter()
                .all(|parent| parent.single_filesystem && parent.paths == snap.paths);
        if self.detect_moves && !detect_moves_ok {
            warn!(
                "ignoring --detect-moves: it needs --one-file-system, one backup path, and parents made the same way of the same path"
            );
        }
        let (parent_trees, parent_ids): (Vec<_>, _) = parent
            .into_iter()
            .map(|parent| (parent.tree, parent.id))
            .unzip();

        (
            parent_ids,
            Parent::new(
                repo.dbe(),
                repo.index(),
                parent_trees,
                self.ignore_ctime,
                self.ignore_inode,
                self.restored_cutoff(),
                (self.detect_moves && detect_moves_ok).then(|| {
                    self.detect_moves_max_entries
                        .unwrap_or(DETECT_MOVES_MAX_ENTRIES)
                }),
            ),
        )
    }
}

#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[non_exhaustive]
#[allow(clippy::struct_excessive_bools)]
/// Options for the `backup` command.
pub struct BackupOptions {
    /// Set filename to be used when backing up from stdin
    #[cfg_attr(
        feature = "clap",
        clap(long, value_name = "FILENAME", default_value = "stdin", value_hint = ValueHint::FilePath)
    )]
    #[cfg_attr(feature = "merge", merge(skip))]
    pub stdin_filename: String,

    /// Call the given command and use its output as stdin
    #[cfg_attr(feature = "clap", clap(long, value_name = "COMMAND"))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub stdin_command: Option<CommandInput>,

    /// Manually set backup path in snapshot
    #[cfg_attr(feature = "clap", clap(long, value_name = "PATH", value_hint = ValueHint::DirPath))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub as_path: Option<PathBuf>,

    /// Don't scan the backup source for its size - this disables ETA estimation for backup.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub no_scan: bool,

    /// Fail without saving a snapshot if source inspection or file processing has errors.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub strict: bool,

    /// Read a file with several hard links only once per run: another path of
    /// the same inode (same device, inode, size, mtime and ctime) reuses the
    /// content already archived in this run.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub reuse_hardlinks: bool,

    /// Read a file whose btrfs data extents are exactly those of a file already
    /// archived in this run (or of an unchanged file vouched for by the parent
    /// snapshot) only once: the others reuse its content. Needs a read-only
    /// btrfs subvolume as the source and the privilege to search its tree;
    /// otherwise this logs why it does nothing and every file is read.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub reuse_extents: bool,

    /// Metadata-only work admission before content processing.
    #[cfg_attr(feature = "clap", clap(flatten))]
    #[serde(flatten)]
    pub admission: BackupAdmissionOptions,

    /// Dry-run mode: Don't write any data or snapshot
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::bool::overwrite_false))]
    pub dry_run: bool,

    #[cfg_attr(feature = "clap", clap(flatten))]
    #[serde(flatten)]
    /// Options how to use a parent snapshot
    pub parent_opts: ParentOptions,

    #[cfg_attr(feature = "clap", clap(flatten))]
    #[serde(flatten)]
    /// Options how to save entries from a local source
    pub ignore_save_opts: LocalSourceSaveOptions,

    #[cfg_attr(feature = "clap", clap(flatten))]
    #[serde(flatten)]
    /// excludes
    pub excludes: Excludes,

    #[cfg_attr(feature = "clap", clap(flatten))]
    #[serde(flatten)]
    /// Options how to filter from a local source
    pub ignore_filter_opts: LocalSourceFilterOptions,
}

/// Archive data, create a snapshot.
///
/// # Type Parameters
///
/// * `R` - The type of the indexed IDs (repository).
/// * `S` - The type of the backup source.
///
/// # Arguments
///
/// * `repo` - The repository to use
/// * `opts` - The backup options
/// * `src` - The source to backup
/// * `snap` - The snapshot with raw information
///
/// # Errors
///
/// * If sending the message to the raw packer fails.
/// * If converting the data length to u64 fails
/// * If sending the message to the raw packer fails.
/// * If the index file could not be serialized.
/// * If the time is not in the range of `Local::now()`
///
/// # Returns
///
/// The snapshot pointing to the backup'ed data.
pub(crate) fn archive<S, R>(
    repo: &Repository<R>,
    opts: &BackupOptions,
    src: &S,
    mut snap: SnapshotFile,
    backup_paths: &[PathBuf],
) -> RusticResult<SnapshotFile>
where
    R: IndexedIds,
    S: ReadSource + 'static,
    <S as ReadSource>::Open: Send,
    <S as ReadSource>::Iter: Send,
{
    if opts.admission.enabled() {
        let inventory = backup_inventory(src, repo.config(), &opts.admission)?;
        info!("backup admission passed: {inventory:?}");
    }
    let index = repo.index();

    let as_path = opts
        .as_path
        .as_ref()
        .map(|p| -> RusticResult<_> {
            Ok(p.parse_dot()
                .map_err(|err| {
                    RusticError::with_source(
                        ErrorKind::InvalidInput,
                        "Failed to parse dotted path `{path}`",
                        err,
                    )
                    .attach_context("path", p.display().to_string())
                })?
                .to_path_buf())
        })
        .transpose()?;

    let paths = as_path
        .as_ref()
        .map_or(backup_paths, |p| std::slice::from_ref(p));

    snap.paths.set_paths(paths).map_err(|err| {
        RusticError::with_source(
            ErrorKind::Internal,
            "Failed to set paths `{paths}` in snapshot.",
            err,
        )
        .attach_context(
            "paths",
            backup_paths
                .iter()
                .map(|p| p.display().to_string())
                .join(","),
        )
    })?;

    // Inode numbers identify a directory only within one filesystem (or btrfs
    // subvolume): record whether this source is one path read without
    // leaving its filesystem, as move detection requires of the snapshot and
    // of its parents. For a custom source, `--one-file-system` is the
    // caller's assertion that it stays within one filesystem.
    snap.single_filesystem = opts.ignore_filter_opts.one_file_system && backup_paths.len() == 1;

    opts.parent_opts.validate()?;
    let (parent_ids, parent) = opts.parent_opts.get_parent(repo, &snap);
    if parent_ids.is_empty() {
        info!("using no parent");
    } else {
        info!("using parents {}", parent_ids.iter().join(", "));
        snap.parent = Some(parent_ids[0]);
        snap.parents = parent_ids;
    }

    let be = DryRunBackend::new(repo.dbe().clone(), opts.dry_run);
    info!("starting to backup {backup_paths:?} ...");
    let archiver = Archiver::new(be, index, repo.config(), parent, snap)?
        .strict(opts.strict)
        .reuse_hardlinks(opts.reuse_hardlinks)
        .reuse_extents(opts.reuse_extents.then(|| backup_paths[0].as_path()));
    let p = repo.progress_bytes("backing up...");

    archiver.archive(
        src,
        &backup_paths[0],
        as_path.as_ref(),
        opts.parent_opts.skip_if_unchanged,
        opts.no_scan,
        &p,
    )
}

/// Backup data, create a snapshot.
///
/// # Type Parameters
///
/// * `R` - The type of the indexed IDs (repository).
///
/// # Arguments
///
/// * `repo` - The repository to use
/// * `opts` - The backup options
/// * `source` - The source to backup
/// * `snap` - The snapshot with raw information
///
/// # Errors
///
/// * If sending the message to the raw packer fails.
/// * If converting the data length to u64 fails
/// * If sending the message to the raw packer fails.
/// * If the index file could not be serialized.
/// * If the time is not in the range of `Local::now()`
///
/// # Returns
///
/// The snapshot pointing to the backup'ed data.
pub(crate) fn backup<R: IndexedIds>(
    repo: &Repository<R>,
    opts: &BackupOptions,
    source: &PathList,
    snap: SnapshotFile,
) -> RusticResult<SnapshotFile> {
    let backup_stdin = PathList::from_string("-")?;

    let snap = if *source == backup_stdin {
        let path = PathBuf::from(&opts.stdin_filename);
        let backup_paths = vec![path.clone()];
        // for stdin, use no parent
        let mut opts = opts.clone();
        opts.parent_opts.force = true;
        if let Some(command) = &opts.stdin_command {
            let src = ChildStdoutSource::new(command, path)?;
            let res = archive(repo, &opts, &src, snap, &backup_paths)?;
            src.finish()?;
            res
        } else {
            let src = StdinSource::new(path);
            archive(repo, &opts, &src, snap, &backup_paths)?
        }
    } else {
        let backup_path = source.paths();
        let src = LocalSource::new(
            opts.ignore_save_opts,
            &opts.excludes,
            &opts.ignore_filter_opts,
            &backup_path,
        )?;
        archive(repo, opts, &src, snap, &backup_path)?
    };

    Ok(snap)
}
