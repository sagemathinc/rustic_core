use anyhow::Result;
use insta::assert_debug_snapshot;
use rstest::rstest;
use tempfile::tempdir;

use rustic_core::CheckOptions;

use crate::repo_from_fixture;

#[rstest]
#[case("repo-data-missing.tar.gz")]
#[case("repo-duplicates.tar.gz")]
// #[case("repo-index-missing-blob.tar.gz")] TODO: Activate when paths are identical for unix/windows
#[case("repo-index-missing.tar.gz")]
#[case("repo-mixed.tar.gz")]
#[case("repo-obsolete-index.tar.gz")]
#[case("repo-unreferenced-data.tar.gz")]
#[case("repo-unused-data-missing.tar.gz")]
fn test_check(#[case] repo_file: &str) -> Result<()> {
    // unpack repo
    let dir = tempdir()?;
    let repo = repo_from_fixture(&dir, repo_file)?;

    let opts = CheckOptions::default().read_data(true);
    let check_results = repo.check(opts)?;
    assert_debug_snapshot!(repo_file, check_results);

    Ok(())
}

/// Open a password repository on a local directory, with this cache directory.
fn open_local(
    repo_dir: &std::path::Path,
    cache_dir: &std::path::Path,
) -> Result<rustic_core::Repository<rustic_core::OpenStatus>> {
    use rustic_core::{Credentials, Repository, RepositoryBackends, RepositoryOptions};
    use std::sync::Arc;
    let be = rustic_backend::LocalBackend::new(repo_dir.to_str().unwrap(), None)?;
    let be = RepositoryBackends::new(Arc::new(be), None);
    let options = RepositoryOptions::default().cache_dir(cache_dir.to_path_buf());
    Ok(Repository::new(&options, &be)?.open(&Credentials::password("test"))?)
}

#[rstest]
#[case::index("index")]
#[case::snapshot("snapshots")]
fn a_corrupt_file_is_found_with_a_warm_cache(#[case] subdir: &str) -> Result<()> {
    use rustic_core::{
        BackupOptions, ConfigOptions, Credentials, KeyOptions, PathList, Repository,
        RepositoryBackends, RepositoryOptions, repofile::SnapshotFile,
    };
    use std::sync::Arc;

    let repo_dir = tempdir()?;
    let cache_dir = tempdir()?;
    let source = tempdir()?;
    std::fs::write(source.path().join("file"), vec![7; 10_000])?;

    // a repository with one snapshot (an index and a snapshot file)
    {
        let be = rustic_backend::LocalBackend::new(repo_dir.path().to_str().unwrap(), None)?;
        let be = RepositoryBackends::new(Arc::new(be), None);
        let options = RepositoryOptions::default().cache_dir(cache_dir.path().to_path_buf());
        let repo = Repository::new(&options, &be)?.init(
            &Credentials::password("test"),
            &KeyOptions::default(),
            &ConfigOptions::default(),
        )?;
        let repo = repo.to_indexed_ids()?;
        let opts = BackupOptions::default().no_scan(true);
        _ = repo.backup(
            &opts,
            &PathList::from_iter([source.path()]),
            SnapshotFile::default(),
        )?;
    }

    // a clean check, which also warms the cache with the index and snapshot files
    let opts = CheckOptions::default().read_data(true);
    open_local(repo_dir.path(), cache_dir.path())?
        .check(opts)?
        .is_ok()?;

    // flip one byte of the repository's file; the cached copy stays intact
    let dir = repo_dir.path().join(subdir);
    let file = std::fs::read_dir(&dir)?
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_file())
        .expect("a file in the repository");
    let mut bytes = std::fs::read(&file)?;
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&file, bytes)?;

    // check must compare the cache with the repository, not with itself
    let results = open_local(repo_dir.path(), cache_dir.path())?.check(opts)?;
    assert!(
        results.is_ok().is_err(),
        "check with a warm cache passed a corrupt {subdir} file: {results:?}"
    );
    Ok(())
}
