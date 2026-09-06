//! Metadata-only admission for immutable backup sources. This does not replace an
//! external process deadline/memory limit: a filesystem syscall can itself block.
use std::{
    io,
    num::NonZeroU64,
    path::Path,
    time::{Duration, Instant},
};

use derive_setters::Setters;
use serde::{Deserialize, Serialize};

use crate::{
    ReadSource,
    backend::node::Node,
    error::{ErrorKind, RusticError, RusticResult},
    repofile::{ConfigFile, configfile::Chunker},
};

/// Optional work limits. An absent value is unlimited; zero is invalid.
/// These are admission limits, not filters: exceeding one rejects the backup.
#[cfg_attr(feature = "clap", derive(clap::Parser))]
#[cfg_attr(feature = "merge", derive(conflate::Merge))]
#[derive(Clone, Default, Debug, Deserialize, Serialize, Setters)]
#[serde(default, rename_all = "kebab-case", deny_unknown_fields)]
#[setters(into)]
#[non_exhaustive]
pub struct BackupAdmissionOptions {
    /// Maximum selected directory entries, including each hard-linked path.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub max_entries: Option<NonZeroU64>,
    /// Maximum sum of regular-file apparent sizes, without deduplication credits.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub max_apparent_bytes: Option<NonZeroU64>,
    /// Maximum individual regular-file apparent size. Equality is allowed.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub max_file_bytes: Option<NonZeroU64>,
    /// Maximum conservative content-reference count for the configured chunker.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub max_chunk_references: Option<NonZeroU64>,
    /// Maximum serialized source-node metadata bytes (excluding content references).
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub max_metadata_bytes: Option<NonZeroU64>,
    /// Maximum number of path components returned by the source.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub max_path_depth: Option<NonZeroU64>,
    /// Maximum preflight elapsed seconds; also enforce an external process deadline.
    #[cfg_attr(feature = "clap", clap(long))]
    #[cfg_attr(feature = "merge", merge(strategy = conflate::option::overwrite_none))]
    pub preflight_timeout_seconds: Option<NonZeroU64>,
}

impl BackupAdmissionOptions {
    /// Whether any limit requests admission before content processing.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.max_entries.is_some()
            || self.max_apparent_bytes.is_some()
            || self.max_file_bytes.is_some()
            || self.max_chunk_references.is_some()
            || self.max_metadata_bytes.is_some()
            || self.max_path_depth.is_some()
            || self.preflight_timeout_seconds.is_some()
    }
}

/// A successfully completed inventory, not proof of an immutable source or a backup.
///
/// Reference bounds describe logical metadata before compression/deduplication;
/// they are not total process-memory, repository-size or restore-capacity bounds.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct BackupInventory {
    /// Selected directory entries. Hard links count separately.
    pub entries: u64,
    /// Selected regular-file entries.
    pub files: u64,
    /// Sum of regular-file apparent sizes, including each hard-linked path.
    pub apparent_bytes: u64,
    /// Maximum possible content references for the configured minimum chunk size.
    pub chunk_references_bound: u64,
    /// Reference JSON upper bound: 64 hex digits, two quotes, and a comma per ID.
    pub content_reference_bytes_bound: u64,
    /// Serialized source-node metadata, including names and extended attributes.
    pub node_metadata_bytes: u64,
    /// Greatest observed source path component count.
    pub max_path_depth: u64,
}

/// Completed inspection of normal backup scope, before a size exclusion filter.
///
/// Excluded entries still consume traversal and metadata budgets. Only their
/// content work is omitted from retained apparent-byte/reference admission.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct BackupSelectionInventory {
    /// Metadata/work bounds for the retained selection.
    pub retained: BackupInventory,
    /// All inspected directory entries, including oversized files.
    pub inspected_entries: u64,
    /// Metadata bytes for all inspected nodes, including oversized files.
    pub inspected_node_metadata_bytes: u64,
    /// Largest inspected path depth, including oversized files.
    pub inspected_max_path_depth: u64,
    /// Number of regular-file paths omitted solely by the supplied size limit.
    pub excluded_files: u64,
    /// Sum of omitted logical sizes, without hardlink deduplication credits.
    pub excluded_apparent_bytes: u64,
}

