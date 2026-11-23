use std::{collections::HashSet, fs, path::PathBuf, str::FromStr};

use anyhow::Result;
use bytesize::ByteSize;
use insta::{Settings, assert_ron_snapshot};
use rstest::rstest;
use tempfile::tempdir;

use rustic_core::{
    BackupOptions, CheckOptions, ConfigOptions, PathList,
    repofile::{Chunker, SnapshotFile},
};

use super::{RepoOpen, TestSource, insta_snapshotfile_redaction, set_up_repo, tar_gz_testdata};

#[rstest]
fn test_chunker_params(
    tar_gz_testdata: Result<TestSource>,
    set_up_repo: Result<RepoOpen>,
    insta_snapshotfile_redaction: Settings,
) -> Result<()> {
    // uncomment for logging output
    // SimpleLogger::init(log::LevelFilter::Debug, Config::default())?;

    // Fixtures
    let (source, mut repo) = (tar_gz_testdata?, set_up_repo?.to_indexed_ids()?);

    let paths = &source.path_list();

    // set fixed size chunker with a given chunk size
    let config_opts = ConfigOptions::default()
        .set_chunker(Chunker::FixedSize)
        .set_chunk_size(ByteSize(8000));

    assert!(repo.apply_config(&config_opts)?);

    // we use as_path to not depend on the actual tempdir
    let opts = BackupOptions::default().as_path(PathBuf::from_str("test")?);
    let snapshot = repo.backup(&opts, paths, SnapshotFile::default())?;

    // We can also bind to scope ( https://docs.rs/insta/latest/insta/struct.Settings.html#method.bind_to_scope )
    // But I think that can get messy with a lot of tests, also checking which settings are currently applied
    // will be probably harder
    insta_snapshotfile_redaction.bind(|| {
        assert_ron_snapshot!("chunker-fixedsize", &snapshot);
    });

    Ok(())
}

#[rstest]
#[case(Chunker::Rabin)]
#[case(Chunker::FixedSize)]
fn test_backup_zeros(set_up_repo: Result<RepoOpen>, #[case] chunker: Chunker) -> Result<()> {
    let mut repo = set_up_repo?.to_indexed_ids()?;
    let config_opts = ConfigOptions::default().set_chunker(chunker);
    _ = repo.apply_config(&config_opts)?;

    // Runs of zeros between data, so that zero chunks follow and precede other chunks.
    let mut data = vec![0; 8 << 20];
    let mut x: u32 = 1;
    for range in [0..100_000, 3_000_123..3_100_000, (8 << 20) - 5..8 << 20] {
        for b in &mut data[range] {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            *b = (x >> 24) as u8 | 1;
        }
    }
    let dir = tempdir()?;
    let file = dir.path().join("zeros");
    fs::write(&file, &data)?;

    let opts = BackupOptions::default().as_path(PathBuf::from_str("test")?);
    _ = repo.backup(
        &opts,
        &PathList::from_iter(Some(dir.path().to_path_buf())),
        SnapshotFile::default(),
    )?;

    // The zero chunks are stored once, under the hash of their content.
    let repo = repo.to_indexed()?;
    repo.check(CheckOptions::default().read_data(true))?
        .is_ok()?;
    let node = repo.node_from_snapshot_path("latest:test/zeros", |_| true)?;
    let content = node.content.as_ref().unwrap();
    assert!(content.iter().collect::<HashSet<_>>().len() < content.len());
    let mut restored = Vec::new();
    repo.dump(&node, &mut restored)?;
    assert_eq!(restored, data);
    Ok(())
}

/// The same bytes stored densely and as a sparse file back up to the same content ids:
/// skipped holes and zero runs read from the file get identical chunks.
#[cfg(target_os = "linux")]
#[rstest]
#[case(Chunker::Rabin)]
#[case(Chunker::FixedSize)]
fn test_backup_sparse_matches_dense(
    set_up_repo: Result<RepoOpen>,
    #[case] chunker: Chunker,
) -> Result<()> {
    use std::os::unix::fs::{FileExt, MetadataExt};

    use rustic_core::ParentOptions;

    let mut repo = set_up_repo?.to_indexed_ids()?;
    let config_opts = ConfigOptions::default().set_chunker(chunker);
    _ = repo.apply_config(&config_opts)?;

    // Data islands between long zero runs, the last one ending at EOF.
    let len = 12 << 20;
    let islands = [0..100_000, 4_000_123..4_100_000, (9 << 20)..(9 << 20) + 7, len - 5..len];
    let mut data = vec![0; len];
    let mut x: u32 = 7;
    for range in islands.clone() {
        for b in &mut data[range] {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            *b = (x >> 24) as u8 | 1;
        }
    }
    let dense_dir = tempdir()?;
    fs::write(dense_dir.path().join("file"), &data)?;
    let sparse_dir = tempdir()?;
    let sparse = fs::File::create(sparse_dir.path().join("file"))?;
    sparse.set_len(len as u64)?;
    for range in islands {
        sparse.write_all_at(&data[range.clone()], range.start as u64)?;
    }
    drop(sparse);
    let blocks = fs::metadata(sparse_dir.path().join("file"))?.blocks();
    assert!(blocks * 512 < len as u64 / 2, "the test file system must keep holes");

    let opts = BackupOptions::default().as_path(PathBuf::from_str("test")?);
    let dense = repo.backup(
        &opts,
        &PathList::from_iter(Some(dense_dir.path().to_path_buf())),
        SnapshotFile::default(),
    )?;
    let opts = opts.parent_opts(ParentOptions::default().force(true));
    let sparse = repo.backup(
        &opts,
        &PathList::from_iter(Some(sparse_dir.path().to_path_buf())),
        SnapshotFile::default(),
    )?;

    // The sparse backup adds no data.
    let summary = sparse.summary.as_ref().unwrap();
    assert_eq!(summary.files_new, 1);
    assert_eq!(summary.data_blobs, 0);
    assert_eq!(summary.data_added, 0);

    let repo = repo.to_indexed()?;
    repo.check(CheckOptions::default().read_data(true))?
        .is_ok()?;
    let dense_node =
        repo.node_from_snapshot_path(&format!("{}:test/file", dense.id), |_| true)?;
    let sparse_node =
        repo.node_from_snapshot_path(&format!("{}:test/file", sparse.id), |_| true)?;
    assert_eq!(sparse_node.content, dense_node.content);
    let mut restored = Vec::new();
    repo.dump(&sparse_node, &mut restored)?;
    assert_eq!(restored, data);
    Ok(())
}
