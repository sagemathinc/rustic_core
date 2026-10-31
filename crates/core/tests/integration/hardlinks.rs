//! Adversaries for `BackupOptions::reuse_hardlinks`.
//!
//! The oracle is differential: every scenario is archived once with and once
//! without the option, and each path must get the same content both ways
//! whenever the paths of an inode really have the same bytes.  Reuse may only
//! save reads; it must never change what a path's snapshot entry says.

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use pretty_assertions::assert_eq;
use rstest::rstest;

use rustic_core::{
    BackupOptions, IndexedIds, IndexedIdsStatus, LsOptions, ParentOptions, ReadSource,
    ReadSourceEntry, ReadSourceOpen, Repository, RusticResult,
    repofile::{Metadata, Node, NodeType, SnapshotFile},
};

use super::{RepoOpen, set_up_repo};

/// One path of the synthetic source: the stat it reports and the bytes an
/// open returns.
#[derive(Clone, Debug)]
struct Path {
    name: &'static str,
    device: u64,
    inode: u64,
    links: u64,
    mtime: Option<i64>,
    ctime: Option<i64>,
    bytes: Vec<u8>,
    /// Sleep this long in open, so other paths reach the archiver meanwhile.
    delay_ms: u64,
}

fn path(name: &'static str, inode: u64, links: u64, bytes: &[u8]) -> Path {
    Path {
        name,
        device: 1,
        inode,
        links,
        mtime: Some(1_600_000_000),
        ctime: Some(1_600_000_000),
        bytes: bytes.to_vec(),
        delay_ms: 0,
    }
}

impl Path {
    fn ctime(mut self, ctime: i64) -> Self {
        self.ctime = Some(ctime);
        self
    }
    fn device(mut self, device: u64) -> Self {
        self.device = device;
        self
    }
    fn slow(mut self) -> Self {
        self.delay_ms = 300;
        self
    }
}

/// Paths of a synthetic directory `test`.  Opens are counted; the first open
/// of each inode in `fail_first` fails, whichever path makes it.
#[derive(Clone)]
struct Source {
    paths: Vec<Path>,
    fail_first: Vec<(u64, u64)>,
    failed: Arc<Mutex<HashSet<(u64, u64)>>>,
    opens: Arc<AtomicUsize>,
}

impl Source {
    fn new(mut paths: Vec<Path>) -> Self {
        paths.sort_by_key(|p| p.name);
        Self {
            paths,
            fail_first: Vec::new(),
            failed: Arc::default(),
            opens: Arc::default(),
        }
    }
}

struct Open {
    path: Path,
    /// The first open of this inode (whichever path it is) fails.
    fail_first: Option<(u64, u64)>,
    failed: Arc<Mutex<HashSet<(u64, u64)>>>,
    opens: Arc<AtomicUsize>,
}

impl ReadSourceOpen for Open {
    type Reader = std::io::Cursor<Vec<u8>>;
    fn open(self) -> RusticResult<Self::Reader> {
        _ = self.opens.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(self.path.delay_ms));
        if let Some(key) = self.fail_first
            && self.failed.lock().unwrap().insert(key)
        {
            return Err(rustic_core::RusticError::new(
                rustic_core::ErrorKind::InputOutput,
                "injected open failure",
            ));
        }
        Ok(std::io::Cursor::new(self.path.bytes))
    }
}

impl ReadSource for Source {
    type Open = Open;
    type Iter = std::vec::IntoIter<RusticResult<ReadSourceEntry<Open>>>;

    fn size(&self) -> RusticResult<Option<u64>> {
        Ok(None)
    }

