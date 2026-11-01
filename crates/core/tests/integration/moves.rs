//! `ParentOptions::detect_moves`: files inside a moved or renamed directory
//! are found in the parent by the directory's inode, and only if they are
//! unchanged.
#![cfg(unix)]

use std::{collections::BTreeMap, fs, path::Path, path::PathBuf};

use anyhow::Result;
use pretty_assertions::assert_eq;
use rstest::rstest;

use rustic_core::{
    BackupOptions, DataId, IndexedIds, LocalSourceFilterOptions, LsOptions, ParentOptions,
    PathList, Repository, repofile::SnapshotFile,
};

use super::{RepoOpen, set_up_repo};

/// Write `dir/name` with 100 kB of `byte` and a fixed mtime.
fn write(dir: &Path, name: &str, byte: u8) -> Result<()> {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, vec![byte; 100_000])?;
    let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    fs::File::options()
        .write(true)
        .open(&path)?
        .set_modified(when)?;
    Ok(())
}

fn backup<S: IndexedIds>(
    repo: &Repository<S>,
    dir: &Path,
    parent_opts: ParentOptions,
) -> Result<SnapshotFile> {
    Ok(repo.backup(
        &BackupOptions::default()
            .no_scan(true)
            .as_path(PathBuf::from("/home"))
            .ignore_filter_opts(LocalSourceFilterOptions::default().one_file_system(true))
            .parent_opts(parent_opts),
        &PathList::from_iter([dir]),
        SnapshotFile::default(),
    )?)
}

fn parent(snap: &SnapshotFile, detect_moves: bool) -> ParentOptions {
    ParentOptions::default()
        .parents(vec![snap.id.to_string()])
        .detect_moves(detect_moves)
}

/// (files read: new or changed, files unmodified) of a backup.
fn counts(snap: &SnapshotFile) -> (u64, u64) {
    let summary = snap.summary.as_ref().unwrap();
    (
        summary.files_new + summary.files_changed,
        summary.files_unmodified,
    )
}

/// The content ids of every file of a snapshot, by path.
fn content<S: IndexedIds>(
    repo: &Repository<S>,
    snap: &SnapshotFile,
) -> Result<BTreeMap<String, Vec<DataId>>> {
    let node = repo.node_from_snapshot_and_path(snap, "home")?;
    Ok(repo
        .ls(&node, &LsOptions::default().recursive(true))?
        .filter_map(|item| {
            let (path, node) = item.unwrap();
            node.content.map(|c| (path.display().to_string(), c))
        })
        .collect())
}

/// A tree with `big/` (4 files, one nested) and `other`.
fn tree() -> Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    for (name, byte) in [
        ("big/a", b'a'),
        ("big/b", b'b'),
        ("big/c", b'c'),
        ("big/sub/d", b'd'),
        ("other", b'o'),
    ] {
        write(dir.path(), name, byte)?;
    }
    Ok(dir)
}

/// Back up [`tree`], apply `change` to it, and back it up again without and
/// with `detect_moves`; return the two second backups and the repository.
fn moved<F: Fn(&Path) -> Result<()>>(
    set_up_repo: Result<RepoOpen>,
    change: F,
) -> Result<(
    SnapshotFile,
    SnapshotFile,
    SnapshotFile,
    Repository<impl IndexedIds>,
)> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let dir = tree()?;
    let first = backup(&repo, dir.path(), ParentOptions::default())?;
    assert_eq!(counts(&first), (5, 0));
    change(dir.path())?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let without = backup(&repo, dir.path(), parent(&first, false))?;
    let with = backup(&repo, dir.path(), parent(&first, true))?;
    let repo = repo.drop_index().to_indexed_ids()?;
    Ok((first, without, with, repo))
}

#[rstest]
#[case::renamed(|dir: &Path| Ok(fs::rename(dir.join("big"), dir.join("renamed"))?))]
#[case::moved_deeper(|dir: &Path| {
    fs::create_dir_all(dir.join("x/y"))?;
    Ok(fs::rename(dir.join("big"), dir.join("x/y/big2"))?)
})]
#[case::nested_dir_moved_up(|dir: &Path| Ok(fs::rename(dir.join("big/sub"), dir.join("sub"))?))]
fn files_in_a_moved_directory_are_not_read(
    set_up_repo: Result<RepoOpen>,
    #[case] change: fn(&Path) -> Result<()>,
) -> Result<()> {
    let (first, without, with, repo) = moved(set_up_repo, change)?;
    // without the option the moved files are new paths, and read
    assert_ne!(counts(&without).0, 0);
    // with it, every file matches
    assert_eq!(counts(&with), (0, 5));
    // and has its own content
    let mut first: Vec<_> = content(&repo, &first)?.into_values().collect();
    let mut with: Vec<_> = content(&repo, &with)?.into_values().collect();
    first.sort();
    with.sort();
    assert_eq!(with, first);
    Ok(())
}

