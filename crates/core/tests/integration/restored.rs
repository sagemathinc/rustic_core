//! `ParentOptions::restored_before`: the first backup of a tree restored from
//! its parent snapshot doesn't read files the restore wrote, and reads every
//! file changed after it.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use rstest::rstest;

use rustic_core::{
    BackupOptions, DataId, IndexedIds, ParentOptions, ReadSource, ReadSourceEntry, ReadSourceOpen,
    Repository, RusticResult,
    repofile::{Metadata, Node, NodeType, SnapshotFile},
};

use super::{RepoOpen, set_up_repo};

/// One file of a [`FileSource`]: its name, metadata and content byte.
#[derive(Clone, Debug)]
struct File {
    name: &'static str,
    inode: u64,
    links: u64,
    device_id: u64,
    size: u64,
    mtime: i64,
    ctime: i64,
    byte: u8,
}

impl File {
    /// A file as first backed up: inode `inode`, ctime 1000.
    fn new(name: &'static str, inode: u64, byte: u8) -> Self {
        Self {
            name,
            inode,
            links: 1,
            device_id: 0,
            size: 4096,
            mtime: 500,
            ctime: 1000,
            byte,
        }
    }

    /// The same file as a restore wrote it: a new inode, and the ctime of the
    /// restore (2000); size, mtime and content unchanged.
    fn restored(&self) -> Self {
        Self {
            inode: self.inode + 100,
            ctime: 2000,
            ..self.clone()
        }
    }
}

/// A source of files with the given metadata, recording which are opened.
struct FileSource {
    files: Vec<File>,
    opened: Arc<Mutex<Vec<&'static str>>>,
}

struct FileOpen {
    file: File,
    opened: Arc<Mutex<Vec<&'static str>>>,
}

impl ReadSourceOpen for FileOpen {
    type Reader = std::io::Cursor<Vec<u8>>;
    fn open(self) -> RusticResult<Self::Reader> {
        self.opened.lock().unwrap().push(self.file.name);
        Ok(std::io::Cursor::new(vec![
            self.file.byte;
            usize::try_from(self.file.size)
                .unwrap()
        ]))
    }
}

impl ReadSource for FileSource {
    type Open = FileOpen;
    type Iter = std::vec::IntoIter<RusticResult<ReadSourceEntry<FileOpen>>>;

    fn size(&self) -> RusticResult<Option<u64>> {
        Ok(None)
    }