    fn entries(&self) -> Self::Iter {
        self.paths
            .iter()
            .map(|p| {
                let key = (p.device, p.inode);
                Ok(ReadSourceEntry {
                    path: PathBuf::from("test").join(p.name),
                    node: Node::new_node(
                        p.name.as_ref(),
                        NodeType::File,
                        Metadata {
                            size: p.bytes.len() as u64,
                            links: p.links,
                            inode: p.inode,
                            device_id: p.device,
                            mtime: p.mtime.map(|t| jiff::Timestamp::from_second(t).unwrap()),
                            ctime: p.ctime.map(|t| jiff::Timestamp::from_second(t).unwrap()),
                            ..Metadata::default()
                        },
                    ),
                    open: Some(Open {
                        path: p.clone(),
                        fail_first: self.fail_first.contains(&key).then_some(key),
                        failed: self.failed.clone(),
                        opens: self.opens.clone(),
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

/// Path name -> content ids in the snapshot (paths that failed are absent).
type Contents = BTreeMap<String, Vec<rustic_core::DataId>>;

struct Run {
    snapshot: SnapshotFile,
    contents: Contents,
    opens: usize,
}

/// Archive `source` (with `parent`, if any); the repository comes back with
/// its index reloaded, so the next run sees this run's blobs.
fn archive<S: IndexedIds>(
    repo: Repository<S>,
    source: &Source,
    reuse: bool,
    parent: Option<&SnapshotFile>,
    parent_opts: ParentOptions,
) -> Result<(Repository<IndexedIdsStatus>, Run)> {
    let source = Source {
        failed: Arc::default(),
        opens: Arc::default(),
        ..source.clone()
    };
    let opens = source.opens.clone();
    let parents: Vec<String> = parent.iter().map(|snap| snap.id.to_string()).collect();
    let snapshot = repo.archive(
        &BackupOptions::default()
            .no_scan(true)
            .reuse_hardlinks(reuse)
            // no parent means none, not the latest matching snapshot
            .parent_opts(parent_opts.parents(parents).force(parent.is_none())),
        &source,
        SnapshotFile::default(),
        &[PathBuf::from("test")],
    )?;
    let repo = repo.drop_index().to_indexed_ids()?;
    let node = repo.node_from_snapshot_and_path(&snapshot, "test")?;
    let contents = repo
        .ls(&node, &LsOptions::default())?
        .map(|item| {
            let (path, node) = item.unwrap();
            (path.display().to_string(), node.content.unwrap_or_default())
        })
        .collect();
    Ok((
        repo,
        Run {
            snapshot,
            contents,
            opens: opens.load(Ordering::SeqCst),
        },
    ))
}

/// Archive each source in turn (each with the previous snapshot as parent),
/// with and without reuse; return the runs with reuse, after checking that
/// every run gave every path the same content both ways.
fn differential(repo: RepoOpen, runs: &[Source], parent_opts: &ParentOptions) -> Result<Vec<Run>> {
    let mut repo = repo.to_indexed_ids()?;
    let mut results = Vec::new();
    for reuse in [false, true] {
        let mut parent: Option<SnapshotFile> = None;
        let mut these = Vec::new();
        for source in runs {
            let (next, run) = archive(repo, source, reuse, parent.as_ref(), parent_opts.clone())?;
            repo = next;
            parent = Some(run.snapshot.clone());
            these.push(run);
        }
        results.push(these);
    }
    let with = results.pop().unwrap();
    let without = results.pop().unwrap();
    for (i, (a, b)) in without.iter().zip(&with).enumerate() {
        assert_eq!(
            a.contents, b.contents,
            "run {i}: content per path differs with --reuse-hardlinks"
        );
    }
    Ok(with)
}

fn id_of(run: &Run, name: &str) -> Vec<rustic_core::DataId> {
    run.contents
        .get(name)
        .cloned()
        .unwrap_or_else(|| panic!("{name} is not in the snapshot: {:?}", run.contents.keys()))
}

#[rstest]
fn owner_read_failure_gives_waiters_their_own_read(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // three paths of one inode; whichever path owns the read fails (slowly,
    // so the other two are waiting), and they must read the file themselves
    let mut source = Source::new(vec![
        path("a", 5, 3, b"inode five").slow(),
        path("b", 5, 3, b"inode five").slow(),
        path("c", 5, 3, b"inode five").slow(),
    ]);
    source.fail_first = vec![(1, 5)];
    let (repo, run) = archive(
        set_up_repo?.to_indexed_ids()?,
        &source,
        true,
        None,
        ParentOptions::default(),
    )?;
    assert_eq!(run.snapshot.summary.as_ref().unwrap().error_count, 1);
    assert_eq!(run.contents.len(), 2, "only the failed path is missing");
    let ids: Vec<_> = run.contents.values().collect();
    assert!(!ids[0].is_empty(), "a waiter got empty content");
    assert_eq!(ids[0], ids[1]);
    // and it is the real content: the same as a plain read of the bytes
    let (_, plain) = archive(
        repo,
        &Source::new(vec![path("x", 9, 1, b"inode five")]),
        false,
        None,
        ParentOptions::default(),
    )?;
    assert_eq!(*ids[0], id_of(&plain, "x"));
    Ok(())
}

#[rstest]
fn the_same_inode_number_on_two_devices_is_two_files(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // identical stat apart from the device, different bytes
    let runs = differential(
        set_up_repo?,
        &[Source::new(vec![
            path("a", 5, 2, b"device one"),
            path("b", 5, 2, b"device one"),
            path("c", 5, 2, b"device two").device(2),
            path("d", 5, 2, b"device two").device(2),
        ])],
        &ParentOptions::default(),
    )?;
    assert_ne!(id_of(&runs[0], "a"), id_of(&runs[0], "c"));
    assert_eq!(runs[0].opens, 2);
    Ok(())
}

#[rstest]
fn a_change_while_the_owner_reads_is_read_again(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // a and b were stat'd before the file changed, c and d after (new ctime):
    // c and d must not get the content read for a and b, nor the reverse
    let runs = differential(
        set_up_repo?,
        &[Source::new(vec![
            path("a", 5, 2, b"before the change").slow(),
            path("b", 5, 2, b"before the change"),
            path("c", 5, 2, b"after the change!").ctime(1_600_000_100),
            path("d", 5, 2, b"after the change!").ctime(1_600_000_100),
        ])],
        &ParentOptions::default(),
    )?;
    assert_ne!(id_of(&runs[0], "a"), id_of(&runs[0], "c"));
    Ok(())
}

#[rstest]
fn an_inode_number_reused_with_the_same_size_and_mtime_is_read(
    set_up_repo: Result<RepoOpen>,
) -> Result<()> {
    // run 2: inode 5 is another file now, with the same size and mtime but a
    // new ctime; neither path may get run 1's content
    let runs = differential(
        set_up_repo?,
        &[
            Source::new(vec![
                path("a", 5, 2, b"first file"),
                path("b", 5, 2, b"first file"),
            ]),
            Source::new(vec![
                path("a", 5, 2, b"other file").ctime(1_600_000_500),
                path("b", 5, 2, b"other file").ctime(1_600_000_500),
            ]),
        ],
        &ParentOptions::default(),
    )?;
    assert_ne!(id_of(&runs[0], "a"), id_of(&runs[1], "a"));
    assert_eq!(runs[1].opens, 1);
    Ok(())
}

#[rstest]
fn the_link_count_changing_between_runs(set_up_repo: Result<RepoOpen>) -> Result<()> {
    // 1 -> 2: a gains a link b (ctime changes) and new content of the same
    // size and mtime; 2 -> 1: b goes away again and a changes once more
    let runs = differential(
        set_up_repo?,
        &[
            Source::new(vec![path("a", 5, 1, b"one link")]),
            Source::new(vec![
                path("a", 5, 2, b"two link").ctime(1_600_000_100),
                path("b", 5, 2, b"two link").ctime(1_600_000_100),
            ]),
            Source::new(vec![path("a", 5, 1, b"one more").ctime(1_600_000_200)]),
        ],
        &ParentOptions::default(),
    )?;
    assert_ne!(id_of(&runs[0], "a"), id_of(&runs[1], "a"));
    assert_ne!(id_of(&runs[1], "a"), id_of(&runs[2], "a"));
    Ok(())
}

#[rstest]
fn a_parent_group_split_or_merged(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let runs = differential(
        set_up_repo?,
        &[
            // a, b, c: one inode; x, y: two separate inodes
            Source::new(vec![
                path("a", 5, 3, b"group of three"),
                path("b", 5, 3, b"group of three"),
                path("c", 5, 3, b"group of three"),
                path("x", 7, 1, b"inode seven"),
                path("y", 8, 1, b"inode eight"),
            ]),
            // c split off into a new inode; y now a link to x
            Source::new(vec![
                path("a", 5, 2, b"group of three").ctime(1_600_000_100),
                path("b", 5, 2, b"group of three").ctime(1_600_000_100),
                path("c", 6, 1, b"split off now!"),
                path("x", 7, 2, b"inode seven").ctime(1_600_000_100),
                path("y", 7, 2, b"inode seven").ctime(1_600_000_100),
            ]),
        ],
        &ParentOptions::default(),
    )?;
    assert_ne!(id_of(&runs[1], "c"), id_of(&runs[1], "a"));
    assert_eq!(id_of(&runs[1], "y"), id_of(&runs[1], "x"));
    Ok(())
}

/// Within one run, two paths of the same inode number whose stat differs in
/// one guarded way, with different bytes: the cache must not let the second
/// reuse the first, so both are read.  Each case fails if its guard in the
/// hard-link cache (state equality, or admission) is removed.
#[rstest]
#[case::size_differs(|_: &mut Path| (), |b: &mut Path| b.bytes.push(b'!'))]
#[case::mtime_differs(|_: &mut Path| (), |b: &mut Path| b.mtime = Some(T1))]
#[case::ctime_differs(|_: &mut Path| (), |b: &mut Path| b.ctime = Some(T1))]
#[case::inode_zero(|a: &mut Path| a.inode = 0, |b: &mut Path| b.inode = 0)]
#[case::single_link(|a: &mut Path| a.links = 1, |b: &mut Path| b.links = 1)]
#[case::no_mtime(|a: &mut Path| a.mtime = None, |b: &mut Path| b.mtime = None)]
#[case::no_ctime(|a: &mut Path| a.ctime = None, |b: &mut Path| b.ctime = None)]
fn the_cache_admits_and_matches_only_identical_hard_links(
    set_up_repo: Result<RepoOpen>,
    #[case] change_a: fn(&mut Path),
    #[case] change_b: fn(&mut Path),
) -> Result<()> {
    let mut a = path("a", 5, 2, b"bytes of a");
    let mut b = path("b", 5, 2, b"bytes of b");
    change_a(&mut a);
    change_b(&mut b);
    let runs = differential(
        set_up_repo?,
        &[Source::new(vec![a, b])],
        &ParentOptions::default(),
    )?;
    assert_ne!(id_of(&runs[0], "a"), id_of(&runs[0], "b"));
    assert_eq!(runs[0].opens, 2);
    Ok(())
}

/// A parent match that did not compare ctime and inode trusts that one path.
/// Reuse must not extend that trust to a path of the inode that has no parent
/// entry: without reuse, that path is read.  A match is weak when an ignore
/// option is set, or when the parent entry has no ctime or no inode number.
#[rstest]
#[case::ignore_ctime(ParentOptions::default().ignore_ctime(true), (5, Some(T0)), (5, T1))]
#[case::ignore_inode(ParentOptions::default().ignore_inode(true), (5, Some(T0)), (6, T0))]
#[case::parent_without_ctime(ParentOptions::default(), (5, None), (5, T1))]
#[case::parent_without_inode(ParentOptions::default(), (0, Some(T0)), (6, T0))]
fn a_weak_parent_match_does_not_vouch_for_new_paths(
    set_up_repo: Result<RepoOpen>,
    #[case] parent_opts: ParentOptions,
    #[case] first: (u64, Option<i64>),
    #[case] second: (u64, i64),
) -> Result<()> {
    // run 2: a has new bytes (same size and mtime) and a new link z; what
    // differs from run 1 is not compared, so a matches its parent entry
    let run1 = Path {
        ctime: first.1,
        ..path("a", first.0, 1, b"old bytes")
    };
    let runs = differential(
        set_up_repo?,
        &[
            Source::new(vec![run1]),
            Source::new(vec![
                path("a", second.0, 2, b"new bytes").ctime(second.1),
                path("z", second.0, 2, b"new bytes").ctime(second.1),
            ]),
        ],
        &parent_opts,
    )?;
    assert_ne!(id_of(&runs[1], "z"), id_of(&runs[0], "a"));
    Ok(())
}

const T0: i64 = 1_600_000_000;
const T1: i64 = 1_600_000_100;
