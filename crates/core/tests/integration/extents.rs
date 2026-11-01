//! `BackupOptions::reuse_extents` in the archiver.
//!
//! Real btrfs signatures need a read-only subvolume and `CAP_SYS_ADMIN`; they
//! are tested by the `rustic_extents` loopback job and rustic's oracle. Here,
//! with the `test-hooks` feature, a source root holding
//! `.rustic-test-fake-extents` gets fake signatures (a first line `SIG:<name>`),
//! so the archiver's side runs anywhere: who reads, who reuses, the parent
//! seeding and its order, the end check and the statistics.

use std::{collections::BTreeMap, fs, path::Path};

use anyhow::Result;
use pretty_assertions::assert_eq;
use rstest::rstest;

use rustic_core::{
    BackupOptions, IndexedIdsStatus, LsOptions, ParentOptions, PathList, Repository,
    repofile::SnapshotFile,
};

use super::{RepoOpen, set_up_repo};

type Repo = Repository<IndexedIdsStatus>;

/// Path -> content ids of a snapshot.
fn contents(
    repo: &Repo,
    snapshot: &SnapshotFile,
) -> Result<BTreeMap<String, Vec<rustic_core::DataId>>> {
    let node = repo.node_from_snapshot_and_path(snapshot, "")?;
    Ok(repo
        .ls(&node, &LsOptions::default())?
        .map(|item| {
            let (path, node) = item.unwrap();
            (path.display().to_string(), node.content.unwrap_or_default())
        })
        .filter(|(_, content)| !content.is_empty())
        .collect())
}

/// Back up `dir` (with `parent`, if any) and reload the index.
fn backup(
    repo: Repo,
    dir: &Path,
    reuse: bool,
    parent: Option<&SnapshotFile>,
    parent_opts: ParentOptions,
) -> Result<(Repo, SnapshotFile)> {
    let parents: Vec<String> = parent.iter().map(|snap| snap.id.to_string()).collect();
    let opts = BackupOptions::default()
        .no_scan(true)
        .reuse_extents(reuse)
        .as_path(Path::new("test").to_path_buf())
        .parent_opts(parent_opts.parents(parents).force(parent.is_none()));
    let snapshot = repo.backup(&opts, &PathList::from_iter([dir]), SnapshotFile::default())?;
    Ok((repo.drop_index().to_indexed_ids()?, snapshot))
}

fn reused(snapshot: &SnapshotFile) -> (u64, u64) {
    let summary = snapshot.summary.as_ref().unwrap();
    (summary.extent_reused_files, summary.extent_reused_bytes)
}

fn write(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    fs::write(dir.join(name), data)?;
    Ok(())
}

#[rstest]
fn without_a_pinned_source_it_is_a_no_op(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let repo = set_up_repo?.to_indexed_ids()?;
    let dir = tempfile::tempdir()?;
    write(dir.path(), "a", &vec![7; 100_000])?;
    write(dir.path(), "b", &vec![7; 100_000])?;
    let (repo, plain) = backup(repo, dir.path(), false, None, ParentOptions::default())?;
    let (repo, with) = backup(repo, dir.path(), true, None, ParentOptions::default())?;
    assert_eq!(reused(&with), (0, 0));
    assert_eq!(contents(&repo, &with)?, contents(&repo, &plain)?);
    // zero statistics are not even serialized
    let json = serde_json::to_value(with.summary.as_ref().unwrap())?;
    assert!(json.get("extent_reused_files").is_none());
    Ok(())
}

#[cfg(all(unix, feature = "test-hooks"))]
mod fake {
    use super::*;
    use pretty_assertions::assert_eq;

    fn fake_source() -> Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        write(dir.path(), ".rustic-test-fake-extents", b"")?;
        Ok(dir)
    }

    fn signed(signature: &str, fill: u8, len: usize) -> Vec<u8> {
        let mut data = format!("SIG:{signature}\n").into_bytes();
        data.resize(len, fill);
        data
    }

    #[rstest]
    fn one_file_per_signature_is_read(set_up_repo: Result<RepoOpen>) -> Result<()> {
        let repo = set_up_repo?.to_indexed_ids()?;
        let dir = fake_source()?;
        let x = signed("x", 1, 300_000);
        for name in ["a", "b", "c", "d"] {
            write(dir.path(), name, &x)?;
        }
        write(dir.path(), "e", &signed("y", 2, 200_000))?;
        write(dir.path(), "f", &signed("y", 2, 200_000))?;
        write(dir.path(), "g", &vec![3; 50_000])?; // no signature
        write(dir.path(), "h", &vec![3; 50_000])?;

        let (repo, plain) = backup(repo, dir.path(), false, None, ParentOptions::default())?;
        let (repo, with) = backup(repo, dir.path(), true, None, ParentOptions::default())?;
        // three of the four x files and one of the two y files are not read
        assert_eq!(reused(&with), (4, 3 * 300_000 + 200_000));
        assert_eq!(reused(&plain), (0, 0));
        assert_eq!(contents(&repo, &with)?, contents(&repo, &plain)?);
        Ok(())
    }

    #[rstest]
    fn a_vouched_parent_match_seeds_later_files(set_up_repo: Result<RepoOpen>) -> Result<()> {
        let repo = set_up_repo?.to_indexed_ids()?;
        let dir = fake_source()?;
        let x = signed("x", 1, 300_000);
        write(dir.path(), "m", &x)?;
        let (repo, first) = backup(repo, dir.path(), true, None, ParentOptions::default())?;
        assert_eq!(reused(&first), (0, 0));

        // "m" is unchanged; "n" and "z", after it, are new files with its
        // signature; "a", before it, is read (the seed comes later in the tree)
        for name in ["a", "n", "z"] {
            write(dir.path(), name, &x)?;
        }
        let (repo, second) = backup(
            repo,
            dir.path(),
            true,
            Some(&first),
            ParentOptions::default(),
        )?;
        assert_eq!(reused(&second), (2, 2 * 300_000));
        let all = contents(&repo, &second)?;
        assert_eq!(all.len(), 4, "{all:?}");
        let m = all.iter().find(|(path, _)| path.ends_with('m')).unwrap().1;
        assert!(all.values().all(|content| content == m), "{all:?}");

        // a match that ignored ctime doesn't vouch for the inode: no seed
        let (_, third) = backup(
            repo,
            dir.path(),
            true,
            Some(&first),
            ParentOptions::default().ignore_ctime(true),
        )?;
        // "a" reads, and "n" and "z" reuse it: nothing came from the parent
        assert_eq!(reused(&third), (2, 2 * 300_000));
        Ok(())
    }

    #[rstest]
    fn a_changed_source_saves_no_snapshot(set_up_repo: Result<RepoOpen>) -> Result<()> {
        let repo = set_up_repo?.to_indexed_ids()?;
        let dir = fake_source()?;
        write(dir.path(), "a", &signed("x", 1, 10_000))?;
        write(dir.path(), ".rustic-test-end-fails", b"")?;
        let opts = BackupOptions::default().no_scan(true).reuse_extents(true);
        let err = repo
            .backup(
                &opts,
                &PathList::from_iter([dir.path()]),
                SnapshotFile::default(),
            )
            .unwrap_err();
        assert!(err.to_string().contains("no snapshot was saved"), "{err}");
        assert_eq!(repo.get_all_snapshots()?, Vec::new());
        Ok(())
    }
}