#[rstest]
fn a_changed_file_in_a_moved_directory_is_read(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let (_, _, with, _) = moved(set_up_repo, |dir| {
        fs::rename(dir.join("big"), dir.join("renamed"))?;
        // other bytes, same size, same mtime: only the ctime shows it
        write(dir, "renamed/b", b'B')
    })?;
    assert_eq!(counts(&with), (1, 4));
    Ok(())
}

#[rstest]
fn a_new_directory_at_the_old_name_is_read(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let (first, _, with, repo) = moved(set_up_repo, |dir| {
        fs::rename(dir.join("big"), dir.join("renamed"))?;
        // the old name again, with files of the same names, sizes and mtimes
        // but other content
        for (name, byte) in [
            ("big/a", b'A'),
            ("big/b", b'B'),
            ("big/c", b'C'),
            ("big/sub/d", b'D'),
        ] {
            write(dir, name, byte)?;
        }
        Ok(())
    })?;
    // the new files are read, the moved ones match
    assert_eq!(counts(&with), (4, 5));
    let (first, with) = (content(&repo, &first)?, content(&repo, &with)?);
    assert_eq!(with["renamed/a"], first["big/a"]);
    assert_ne!(with["big/a"], first["big/a"]);
    Ok(())
}

#[rstest]
fn swapped_directories_keep_their_own_content(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let dir = tempfile::tempdir()?;
    // two directories with files of the same names, sizes and mtimes, other
    // content
    for (name, byte) in [("p/f", b'p'), ("p/g", b'P'), ("q/f", b'q'), ("q/g", b'Q')] {
        write(dir.path(), name, byte)?;
    }
    let first = backup(&repo, dir.path(), ParentOptions::default())?;
    // swap them
    fs::rename(dir.path().join("p"), dir.path().join("t"))?;
    fs::rename(dir.path().join("q"), dir.path().join("p"))?;
    fs::rename(dir.path().join("t"), dir.path().join("q"))?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let with = backup(&repo, dir.path(), parent(&first, true))?;
    // nothing is read: each file is found by its directory's inode
    assert_eq!(counts(&with), (0, 4));
    let repo = repo.drop_index().to_indexed_ids()?;
    let (first, with) = (content(&repo, &first)?, content(&repo, &with)?);
    assert_eq!(with["p/f"], first["q/f"]);
    assert_eq!(with["q/f"], first["p/f"]);
    assert_eq!(with["p/g"], first["q/g"]);
    assert_eq!(with["q/g"], first["p/g"]);
    Ok(())
}

#[rstest]
fn a_renamed_file_is_read(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // a rename gives the file itself a new ctime: it is not found
    let (_, _, with, _) = moved(set_up_repo, |dir| {
        Ok(fs::rename(dir.join("other"), dir.join("big/other"))?)
    })?;
    assert_eq!(counts(&with), (1, 4));
    Ok(())
}

/// One entry of a [`Synth`] source: a directory or a file with exactly this
/// metadata (all files have 4096 bytes and mtime 500).
#[derive(Clone, Copy, Debug)]
struct Entry {
    path: &'static str,
    dir: bool,
    inode: u64,
    ctime: Option<i64>,
    byte: u8,
}

fn dir(path: &'static str, inode: u64) -> Entry {
    Entry {
        path,
        dir: true,
        inode,
        ctime: Some(100),
        byte: 0,
    }
}

fn file(path: &'static str, inode: u64, ctime: Option<i64>, byte: u8) -> Entry {
    Entry {
        path,
        dir: false,
        inode,
        ctime,
        byte,
    }
}

/// A source of exactly these entries (in tree order), recording the files
/// opened.
struct Synth {
    entries: Vec<Entry>,
    opened: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
}

struct SynthOpen(Entry, std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>);

impl rustic_core::ReadSourceOpen for SynthOpen {
    type Reader = std::io::Cursor<Vec<u8>>;
    fn open(self) -> rustic_core::RusticResult<Self::Reader> {
        self.1.lock().unwrap().push(self.0.path);
        Ok(std::io::Cursor::new(vec![self.0.byte; 4096]))
    }
}

impl rustic_core::ReadSource for Synth {
    type Open = SynthOpen;
    type Iter =
        std::vec::IntoIter<rustic_core::RusticResult<rustic_core::ReadSourceEntry<SynthOpen>>>;