fn invalid(reason: &'static str) -> Box<RusticError> {
    RusticError::new(
        ErrorKind::InvalidInput,
        "Backup admission failed: {reason}.",
    )
    .attach_context("reason", reason)
}

fn add(left: u64, right: u64) -> RusticResult<u64> {
    left.checked_add(right)
        .ok_or_else(|| invalid("arithmetic overflow"))
}

fn check(value: u64, limit: Option<NonZeroU64>, name: &'static str) -> RusticResult<()> {
    if let Some(limit) = limit
        && value > limit.get()
    {
        return Err(invalid("work budget exceeded")
            .attach_context("budget", name)
            .attach_context("limit", limit.to_string())
            .attach_context("observed", value.to_string()));
    }
    Ok(())
}

fn check_deadline(started: Instant, seconds: Option<NonZeroU64>) -> RusticResult<()> {
    if let Some(seconds) = seconds
        && started.elapsed() >= Duration::from_secs(seconds.get())
    {
        return Err(invalid("preflight deadline exceeded"));
    }
    Ok(())
}

struct MetadataCounter {
    bytes: u64,
    remaining: u64,
}

impl io::Write for MetadataCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(buf.len()).map_err(io::Error::other)?;
        if length > self.remaining {
            return Err(io::Error::other("backup metadata budget exceeded"));
        }
        self.bytes += length;
        self.remaining -= length;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Inspect selected metadata without opening content or saving a snapshot.
///
/// Callers requiring a stable decision must supply the same immutable source to
/// this function and the backup. Filters are those of `source`; this function
/// does not silently introduce exclusions. Host resource limits must also cover
/// traversal, node construction, repository indexes and subsequent backup work.
///
/// # Errors
/// Rejects unknown-size streams, inspection failures, invalid chunk parameters,
/// arithmetic overflow, elapsed deadlines and exceeded limits. Returns no
/// successful inventory after a partial scan.
pub fn backup_inventory<R: ReadSource>(
    source: &R,
    config: &ConfigFile,
    limits: &BackupAdmissionOptions,
) -> RusticResult<BackupInventory> {
    Ok(backup_selection_inventory(source, config, limits, None, |_, _| Ok(()))?.retained)
}