    fn entries(&self) -> Self::Iter {
        let mut files = self.files.clone();
        files.sort_by_key(|file| file.name);
        files
            .into_iter()
            .map(|file| {
                Ok(ReadSourceEntry {
                    path: PathBuf::from("test").join(file.name),
                    node: Node::new_node(
                        file.name.as_ref(),
                        NodeType::File,
                        Metadata {
                            size: file.size,
                            links: file.links,
                            inode: file.inode,
                            device_id: file.device_id,
                            mtime: Some(Timestamp::from_second(file.mtime).unwrap()),
                            ctime: Some(Timestamp::from_second(file.ctime).unwrap()),
                            ..Metadata::default()
                        },
                    ),
                    open: Some(FileOpen {
                        file,
                        opened: self.opened.clone(),
                    }),
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn strict_entries(&self) -> Self::Iter {
        self.entries()
    }
}

/// Archive `files` with these parent options; return the names opened (in
/// sorted order) and the snapshot.
fn archive<S: IndexedIds>(
    repo: &Repository<S>,
    files: Vec<File>,
    parent_opts: ParentOptions,
    reuse_hardlinks: bool,
) -> Result<(Vec<&'static str>, SnapshotFile)> {
    let opened = Arc::new(Mutex::new(Vec::new()));
    let source = FileSource {
        files,
        opened: opened.clone(),
    };
    let snapshot = repo.archive(
        &BackupOptions::default()
            .no_scan(true)
            .reuse_hardlinks(reuse_hardlinks)
            .parent_opts(parent_opts),
        &source,
        SnapshotFile::default(),
        &[PathBuf::from("test")],
    )?;
    let mut opened = opened.lock().unwrap().clone();
    opened.sort_unstable();
    Ok((opened, snapshot))
}

fn parent(snap: &SnapshotFile) -> ParentOptions {
    ParentOptions::default().parents(vec![snap.id.to_string()])
}

fn at(second: i64) -> Timestamp {
    Timestamp::from_second(second).unwrap()
}

/// The content ids of each file of a snapshot, by name.
fn content<S: IndexedIds>(
    repo: &Repository<S>,
    snap: &SnapshotFile,
) -> Result<Vec<(String, Option<Vec<DataId>>)>> {
    let tree = repo.get_tree(&snap.tree)?;
    let dir = tree
        .nodes
        .iter()
        .find(|n| n.name() == std::ffi::OsStr::new("test"))
        .unwrap();
    let tree = repo.get_tree(&dir.subtree.unwrap())?;
    Ok(tree
        .nodes
        .into_iter()
        .map(|n| (n.name().to_string_lossy().to_string(), n.content))
        .collect())
}

fn original() -> Vec<File> {
    vec![
        File::new("a", 1, b'a'),
        File::new("b", 2, b'b'),
        File::new("c", 3, b'c'),
    ]
}

/// Back up [`original`] once, and reload the index so its blobs are known.
fn first_backup(
    set_up_repo: Result<RepoOpen>,
) -> Result<(Repository<impl IndexedIds>, SnapshotFile)> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let (opened, first) = archive(&repo, original(), ParentOptions::default(), false)?;
    assert_eq!(opened, ["a", "b", "c"]);
    let repo = repo.drop_index().to_indexed_ids()?;
    Ok((repo, first))
}

#[rstest]
fn a_restored_tree_is_read_again_without_the_option(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let (repo, first) = first_backup(set_up_repo)?;
    let restored = original().iter().map(File::restored).collect();
    // new inodes and ctimes: today every file is read again (this is what the
    // option avoids, and shows the test detects reads)
    let (opened, _) = archive(&repo, restored, parent(&first), false)?;
    assert_eq!(opened, ["a", "b", "c"]);
    Ok(())
}

#[rstest]
fn files_the_restore_wrote_match_their_parent(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let (repo, first) = first_backup(set_up_repo)?;
    let restored = original().iter().map(File::restored).collect();
    let (opened, second) = archive(
        &repo,
        restored,
        parent(&first).restored_before(at(3000)),
        false,
    )?;
    assert!(opened.is_empty(), "opened {opened:?}");
    let repo = repo.drop_index().to_indexed_ids()?;
    assert_eq!(content(&repo, &second)?, content(&repo, &first)?);
    Ok(())
}

#[rstest]
#[case::changed_after_the_restore(3001)]
// strictly before: a change at the restore time itself is read
#[case::changed_at_the_restore_time(3000)]
fn a_file_changed_after_the_restore_is_read(
    set_up_repo: Result<RepoOpen>,
    #[case] ctime: i64,
) -> Result<()> {
    let (repo, first) = first_backup(set_up_repo)?;
    let mut files: Vec<File> = original().iter().map(File::restored).collect();
    // "b" rewritten after the restore with other bytes of the same size, and
    // its mtime set back: only the ctime shows the change
    files[1].ctime = ctime;
    files[1].byte = b'B';
    let (opened, second) = archive(
        &repo,
        files,
        parent(&first).restored_before(at(3000)),
        false,
    )?;
    assert_eq!(opened, ["b"]);
    let repo = repo.drop_index().to_indexed_ids()?;
    let (first, second) = (content(&repo, &first)?, content(&repo, &second)?);
    assert_eq!(second[0], first[0]);
    assert_ne!(second[1], first[1]);
    assert_eq!(second[2], first[2]);
    Ok(())
}

#[rstest]
#[case::size(|file: &mut File| file.size = 8192)]
#[case::mtime(|file: &mut File| file.mtime = 501)]
fn a_restored_file_still_compares_size_and_mtime(
    set_up_repo: Result<RepoOpen>,
    #[case] change: fn(&mut File),
) -> Result<()> {
    let (repo, first) = first_backup(set_up_repo)?;
    let mut files: Vec<File> = original().iter().map(File::restored).collect();
    change(&mut files[2]);
    let (opened, _) = archive(
        &repo,
        files,
        parent(&first).restored_before(at(3000)),
        false,
    )?;
    assert_eq!(opened, ["c"]);
    Ok(())
}

#[rstest]
#[case::no_explicit_parent(|_first: &SnapshotFile| ParentOptions::default())]
#[case::two_parents(|first: &SnapshotFile| {
    ParentOptions::default().parents(vec![first.id.to_string(), first.id.to_string()])
})]
#[case::force(|first: &SnapshotFile| ParentOptions::default().parents(vec![first.id.to_string()]).force(true))]
fn the_option_needs_exactly_one_explicit_parent(
    set_up_repo: Result<RepoOpen>,
    #[case] parent_opts: fn(&SnapshotFile) -> ParentOptions,
) -> Result<()> {
    let (repo, first) = first_backup(set_up_repo)?;
    let restored = original().iter().map(File::restored).collect();
    let (opened, _) = archive(
        &repo,
        restored,
        parent_opts(&first).restored_before(at(3000)),
        false,
    )?;
    // ignored: every file is read, as without it
    assert_eq!(opened, ["a", "b", "c"]);
    Ok(())
}

#[rstest]
fn a_restored_match_vouches_for_no_other_path(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    // "a" has two hard links, recorded with its device
    let linked = File {
        links: 2,
        device_id: 7,
        ..File::new("a", 1, b'a')
    };
    let (opened, first) = archive(&repo, vec![linked.clone()], ParentOptions::default(), true)?;
    assert_eq!(opened, ["a"]);
    let repo = repo.drop_index().to_indexed_ids()?;
    // restored: "a" matches its parent under the restore rule, and "z", a new
    // path of the same (new) inode, must not reuse that match: the restore
    // rule proves the content at a path, not an inode's identity
    let a = linked.restored();
    let z = File { name: "z", ..a };
    let (opened, _) = archive(
        &repo,
        vec![a, z],
        parent(&first).restored_before(at(3000)),
        true,
    )?;
    assert_eq!(opened, ["z"]);
    Ok(())
}

/// The real thing on the local filesystem: back up a tree, restore it into an
/// empty directory, and back up the restored tree with the restore's time.
#[cfg(target_os = "linux")]
#[rstest]
fn a_real_restore_is_not_read_again(set_up_repo: Result<RepoOpen>) -> Result<()> {
    use std::{fs, time::Duration};

    use rustic_core::{LocalDestination, LsOptions, PathList, RestoreOptions};

    let repo = set_up_repo?.to_indexed_ids()?;
    let source = tempfile::tempdir()?;
    for (name, byte) in [("one", b'1'), ("two", b'2'), ("three", b'3')] {
        fs::write(source.path().join(name), vec![byte; 100_000])?;
    }
    fs::create_dir(source.path().join("sub"))?;
    fs::write(source.path().join("sub/four"), vec![b'4'; 100_000])?;

    let backup = |repo: &Repository<_>, dir: &std::path::Path, parent_opts| {
        repo.backup(
            &BackupOptions::default()
                .no_scan(true)
                .as_path(PathBuf::from("/home"))
                .parent_opts(parent_opts),
            &PathList::from_iter([dir]),
            SnapshotFile::default(),
        )
    };
    let first = backup(&repo, source.path(), ParentOptions::default())?;

    // restore it into an empty directory, as a project move does
    let repo = repo.to_indexed()?;
    let node = repo.node_from_snapshot_and_path(&first, "home")?;
    let ls = repo.ls(&node, &LsOptions::default().recursive(true))?;
    let target = tempfile::tempdir()?;
    let dest = LocalDestination::new(target.path().to_str().unwrap(), true, false)?;
    let opts = RestoreOptions::default();
    let plan = repo.prepare_restore(&opts, ls.clone(), &dest, false)?;
    repo.restore(plan, &opts, ls, &dest)?;
    // the restore's end, taken after it, a little later than any ctime the
    // restore set (coarse kernel timestamps)
    std::thread::sleep(Duration::from_millis(50));
    let restored_before = Timestamp::now();
    std::thread::sleep(Duration::from_millis(50));

    // after the restore: "two" rewritten with other bytes of the same size,
    // its mtime set back
    let two = target.path().join("two");
    let mtime = fs::metadata(&two)?.modified()?;
    fs::write(&two, vec![b'X'; 100_000])?;
    fs::File::options()
        .write(true)
        .open(&two)?
        .set_modified(mtime)?;

    let repo = repo.drop_index().to_indexed_ids()?;
    let without = backup(&repo, target.path(), parent(&first))?;
    let with = backup(
        &repo,
        target.path(),
        parent(&first).restored_before(restored_before),
    )?;
    let (without, with) = (without.summary.unwrap(), with.summary.unwrap());
    // without the option every file is read again
    assert_eq!((without.files_changed, without.files_unmodified), (4, 0));
    // with it only "two"
    assert_eq!((with.files_changed, with.files_unmodified), (1, 3));
    Ok(())
}