    fn size(&self) -> rustic_core::RusticResult<Option<u64>> {
        Ok(None)
    }

    fn entries(&self) -> Self::Iter {
        use rustic_core::repofile::{Metadata, Node, NodeType};
        self.entries
            .iter()
            .map(|entry| {
                let path = PathBuf::from("test").join(entry.path);
                let meta = Metadata {
                    size: if entry.dir { 0 } else { 4096 },
                    inode: entry.inode,
                    mtime: Some(jiff::Timestamp::from_second(500).unwrap()),
                    ctime: entry
                        .ctime
                        .map(|c| jiff::Timestamp::from_second(c).unwrap()),
                    ..Metadata::default()
                };
                let name = path.file_name().unwrap().to_owned();
                let node_type = if entry.dir {
                    NodeType::Dir
                } else {
                    NodeType::File
                };
                Ok(rustic_core::ReadSourceEntry {
                    node: Node::new_node(&name, node_type, meta),
                    path,
                    open: (!entry.dir).then(|| SynthOpen(*entry, self.opened.clone())),
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn strict_entries(&self) -> Self::Iter {
        self.entries()
    }
}

/// Archive the entries with these parent options; return the files opened
/// and the snapshot.
fn synth<S: IndexedIds>(
    repo: &Repository<S>,
    entries: Vec<Entry>,
    parent_opts: ParentOptions,
) -> Result<(Vec<&'static str>, SnapshotFile)> {
    let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let source = Synth {
        entries,
        opened: opened.clone(),
    };
    let snap = repo.archive(
        // the caller's assertion that this source stays within one filesystem
        &BackupOptions::default()
            .no_scan(true)
            .ignore_filter_opts(LocalSourceFilterOptions::default().one_file_system(true))
            .parent_opts(parent_opts),
        &source,
        SnapshotFile::default(),
        &[PathBuf::from("test")],
    )?;
    let mut opened = opened.lock().unwrap().clone();
    opened.sort_unstable();
    Ok((opened, snap))
}

/// Archive `first`, then `second` with `detect_moves` and these options on
/// top; return the files opened in the second run.
fn synth_moved(
    set_up_repo: Result<RepoOpen>,
    first: Vec<Entry>,
    second: Vec<Entry>,
    opts: fn(ParentOptions) -> ParentOptions,
) -> Result<Vec<&'static str>> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let (_, snap) = synth(&repo, first, ParentOptions::default())?;
    let repo = repo.drop_index().to_indexed_ids()?;
    Ok(synth(&repo, second, opts(parent(&snap, true)))?.0)
}

fn no_options(opts: ParentOptions) -> ParentOptions {
    opts
}

fn ignore_both(opts: ParentOptions) -> ParentOptions {
    opts.ignore_ctime(true).ignore_inode(true)
}

#[rstest]
#[case::default(no_options)]
#[case::ignore_ctime_and_inode(ignore_both)]
fn an_identical_file_is_found_in_a_moved_directory(
    set_up_repo: Result<RepoOpen>,
    #[case] opts: fn(ParentOptions) -> ParentOptions,
) -> Result<()> {
    // shows that the synthetic move is detected at all
    let opened = synth_moved(
        set_up_repo,
        vec![dir("old", 50), file("old/f", 1, Some(1000), b'a')],
        vec![dir("new", 50), file("new/f", 1, Some(1000), b'a')],
        opts,
    )?;
    assert!(opened.is_empty(), "opened {opened:?}");
    Ok(())
}

#[rstest]
// the directory's inode was reused for another directory, whose same-named
// file has the same size and mtime but is another file: --ignore-ctime and
// --ignore-inode don't relax the identity of a moved candidate
#[case::look_alike_ignoring_ctime_and_inode(file("new/f", 2, Some(2000), b'b'), ignore_both)]
// another subvolume reuses inode numbers: same inode, other ctime
#[case::same_inode_other_ctime(file("new/f", 1, Some(1500), b'b'), no_options)]
#[case::same_inode_other_ctime_ignoring_ctime(file("new/f", 1, Some(1500), b'b'), ignore_both)]
fn a_look_alike_in_a_moved_directory_is_read(
    set_up_repo: Result<RepoOpen>,
    #[case] new: Entry,
    #[case] opts: fn(ParentOptions) -> ParentOptions,
) -> Result<()> {
    let opened = synth_moved(
        set_up_repo,
        vec![dir("old", 50), file("old/f", 1, Some(1000), b'a')],
        vec![dir("new", 50), new],
        opts,
    )?;
    assert_eq!(opened, ["new/f"]);
    Ok(())
}

#[rstest]
// inode 0 is not identity, nor is a missing ctime (at its own path today's
// rule accepts both)
#[case::inode_zero(0, Some(1000))]
#[case::ctime_missing(1, None)]
fn a_moved_file_without_full_identity_is_read(
    set_up_repo: Result<RepoOpen>,
    #[case] inode: u64,
    #[case] ctime: Option<i64>,
) -> Result<()> {
    let opened = synth_moved(
        set_up_repo,
        vec![dir("old", 50), file("old/f", inode, ctime, b'a')],
        vec![dir("new", 50), file("new/f", inode, ctime, b'a')],
        no_options,
    )?;
    assert_eq!(opened, ["new/f"]);
    Ok(())
}

#[rstest]
#[case::default(no_options)]
#[case::ignore_ctime_and_inode(ignore_both)]
fn with_two_parents_only_the_identical_candidate_matches(
    set_up_repo: Result<RepoOpen>,
    #[case] opts: fn(ParentOptions) -> ParentOptions,
) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    // parent A, listed first: a look-alike under the same directory inode
    let (_, a) = synth(
        &repo,
        vec![dir("x", 50), file("x/f", 1, Some(1000), b'a')],
        ParentOptions::default(),
    )?;
    // parent B: the file itself
    let (_, b) = synth(
        &repo,
        vec![dir("y", 50), file("y/f", 1, Some(1001), b'b')],
        ParentOptions::default(),
    )?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let (opened, z) = synth(
        &repo,
        vec![dir("z", 50), file("z/f", 1, Some(1001), b'b')],
        opts(
            ParentOptions::default()
                .parents(vec![a.id.to_string(), b.id.to_string()])
                .detect_moves(true),
        ),
    )?;
    assert!(opened.is_empty(), "opened {opened:?}");
    let repo = repo.drop_index().to_indexed_ids()?;
    let content_of = |snap: &SnapshotFile, path: &str| -> Result<_> {
        Ok(repo
            .node_from_snapshot_and_path(snap, &format!("test/{path}"))?
            .content)
    };
    assert_eq!(content_of(&z, "z/f")?, content_of(&b, "y/f")?);
    assert_ne!(content_of(&z, "z/f")?, content_of(&a, "x/f")?);
    Ok(())
}

/// The parent of the cap tests: three directories, each with a file. Its
/// index needs 8 entries: 2 trees visited (the root and "test") and, for
/// each directory, 1 indexed and 1 visited.
fn three_dirs() -> Vec<Entry> {
    vec![
        dir("a", 10),
        file("a/f", 1, Some(1000), b'a'),
        dir("b", 11),
        file("b/f", 2, Some(1000), b'b'),
        dir("c", 12),
        file("c/f", 3, Some(1000), b'c'),
    ]
}

/// "a" renamed to "a2", the first directory indexed.
fn three_dirs_a_moved() -> Vec<Entry> {
    vec![
        dir("a2", 10),
        file("a2/f", 1, Some(1000), b'a'),
        dir("b", 11),
        file("b/f", 2, Some(1000), b'b'),
        dir("c", 12),
        file("c/f", 3, Some(1000), b'c'),
    ]
}

#[rstest]
#[case::at_the_cap(8, false)]
// one over: detection is off, and the partial index (which has "a") is not
// used: "a2/f" is read
#[case::one_over_the_cap(7, true)]
#[case::far_over_the_cap(1, true)]
fn the_index_is_capped(
    set_up_repo: Result<RepoOpen>,
    #[case] max_entries: usize,
    #[case] read: bool,
) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let (_, first) = synth(&repo, three_dirs(), ParentOptions::default())?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let (opened, _) = synth(
        &repo,
        three_dirs_a_moved(),
        parent(&first, true).detect_moves_max_entries(max_entries),
    )?;
    // over the cap the backup still runs, matching files at their own path
    let expected: &[&str] = if read { &["a2/f"] } else { &[] };
    assert_eq!(opened, expected);
    Ok(())
}

#[rstest]
fn without_one_file_system_moves_are_not_detected(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let dir = tree()?;
    let first = backup(&repo, dir.path(), ParentOptions::default())?;
    fs::rename(dir.path().join("big"), dir.path().join("renamed"))?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let snap = repo.backup(
        &BackupOptions::default()
            .no_scan(true)
            .as_path(PathBuf::from("/home"))
            .parent_opts(parent(&first, true)),
        &PathList::from_iter([dir.path()]),
        SnapshotFile::default(),
    )?;
    // ignored: the moved files are read
    assert_eq!(counts(&snap), (4, 1));
    Ok(())
}

#[rstest]
fn a_parent_of_other_paths_gives_no_candidates(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    // the same files, backed up as another path
    let (_, other) = synth(
        &repo,
        vec![dir("old", 50), file("old/f", 1, Some(1000), b'a')],
        ParentOptions::default(),
    )?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let source = Synth {
        entries: vec![dir("new", 50), file("new/f", 1, Some(1000), b'a')],
        opened: opened.clone(),
    };
    // archive() records the --as-path as the snapshot's path
    _ = repo.archive(
        &BackupOptions::default()
            .no_scan(true)
            .as_path(PathBuf::from("elsewhere"))
            .ignore_filter_opts(LocalSourceFilterOptions::default().one_file_system(true))
            .parent_opts(parent(&other, true)),
        &source,
        SnapshotFile::default(),
        &[PathBuf::from("test")],
    )?;
    assert_eq!(*opened.lock().unwrap(), ["new/f"]);
    Ok(())
}

#[rstest]
fn a_custom_source_without_one_file_system_gets_no_candidates(
    set_up_repo: Result<RepoOpen>,
) -> Result<()> {
    // Repository::archive, the entry point the rustic CLI uses
    let repo = set_up_repo?.to_indexed_ids()?;
    let (_, first) = synth(
        &repo,
        vec![dir("old", 50), file("old/f", 1, Some(1000), b'a')],
        ParentOptions::default(),
    )?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let source = Synth {
        entries: vec![dir("new", 50), file("new/f", 1, Some(1000), b'a')],
        opened: opened.clone(),
    };
    let snap = repo.archive(
        &BackupOptions::default()
            .no_scan(true)
            .parent_opts(parent(&first, true)),
        &source,
        SnapshotFile::default(),
        &[PathBuf::from("test")],
    )?;
    assert_eq!(*opened.lock().unwrap(), ["new/f"]);
    assert!(!snap.single_filesystem);
    Ok(())
}

/// A real backup of `dir`, with `--one-file-system` or not.
fn backup_x<S: IndexedIds>(
    repo: &Repository<S>,
    dir: &Path,
    one_file_system: bool,
    parent_opts: ParentOptions,
) -> Result<SnapshotFile> {
    Ok(repo.backup(
        &BackupOptions::default()
            .no_scan(true)
            .as_path(PathBuf::from("/home"))
            .ignore_filter_opts(
                LocalSourceFilterOptions::default().one_file_system(one_file_system),
            )
            .parent_opts(parent_opts),
        &PathList::from_iter([dir]),
        SnapshotFile::default(),
    )?)
}

#[rstest]
fn the_snapshot_records_a_single_filesystem_source(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let dir = tree()?;
    let with = backup_x(&repo, dir.path(), true, ParentOptions::default())?;
    let without = backup_x(&repo, dir.path(), false, ParentOptions::default())?;
    // as stored in the repository
    let stored = |snap: &SnapshotFile| -> Result<bool> {
        Ok(repo
            .get_snapshot_from_str(&snap.id.to_string(), |_| true)?
            .single_filesystem)
    };
    assert!(stored(&with)?);
    assert!(!stored(&without)?);
    // two backup paths are not a single filesystem source either
    let other = tree()?;
    let two = repo.backup(
        &BackupOptions::default()
            .no_scan(true)
            .ignore_filter_opts(LocalSourceFilterOptions::default().one_file_system(true)),
        &PathList::from_iter([dir.path(), other.path()]),
        SnapshotFile::default(),
    )?;
    assert!(!stored(&two)?);
    Ok(())
}

#[rstest]
// a parent made without --one-file-system (or by an older version: the
// marker is absent) gives no candidates
#[case::unmarked_parent(&[false], 4)]
// one unmarked parent among marked ones turns detection off
#[case::mixed_parents(&[true, false], 4)]
#[case::marked_parent(&[true], 0)]
fn every_parent_must_be_a_single_filesystem_source(
    set_up_repo: Result<RepoOpen>,
    #[case] marked: &[bool],
    #[case] read: u64,
) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let dir = tree()?;
    let parents: Vec<String> = marked
        .iter()
        .map(|x| {
            Ok(backup_x(&repo, dir.path(), *x, ParentOptions::default())?
                .id
                .to_string())
        })
        .collect::<Result<_>>()?;
    fs::rename(dir.path().join("big"), dir.path().join("renamed"))?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let snap = backup_x(
        &repo,
        dir.path(),
        true,
        ParentOptions::default().parents(parents).detect_moves(true),
    )?;
    assert_eq!(counts(&snap).0, read);
    Ok(())
}
