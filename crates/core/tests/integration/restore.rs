use std::{fs, path::PathBuf, str::FromStr};

#[cfg(not(windows))]
use std::os::unix::fs::MetadataExt;

use anyhow::Result;
use pretty_assertions::assert_eq;
use rstest::rstest;
use tempfile::tempdir;

use rustic_core::{
    BackupOptions, LocalDestination, LsOptions, RestoreOptions, SparseRestore,
    repofile::SnapshotFile,
};

use super::{RepoOpen, TestSource, set_up_repo, tar_gz_testdata};

#[rstest]
#[cfg(not(windows))]
fn test_restore_preserves_hardlinks(
    tar_gz_testdata: Result<TestSource>,
    set_up_repo: Result<RepoOpen>,
) -> Result<()> {
    let (source, repo) = (tar_gz_testdata?, set_up_repo?.to_indexed_ids()?);

    let opts = BackupOptions::default().as_path(PathBuf::from_str("test")?);
    let _snapshot = repo.backup(&opts, &source.path_list(), SnapshotFile::default())?;

    let repo = repo.to_indexed()?;
    let node = repo.node_from_snapshot_path("latest", |_| true)?;
    let ls_opts = LsOptions::default();
    let ls = repo.ls(&node, &ls_opts)?;

    let restore_dir = tempdir()?;
    let dest = LocalDestination::new(
        restore_dir
            .path()
            .to_str()
            .expect("restore path is valid utf-8"),
        true,
        !node.is_dir(),
    )?;
    let restore_opts = RestoreOptions::default();
    let restore = || -> Result<()> {
        let plan = repo.prepare_restore(&restore_opts, ls.clone(), &dest, false)?;
        repo.restore(plan, &restore_opts, ls.clone(), &dest)?;
        Ok(())
    };
    restore()?;

    let hardlink = restore_dir.path().join("test/0/tests/testfile-hardlink");
    let linked = restore_dir.path().join("test/0/tests/testfile");
    let symlink = restore_dir.path().join("test/0/tests/testfile-symlink");

    let hardlink_meta = fs::metadata(&hardlink)?;
    let linked_meta = fs::metadata(&linked)?;
    assert_eq!(hardlink_meta.dev(), linked_meta.dev());
    assert_eq!(hardlink_meta.ino(), linked_meta.ino());
    assert_eq!(hardlink_meta.nlink(), 2);
    assert_eq!(linked_meta.nlink(), 2);
    assert_eq!(fs::read_to_string(&hardlink)?, fs::read_to_string(&linked)?);
    assert_eq!(fs::read_link(&symlink)?, PathBuf::from("testfile"));

    restore()?;
    assert_eq!(fs::metadata(&hardlink)?.ino(), linked_meta.ino());

    fs::remove_file(&hardlink)?;
    fs::write(&hardlink, "stale")?;
    restore()?;
    assert_eq!(fs::metadata(&hardlink)?.ino(), linked_meta.ino());

    Ok(())
}

#[rstest]
#[cfg(target_os = "linux")]
fn sparse_restore_roundtrip_and_overwrite(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let source = TestSource::new(tempdir()?);
    let mut mixed = vec![0; 16 * 1024 * 1024];
    for offset in (0..mixed.len()).step_by(65536) {
        mixed[offset..offset + 4096].fill(7);
    }
    let fixtures = [
        ("mixed", mixed),
        ("zero", vec![0; 1024 * 1024]),
        ("tail", [vec![9; 17], vec![0; 32768]].concat()),
        ("empty", vec![]),
    ];
    for (name, bytes) in &fixtures {
        fs::write(source.path().join(name), bytes)?;
    }
    fs::hard_link(source.path().join("mixed"), source.path().join("linked"))?;
    let repo = set_up_repo?.to_indexed_ids()?;
    let _snapshot = repo.backup(
        &BackupOptions::default().as_path(PathBuf::from("test")),
        &source.path_list(),
        SnapshotFile::default(),
    )?;
    let repo = repo.to_indexed()?;
    let node = repo.node_from_snapshot_path("latest", |_| true)?;
    let opts = RestoreOptions::default()
        .strict(true)
        .sparse(SparseRestore::ByContentRequired)
        .verify_existing(true)
        .no_ownership(true);
    for existing in [false, true] {
        let restore_dir = tempdir()?;
        if existing {
            fs::create_dir(restore_dir.path().join("test"))?;
            for (name, bytes) in &fixtures {
                fs::write(
                    restore_dir.path().join("test").join(name),
                    vec![0xa5; bytes.len()],
                )?;
            }
        }
        let dest = LocalDestination::new(restore_dir.path().to_str().unwrap(), true, false)?;
        let ls = repo.ls(&node, &LsOptions::default())?;
        let plan = repo.prepare_restore(&opts, ls.clone(), &dest, false)?;
        repo.restore(plan, &opts, ls, &dest)?;
        for (name, bytes) in &fixtures {
            let path = restore_dir.path().join("test").join(name);
            fs::File::open(&path)?.sync_all()?;
            assert_eq!(&fs::read(&path)?, bytes);
            if *name == "mixed" {
                assert!(fs::metadata(&path)?.blocks() * 512 < bytes.len() as u64 / 2);
            }
        }
        assert_eq!(
            fs::metadata(restore_dir.path().join("test/mixed"))?.ino(),
            fs::metadata(restore_dir.path().join("test/linked"))?.ino(),
        );
        // Reuse verified chunks, but repair damage even if mtime has been reset.
        let zero_path = restore_dir.path().join("test/zero");
        let timestamp = filetime::FileTime::from_last_modification_time(&fs::metadata(&zero_path)?);
        fs::write(&zero_path, vec![0xa5; 1024 * 1024])?;
        filetime::set_file_mtime(&zero_path, timestamp)?;
        let ls = repo.ls(&node, &LsOptions::default())?;
        let plan = repo.prepare_restore(&opts, ls.clone(), &dest, false)?;
        assert!(plan.matched_size > 0);
        repo.restore(plan, &opts, ls, &dest)?;
        assert_eq!(fs::read(zero_path)?, vec![0; 1024 * 1024]);
    }
    Ok(())
}