/// Inspect scope before size filtering and stream every oversized file to a sink.
///
/// `source` must apply normal scope filters but MUST NOT already filter by size.
/// Equality with `exclude_larger_than` remains eligible. The callback receives
/// metadata only; it must not open file contents and should use a bounded output
/// sink. A callback failure aborts the scan. Streamed records are provisional
/// until this function returns successfully; a prefix is never complete evidence.
/// Immutable-source identity and binding the report to a backup belong to callers.
///
/// # Errors
/// All inventory/admission errors, including unknown metadata, budget exhaustion,
/// arithmetic overflow and output-sink failures, abort instead of omitting data.
#[allow(clippy::too_many_lines)]
pub fn backup_selection_inventory<R: ReadSource>(
    source: &R,
    config: &ConfigFile,
    limits: &BackupAdmissionOptions,
    exclude_larger_than: Option<NonZeroU64>,
    mut on_excluded: impl FnMut(&Path, &Node) -> RusticResult<()>,
) -> RusticResult<BackupSelectionInventory> {
    let started = Instant::now();
    let entries = source
        .inventory_entries()
        .ok_or_else(|| invalid("source does not support metadata-only inventory"))?;
    let minimum = match config.chunker() {
        Chunker::Rabin => config.chunk_min_size(),
        Chunker::FixedSize => config.chunk_size(),
    };
    let minimum = u64::try_from(minimum).map_err(|_| invalid("invalid minimum chunk size"))?;
    if minimum == 0 {
        return Err(invalid("minimum chunk size must be positive"));
    }
    let mut selection = BackupSelectionInventory::default();
    for entry in entries {
        check_deadline(started, limits.preflight_timeout_seconds)?;
        let entry = entry?;
        selection.inspected_entries = add(selection.inspected_entries, 1)?;
        check(
            selection.inspected_entries,
            limits.max_entries,
            "max-entries",
        )?;
        let depth = u64::try_from(entry.path.components().count())
            .map_err(|_| invalid("path depth overflow"))?;
        selection.inspected_max_path_depth = selection.inspected_max_path_depth.max(depth);
        check(depth, limits.max_path_depth, "max-path-depth")?;

        let node = entry.node;
        // Inventory sources describe metadata only, never a pre-populated tree.
        if node.content.is_some() || node.subtree.is_some() {
            return Err(invalid("source inventory contains non-metadata nodes"));
        }
        let mut counter = MetadataCounter {
            bytes: 0,
            remaining: limits
                .max_metadata_bytes
                .map_or(u64::MAX, NonZeroU64::get)
                .saturating_sub(selection.inspected_node_metadata_bytes),
        };
        serde_json::to_writer(&mut counter, &node).map_err(|err| {
            RusticError::with_source(
                ErrorKind::InvalidInput,
                "Backup metadata inventory failed or exceeded max-metadata-bytes.",
                err,
            )
        })?;
        selection.inspected_node_metadata_bytes =
            add(selection.inspected_node_metadata_bytes, counter.bytes)?;
        if node.is_file() && exclude_larger_than.is_some_and(|limit| node.meta.size > limit.get()) {
            selection.excluded_files = add(selection.excluded_files, 1)?;
            selection.excluded_apparent_bytes =
                add(selection.excluded_apparent_bytes, node.meta.size)?;
            on_excluded(&entry.path, &node)?;
            continue;
        }
        let result = &mut selection.retained;
        result.entries = add(result.entries, 1)?;
        result.max_path_depth = result.max_path_depth.max(depth);
        result.node_metadata_bytes = add(result.node_metadata_bytes, counter.bytes)?;
        if node.is_file() {
            check(node.meta.size, limits.max_file_bytes, "max-file-bytes")?;
            result.files = add(result.files, 1)?;
            result.apparent_bytes = add(result.apparent_bytes, node.meta.size)?;
            check(
                result.apparent_bytes,
                limits.max_apparent_bytes,
                "max-apparent-bytes",
            )?;
            result.chunk_references_bound = add(
                result.chunk_references_bound,
                node.meta.size.div_ceil(minimum),
            )?;
            check(
                result.chunk_references_bound,
                limits.max_chunk_references,
                "max-chunk-references",
            )?;
            result.content_reference_bytes_bound = result
                .chunk_references_bound
                .checked_mul(67)
                .ok_or_else(|| invalid("content-reference metadata overflow"))?;
        }
    }
    check_deadline(started, limits.preflight_timeout_seconds)?;
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ReadSourceEntry,
        backend::node::{Metadata, Node, NodeType},
    };
    use std::{ffi::OsStr, path::PathBuf};

    struct MetadataSource {
        sizes: Vec<u64>,
        fail: bool,
    }

    impl ReadSource for MetadataSource {
        type Open = io::Empty;
        type Iter = std::vec::IntoIter<RusticResult<ReadSourceEntry<Self::Open>>>;
        fn size(&self) -> RusticResult<Option<u64>> {
            panic!("inventory must not use the best-effort size scanner");
        }
        fn entries(&self) -> Self::Iter {
            panic!("inventory must not use the sorted backup walker");
        }
        fn inventory_entries(&self) -> Option<Self::Iter> {
            let mut entries = self
                .sizes
                .iter()
                .enumerate()
                .map(|(i, &size)| {
                    let name = i.to_string();
                    Ok(ReadSourceEntry {
                        path: PathBuf::from(&name),
                        node: Node::new_node(
                            OsStr::new(&name),
                            NodeType::File,
                            Metadata {
                                size,
                                ..Metadata::default()
                            },
                        ),
                        open: None,
                    })
                })
                .collect::<Vec<_>>();
            if self.fail {
                entries.push(Err(invalid("injected metadata error")));
            }
            Some(entries.into_iter())
        }
    }

    #[test]
    fn counts_reference_expansion_not_physical_or_deduplicated_size() {
        let src = MetadataSource {
            sizes: vec![1_u64 << 40, 1, 0],
            fail: false,
        };
        let report = backup_inventory(
            &src,
            &ConfigFile::default(),
            &BackupAdmissionOptions::default(),
        )
        .unwrap();
        assert_eq!(report.entries, 3);
        assert_eq!(report.files, 3);
        assert_eq!(report.apparent_bytes, (1_u64 << 40) + 1);
        assert_eq!(report.chunk_references_bound, 2_097_153);
        assert_eq!(report.content_reference_bytes_bound, 2_097_153 * 67);
        assert!(report.node_metadata_bytes > 0);
    }

    #[test]
    fn selection_counts_excluded_metadata_without_charging_retained_content() {
        let source = MetadataSource {
            sizes: vec![3, 4, 1 << 40],
            fail: false,
        };
        let limits = BackupAdmissionOptions::default()
            .max_entries(NonZeroU64::new(3))
            .max_file_bytes(NonZeroU64::new(3))
            .max_apparent_bytes(NonZeroU64::new(3))
            .max_chunk_references(NonZeroU64::new(1));
        let mut excluded = Vec::new();
        let result = backup_selection_inventory(
            &source,
            &ConfigFile::default(),
            &limits,
            NonZeroU64::new(3),
            |path, node| {
                excluded.push((path.to_owned(), node.meta.size));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(result.inspected_entries, 3);
        assert_eq!(result.retained.entries, 1);
        assert_eq!(result.retained.apparent_bytes, 3);
        assert_eq!(result.retained.chunk_references_bound, 1);
        assert_eq!(result.excluded_files, 2);
        assert_eq!(result.excluded_apparent_bytes, (1 << 40) + 4);
        assert!(result.inspected_node_metadata_bytes > result.retained.node_metadata_bytes);
        assert_eq!(
            excluded,
            vec![(PathBuf::from("1"), 4), (PathBuf::from("2"), 1 << 40)]
        );
        for limits in [
            limits.clone().max_entries(NonZeroU64::new(2)),
            limits.max_metadata_bytes(NonZeroU64::new(result.inspected_node_metadata_bytes - 1)),
        ] {
            assert!(
                backup_selection_inventory(
                    &source,
                    &ConfigFile::default(),
                    &limits,
                    NonZeroU64::new(3),
                    |_, _| Ok(())
                )
                .is_err()
            );
        }
    }

    #[test]
    fn selection_sink_and_scan_failures_never_complete_a_partial_inventory() {
        let config = ConfigFile::default();
        let limits = BackupAdmissionOptions::default();
        for source in [
            MetadataSource {
                sizes: vec![4],
                fail: true,
            },
            MetadataSource {
                sizes: vec![u64::MAX, 4],
                fail: false,
            },
        ] {
            assert!(
                backup_selection_inventory(
                    &source,
                    &config,
                    &limits,
                    NonZeroU64::new(3),
                    |_, _| Ok(())
                )
                .is_err()
            );
        }
        assert!(
            backup_selection_inventory(
                &MetadataSource {
                    sizes: vec![4],
                    fail: false
                },
                &config,
                &limits,
                NonZeroU64::new(3),
                |_, _| Err(invalid("report storage failed"))
            )
            .is_err()
        );
        let result = backup_selection_inventory(
            &MetadataSource {
                sizes: vec![4],
                fail: false,
            },
            &config,
            &limits,
            NonZeroU64::new(3),
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(result.retained, BackupInventory::default());
        assert_eq!(result.excluded_files, 1);
    }

    #[test]
    fn limits_are_inclusive_but_fail_closed_on_any_excess() {
        let src = MetadataSource {
            sizes: vec![3, 3],
            fail: false,
        };
        let opts = BackupAdmissionOptions::default()
            .max_file_bytes(NonZeroU64::new(3))
            .max_apparent_bytes(NonZeroU64::new(6))
            .max_entries(NonZeroU64::new(2))
            .max_chunk_references(NonZeroU64::new(2))
            .max_path_depth(NonZeroU64::new(1));
        let report = backup_inventory(&src, &ConfigFile::default(), &opts).unwrap();
        for limits in [
            opts.clone().max_file_bytes(NonZeroU64::new(2)),
            opts.clone().max_apparent_bytes(NonZeroU64::new(5)),
            opts.clone().max_entries(NonZeroU64::new(1)),
            opts.clone().max_chunk_references(NonZeroU64::new(1)),
            opts.clone()
                .max_metadata_bytes(NonZeroU64::new(report.node_metadata_bytes - 1)),
        ] {
            assert!(backup_inventory(&src, &ConfigFile::default(), &limits).is_err());
        }
        assert!(
            backup_inventory(
                &src,
                &ConfigFile::default(),
                &opts.max_metadata_bytes(NonZeroU64::new(report.node_metadata_bytes))
            )
            .is_ok()
        );
    }

    #[test]
    fn incomplete_unknown_size_overflow_and_invalid_chunker_are_errors() {
        let opts = BackupAdmissionOptions::default();
        let config = ConfigFile::default();
        assert!(
            backup_inventory(
                &MetadataSource {
                    sizes: vec![3],
                    fail: true
                },
                &config,
                &opts
            )
            .is_err()
        );
        assert!(
            backup_inventory(&crate::StdinSource::new("stdin".into()), &config, &opts).is_err()
        );
        assert!(
            backup_inventory(
                &MetadataSource {
                    sizes: vec![u64::MAX, 1],
                    fail: false
                },
                &config,
                &opts
            )
            .is_err()
        );
        let invalid_config = ConfigFile {
            chunk_min_size: Some(0),
            ..ConfigFile::default()
        };
        assert!(
            backup_inventory(
                &MetadataSource {
                    sizes: vec![],
                    fail: false
                },
                &invalid_config,
                &opts
            )
            .is_err()
        );
        assert!(
            check_deadline(
                Instant::now().checked_sub(Duration::from_secs(2)).unwrap(),
                NonZeroU64::new(1)
            )
            .is_err()
        );
        assert!(serde_json::from_str::<BackupAdmissionOptions>(r#"{"max-entries":0}"#).is_err());
    }

    #[test]
    fn fixed_size_counts_partial_chunks_per_file() {
        let config = ConfigFile {
            chunker: Some(Chunker::FixedSize),
            chunk_size: Some(10),
            ..ConfigFile::default()
        };
        let result = backup_inventory(
            &MetadataSource {
                sizes: vec![10, 11, 1, 0],
                fail: false,
            },
            &config,
            &BackupAdmissionOptions::default(),
        )
        .unwrap();
        assert_eq!(result.chunk_references_bound, 4);
    }

    #[cfg(unix)]
    #[test]
    fn local_inventory_respects_filters_links_and_path_limits() {
        use std::{fs, os::unix::fs::symlink};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();
        fs::create_dir(path.join("dir")).unwrap();
        fs::File::create(path.join("dir/sparse"))
            .unwrap()
            .set_len(1 << 30)
            .unwrap();
        fs::hard_link(path.join("dir/sparse"), path.join("alias")).unwrap();
        fs::write(path.join("omit"), b"ignored").unwrap();
        symlink("/does-not-exist", path.join("link")).unwrap();
        let excludes = crate::Excludes::default().globs(vec!["!omit".to_string()]);
        let source = crate::LocalSource::new(
            crate::LocalSourceSaveOptions::default(),
            &excludes,
            &crate::LocalSourceFilterOptions::default(),
            &[path],
        )
        .unwrap();
        let report = backup_inventory(
            &source,
            &ConfigFile::default(),
            &BackupAdmissionOptions::default(),
        )
        .unwrap();
        assert_eq!(report.entries, 4);
        assert_eq!(report.files, 2);
        assert_eq!(report.apparent_bytes, 2 << 30);
        let limited = BackupAdmissionOptions::default().max_path_depth(NonZeroU64::new(1));
        assert!(backup_inventory(&source, &ConfigFile::default(), &limited).is_err());
    }
}
