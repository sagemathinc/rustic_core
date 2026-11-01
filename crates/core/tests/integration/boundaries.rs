//! A node on another filesystem than its backup root (a mount point or
//! nested subvolume, and everything below it) gets nothing from the parent
//! snapshot: it is read in full.

use std::path::PathBuf;

use anyhow::Result;
use pretty_assertions::assert_eq;
use rstest::rstest;

use rustic_core::{
    BackupOptions, IndexedIds, LocalSourceFilterOptions, ParentOptions, ReadSource,
    ReadSourceEntry, ReadSourceOpen, Repository, RusticResult,
    repofile::{Metadata, Node, NodeType, SnapshotFile},
};

use super::{RepoOpen, set_up_repo};

/// One entry of a [`Source`]: a directory or a file (4096 bytes, mtime 500,
/// ctime 1000) with this inode, and whether it is on another filesystem.
#[derive(Clone, Copy, Debug)]
struct Entry {
    path: &'static str,
    dir: bool,
    inode: u64,
    other_fs: bool,
}

fn dir(path: &'static str, inode: u64, other_fs: bool) -> Entry {
    Entry {
        path,
        dir: true,
        inode,
        other_fs,
    }
}

fn file(path: &'static str, inode: u64, other_fs: bool) -> Entry {
    Entry {
        path,
        dir: false,
        inode,
        other_fs,
    }
}

struct Source {
    entries: Vec<Entry>,
    opened: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
}

struct Open(
    &'static str,
    std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
);

impl ReadSourceOpen for Open {
    type Reader = std::io::Cursor<Vec<u8>>;
    fn open(self) -> RusticResult<Self::Reader> {
        self.1.lock().unwrap().push(self.0);
        Ok(std::io::Cursor::new(vec![b'x'; 4096]))
    }
}

impl ReadSource for Source {
    type Open = Open;
    type Iter = std::vec::IntoIter<RusticResult<ReadSourceEntry<Open>>>;

    fn size(&self) -> RusticResult<Option<u64>> {
        Ok(None)
    }

    fn entries(&self) -> Self::Iter {
        self.entries
            .iter()
            .map(|entry| {
                let path = PathBuf::from("test").join(entry.path);
                let meta = Metadata {
                    size: if entry.dir { 0 } else { 4096 },
                    inode: entry.inode,
                    mtime: Some(jiff::Timestamp::from_second(500).unwrap()),
                    ctime: Some(jiff::Timestamp::from_second(1000).unwrap()),
                    ..Metadata::default()
                };
                let node_type = if entry.dir {
                    NodeType::Dir
                } else {
                    NodeType::File
                };
                let mut node = Node::new_node(path.file_name().unwrap(), node_type, meta);
                node.set_other_filesystem(entry.other_fs);
                Ok(ReadSourceEntry {
                    node,
                    path,
                    open: (!entry.dir).then(|| Open(entry.path, self.opened.clone())),
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn strict_entries(&self) -> Self::Iter {
        self.entries()
    }
}

/// Archive the entries (asserting a single-filesystem source) with these
/// parent options; return the files opened and the snapshot.
fn archive<S: IndexedIds>(
    repo: &Repository<S>,
    entries: Vec<Entry>,
    parent_opts: ParentOptions,
) -> Result<(Vec<&'static str>, SnapshotFile)> {
    let opened = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let source = Source {
        entries,
        opened: opened.clone(),
    };
    let snap = repo.archive(
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

/// Back up `first` (all on the root filesystem), then `second` with the
/// first as parent and `detect_moves`; return the files opened the second
/// time.
fn second_run(
    set_up_repo: Result<RepoOpen>,
    first: Vec<Entry>,
    second: Vec<Entry>,
    detect_moves: bool,
) -> Result<Vec<&'static str>> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let (_, snap) = archive(&repo, first, ParentOptions::default())?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let parent = ParentOptions::default()
        .parents(vec![snap.id.to_string()])
        .detect_moves(detect_moves);
    Ok(archive(&repo, second, parent)?.0)
}

fn tree(other_fs: bool) -> Vec<Entry> {
    vec![
        dir("d", 10, other_fs),
        file("d/f", 11, other_fs),
        dir("e", 20, false),
        file("e/g", 21, false),
    ]
}

#[rstest]
fn on_one_filesystem_unchanged_files_match(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // the control: the same tree without a boundary reads nothing
    let opened = second_run(set_up_repo, tree(false), tree(false), false)?;
    assert!(opened.is_empty(), "opened {opened:?}");
    Ok(())
}

#[rstest]
fn a_directory_on_another_filesystem_is_read(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // "d" is now on another filesystem; "e", after it, still matches
    let opened = second_run(set_up_repo, tree(false), tree(true), false)?;
    assert_eq!(opened, ["d/f"]);
    Ok(())
}

#[rstest]
fn below_a_boundary_nothing_matches(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // "d" is on another filesystem; "d/e" (and its file) are back on the
    // root's device, but below the boundary all the same
    let first = vec![
        dir("d", 10, false),
        dir("d/e", 12, false),
        file("d/e/f", 13, false),
    ];
    let second = vec![
        dir("d", 10, true),
        dir("d/e", 12, false),
        file("d/e/f", 13, false),
    ];
    let opened = second_run(set_up_repo, first, second, false)?;
    assert_eq!(opened, ["d/e/f"]);
    Ok(())
}

#[rstest]
fn a_file_on_another_filesystem_is_read(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let opened = second_run(
        set_up_repo,
        vec![file("f", 1, false), file("g", 2, false)],
        vec![file("f", 1, true), file("g", 2, false)],
        false,
    )?;
    assert_eq!(opened, ["f"]);
    Ok(())
}

#[rstest]
#[case::moved_directory_on_the_other_filesystem(true)]
#[case::moved_directory_back_on_the_root_filesystem(false)]
fn below_a_boundary_moves_are_not_detected(
    set_up_repo: Result<RepoOpen>,
    #[case] new_other_fs: bool,
) -> Result<()> {
    // the parent's "old" (inode 50) has an identical file; now a directory of
    // inode 50 appears below a directory on another filesystem
    let opened = second_run(
        set_up_repo,
        vec![dir("old", 50, false), file("old/f", 51, false)],
        vec![
            dir("mnt", 60, true),
            dir("mnt/new", 50, new_other_fs),
            file("mnt/new/f", 51, new_other_fs),
        ],
        true,
    )?;
    assert_eq!(opened, ["mnt/new/f"]);
    Ok(())
}

#[rstest]
fn on_one_filesystem_moves_are_detected(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // the control for the case above
    let opened = second_run(
        set_up_repo,
        vec![dir("old", 50, false), file("old/f", 51, false)],
        vec![
            dir("mnt", 60, false),
            dir("mnt/new", 50, false),
            file("mnt/new/f", 51, false),
        ],
        true,
    )?;
    assert!(opened.is_empty(), "opened {opened:?}");
    Ok(())
}

#[rstest]
fn the_mark_is_not_stored(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // the same tree with and without the mark gives the same tree blobs
    let repo = set_up_repo?.to_indexed_ids()?;
    let (_, unmarked) = archive(&repo, tree(false), ParentOptions::default())?;
    let (_, marked) = archive(&repo, tree(true), ParentOptions::default().force(true))?;
    assert_eq!(marked.tree, unmarked.tree);
    Ok(())
}