#[rstest]
fn restore_write_failure_returns_error(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let source = TestSource::new(tempdir()?);
    fs::write(source.path().join("file"), vec![7; 16384])?;
    let repo = set_up_repo?.to_indexed_ids()?;
    let _snapshot = repo.backup(
        &BackupOptions::default().as_path(PathBuf::from("test")),
        &source.path_list(),
        SnapshotFile::default(),
    )?;
    let repo = repo.to_indexed()?;
    let node = repo.node_from_snapshot_path("latest", |_| true)?;
    let restore_dir = tempdir()?;
    let dest = LocalDestination::new(restore_dir.path().to_str().unwrap(), true, false)?;
    let opts = RestoreOptions::default().no_ownership(true);
    let ls = repo.ls(&node, &LsOptions::default())?;
    let plan = repo.prepare_restore(&opts, ls.clone(), &dest, false)?;
    fs::create_dir_all(restore_dir.path().join("test/file"))?;
    assert!(repo.restore(plan, &opts, ls, &dest).is_err());
    Ok(())
}

#[rstest]
#[cfg(target_os = "linux")]
fn strict_restore_reports_metadata_failure(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let source = TestSource::new(tempdir()?);
    fs::write(source.path().join("file"), [7; 8192])?;
    let repo = set_up_repo?.to_indexed_ids()?;
    let _snapshot = repo.backup(
        &BackupOptions::default()
            .as_path(PathBuf::from("test"))
            .strict(true),
        &source.path_list(),
        SnapshotFile::default(),
    )?;
    let repo = repo.to_indexed()?;
    let node = repo.node_from_snapshot_path("latest", |_| true)?;
    for strict in [false, true] {
        let restore_dir = tempdir()?;
        let dest = LocalDestination::new(restore_dir.path().to_str().unwrap(), true, false)?;
        let opts = RestoreOptions::default().strict(strict).no_ownership(true);
        let ls = repo.ls(&node, &LsOptions::default())?;
        let plan = repo.prepare_restore(&opts, ls.clone(), &dest, false)?;
        let modified = ls.map(|item| {
            item.map(|(path, mut node)| {
                if node.is_file() {
                    node.meta.extended_attributes = serde_json::from_value(serde_json::json!([
                        {"name": "invalid-xattr-namespace", "value": "AQ=="}
                    ]))
                    .unwrap();
                }
                (path, node)
            })
        });
        assert_eq!(repo.restore(plan, &opts, modified, &dest).is_err(), strict);
    }
    Ok(())
}

#[rstest]
#[cfg(not(windows))]
fn restore_delete_replaces_entries_of_another_type(set_up_repo: Result<RepoOpen>) -> Result<()> {
    let source = TestSource::new(tempdir()?);
    fs::create_dir(source.path().join("empty-dir"))?;
    fs::create_dir(source.path().join("full-dir"))?;
    fs::write(source.path().join("full-dir/child"), "child")?;
    fs::write(source.path().join("file-a"), "file a")?;
    fs::write(source.path().join("file-b"), "file b")?;
    let repo = set_up_repo?.to_indexed_ids()?;
    let _snapshot = repo.backup(
        &BackupOptions::default().as_path(PathBuf::from("test")),
        &source.path_list(),
        SnapshotFile::default(),
    )?;
    let repo = repo.to_indexed()?;
    let node = repo.node_from_snapshot_path("latest", |_| true)?;
    let restore_dir = tempdir()?;
    let dest = LocalDestination::new(restore_dir.path().to_str().unwrap(), true, false)?;
    let opts = RestoreOptions::default().delete(true).no_ownership(true);
    let restore = || -> Result<()> {
        let ls = repo.ls(&node, &LsOptions::default())?;
        let plan = repo.prepare_restore(&opts, ls.clone(), &dest, false)?;
        repo.restore(plan, &opts, ls, &dest)?;
        Ok(())
    };
    restore()?;

    // A file where each directory was, a directory where each file was.
    let target = restore_dir.path().join("test");
    fs::remove_dir(target.join("empty-dir"))?;
    fs::write(target.join("empty-dir"), "not a directory")?;
    fs::remove_dir_all(target.join("full-dir"))?;
    fs::write(target.join("full-dir"), "not a directory")?;
    fs::remove_file(target.join("file-a"))?;
    fs::create_dir(target.join("file-a"))?;
    fs::remove_file(target.join("file-b"))?;
    fs::create_dir(target.join("file-b"))?;
    fs::write(target.join("file-b/inside"), "not in the snapshot")?;
    restore()?;

    assert!(fs::symlink_metadata(target.join("empty-dir"))?.is_dir());
    assert_eq!(fs::read_dir(target.join("empty-dir"))?.count(), 0);
    assert!(fs::symlink_metadata(target.join("full-dir"))?.is_dir());
    assert_eq!(fs::read_to_string(target.join("full-dir/child"))?, "child");
    assert_eq!(fs::read_to_string(target.join("file-a"))?, "file a");
    assert_eq!(fs::read_to_string(target.join("file-b"))?, "file b");
    let mut names: Vec<_> = fs::read_dir(&target)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<_, _>>()?;
    names.sort();
    assert_eq!(names, ["empty-dir", "file-a", "file-b", "full-dir"]);
    Ok(())
}
