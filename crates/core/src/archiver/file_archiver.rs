use std::{
    collections::HashMap,
    io::Read,
    sync::{Arc, Mutex},
};

use jiff::Timestamp;

use crate::{
    archiver::{
        parent::{ItemWithParent, ParentResult},
        tree::TreeType,
        tree_archiver::TreeItem,
    },
    backend::{
        ReadSourceOpen,
        decrypt::DecryptWriteBackend,
        node::{Node, NodeType},
        source_reader::SourceReader,
    },
    blob::{
        BlobId, BlobType, DataId,
        packer::{PackSizer, Packer, PackerStats},
    },
    chunker::{Chunk, ChunkIter},
    crypto::hasher::hash,
    error::{ErrorKind, RusticError, RusticResult},
    index::{ReadGlobalIndex, indexer::SharedIndexer},
    progress::Progress,
    repofile::configfile::ConfigFile,
};

/// The `FileArchiver` is responsible for archiving files.
/// It will read the file, chunk it, and write the chunks to the backend.
///
/// # Type Parameters
///
/// * `BE` - The backend type.
/// * `I` - The index to read from.
#[derive(Clone)]
pub(crate) struct FileArchiver<'a, BE: DecryptWriteBackend, I: ReadGlobalIndex> {
    index: &'a I,
    data_packer: Packer<BE>,
    config: ConfigFile,
    /// Content of multiply hard-linked files archived in this run, if enabled.
    hardlinks: Option<Arc<HardlinkCache>>,
}

/// The identity of an inode in this run: device and inode number.
type InodeKey = (u64, u64);

/// What a hard-linked file looked like when its content was archived. A path
/// of the same inode reuses that content only if it looks exactly the same.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HardlinkState {
    size: u64,
    mtime: Timestamp,
    ctime: Timestamp,
}

/// Content of multiply hard-linked files archived in this run.
#[derive(Debug, Default)]
pub(crate) struct HardlinkCache {
    entries: Mutex<HashMap<InodeKey, (HardlinkState, Vec<DataId>)>>,
}

impl HardlinkCache {
    /// The key and state of a node that may share its content with other paths:
    /// a regular file with more than one link, a known inode and both times.
    fn key_and_state(node: &Node) -> Option<(InodeKey, HardlinkState)> {
        let meta = &node.meta;
        if node.node_type != NodeType::File || meta.links < 2 || meta.inode == 0 {
            return None;
        }
        Some((
            (meta.device_id, meta.inode),
            HardlinkState {
                size: meta.size,
                mtime: meta.mtime?,
                ctime: meta.ctime?,
            },
        ))
    }

    /// The content archived in this run for this node's inode, if the node
    /// looks exactly as it did then.
    fn get(&self, node: &Node) -> Option<Vec<DataId>> {
        let (key, state) = Self::key_and_state(node)?;
        let entries = self.entries.lock().ok()?;
        entries
            .get(&key)
            .filter(|(seen, _)| *seen == state)
            .map(|(_, content)| content.clone())
    }

    /// Remember the content archived for this node's inode.
    fn insert(&self, node: &Node) {
        let (Some((key, state)), Some(content)) = (Self::key_and_state(node), &node.content) else {
            return;
        };
        if let Ok(mut entries) = self.entries.lock() {
            _ = entries.insert(key, (state, content.clone()));
        }
    }
}

impl<'a, BE: DecryptWriteBackend, I: ReadGlobalIndex> FileArchiver<'a, BE, I> {
    /// Creates a new `FileArchiver`.
    ///
    /// # Type Parameters
    ///
    /// * `BE` - The backend type.
    /// * `I` - The index to read from.
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to write to.
    /// * `index` - The index to read from.
    /// * `indexer` - The indexer to write to.
    /// * `config` - The config file.
    ///
    /// # Errors
    ///
    /// * If sending the message to the raw packer fails.
    /// * If converting the data length to u64 fails
    pub(crate) fn new(
        be: BE,
        index: &'a I,
        indexer: SharedIndexer<BE>,
        config: &ConfigFile,
    ) -> RusticResult<Self> {
        let pack_sizer =
            PackSizer::from_config(config, BlobType::Data, index.total_size(BlobType::Data));
        let data_packer = Packer::new(be, BlobType::Data, indexer, pack_sizer)?;

        Ok(Self {
            index,
            data_packer,
            config: config.clone(),
            hardlinks: None,
        })
    }

    /// Read each multiply hard-linked file only once in this run.
    pub(crate) fn reuse_hardlinks(&mut self, reuse: bool) {
        self.hardlinks = reuse.then(|| Arc::new(HardlinkCache::default()));
    }

    /// Processes the given item.
    ///
    /// # Type Parameters
    ///
    /// * `O` - The type of the tree item.
    ///
    /// # Arguments
    ///
    /// * `item` - The item to process.
    /// * `p` - The progress tracker.
    ///
    /// # Errors
    ///
    /// * If the item could not be unpacked.
    ///
    /// # Returns
    ///
    /// The processed item.
    pub(crate) fn process<O: ReadSourceOpen>(
        &self,
        item: ItemWithParent<Option<O>>,
        p: &Progress,
    ) -> RusticResult<TreeItem> {
        Ok(match item {
            TreeType::NewTree(item) => TreeType::NewTree(item),
            TreeType::EndTree => TreeType::EndTree,
            TreeType::Other((path, node, (open, parent))) => {
                let reused = if matches!(parent, ParentResult::Matched(())) {
                    None
                } else {
                    self.hardlinks.as_ref().and_then(|cache| cache.get(&node))
                };
                let (node, filesize) = if matches!(parent, ParentResult::Matched(())) {
                    let size = node.meta.size;
                    p.inc(size);
                    (node, size)
                } else if let Some(content) = reused {
                    // Another path of this inode, unchanged since its content
                    // was archived in this run.
                    let mut node = node;
                    node.content = Some(content);
                    let size = node.meta.size;
                    p.inc(size);
                    (node, size)
                } else if node.node_type == NodeType::File {
                    let r = open
                        .ok_or_else(
                            || RusticError::new(
                                ErrorKind::Internal,
                                "Failed to unpack tree type optional at `{path}`. Option should contain a value, but contained `None`.",
                            )
                            .attach_context("path", path.display().to_string())
                            .ask_report(),
                        )?
                        .open_with_holes()
                        .map_err(|err| {
                            err
                            .overwrite_kind(ErrorKind::InputOutput)
                            .prepend_guidance_line("Failed to open ReadSourceOpen at `{path}`")
                            .attach_context("path", path.display().to_string())
                        })?;

                    self.backup_reader(r, node, p).map_err(|err| {
                        err.prepend_guidance_line("Error while backing up `{path}`")
                            .attach_context("path", path.display().to_string())
                    })?
                } else {
                    (node, 0)
                };
                if let Some(cache) = &self.hardlinks {
                    cache.insert(&node);
                }
                TreeType::Other((path, node, (parent, filesize)))
            }
        })
    }

    // TODO: add documentation!
    fn backup_reader(
        &self,
        r: SourceReader<impl Read + Send + 'static>,
        node: Node,
        p: &Progress,
    ) -> RusticResult<(Node, u64)> {
        let mut zero_chunk = None;
        let mut content = Vec::new();
        let mut filesize = 0u64;
        for chunk in ChunkIter::from_config(
            &self.config,
            r,
            usize::try_from(node.meta.size).unwrap_or(usize::MAX),
        )? {
            let (id, size) = match chunk? {
                Chunk::Data(chunk) => {
                    let id = hash(&chunk);
                    let size = chunk.len() as u64;
                    if !self.index.has_data(&DataId::from(id)) {
                        self.data_packer.add(chunk.into(), BlobId::from(id))?;
                    }
                    (DataId::from(id), size)
                }
                Chunk::Zeros(size) => {
                    let id = match zero_chunk {
                        Some((cached_size, id)) if cached_size == size => id,
                        _ => {
                            let chunk = vec![0; size];
                            let id = hash(&chunk);
                            if !self.index.has_data(&DataId::from(id)) {
                                self.data_packer.add(chunk.into(), BlobId::from(id))?;
                            }
                            zero_chunk = Some((size, DataId::from(id)));
                            DataId::from(id)
                        }
                    };
                    (id, size as u64)
                }
            };
            p.inc(size);
            filesize = filesize.checked_add(size).ok_or_else(|| {
                RusticError::new(ErrorKind::InputOutput, "Backup file size overflow.")
            })?;
            content.push(id);
        }

        let mut node = node;
        node.content = Some(content);
        Ok((node, filesize))
    }

    /// Finalizes the archiver.
    ///
    /// # Returns
    ///
    /// The statistics of the archiver.
    ///
    /// # Panics
    ///
    /// * If the channel could not be dropped
    pub(crate) fn finalize(self) -> RusticResult<PackerStats> {
        self.data_packer.finalize()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;
    use crate::backend::node::Metadata;

    fn file(links: u64, inode: u64, size: u64, mtime: i64, ctime: i64) -> Node {
        Node::new_node(
            OsStr::new("f"),
            NodeType::File,
            Metadata {
                links,
                inode,
                device_id: 7,
                size,
                mtime: Some(Timestamp::from_second(mtime).unwrap()),
                ctime: Some(Timestamp::from_second(ctime).unwrap()),
                ..Metadata::default()
            },
        )
    }

    fn archived(mut node: Node, content: &[u8]) -> Node {
        node.content = Some(vec![DataId::from(hash(content))]);
        node
    }

    #[test]
    fn another_path_of_an_unchanged_inode_reuses_its_content() {
        let cache = HardlinkCache::default();
        cache.insert(&archived(file(2, 42, 3, 100, 200), b"abc"));
        assert_eq!(
            cache.get(&file(2, 42, 3, 100, 200)),
            Some(vec![DataId::from(hash(b"abc"))])
        );
    }

    #[test]
    fn any_difference_reads_the_file() {
        let cache = HardlinkCache::default();
        cache.insert(&archived(file(2, 42, 3, 100, 200), b"abc"));
        for other in [
            file(2, 43, 3, 100, 200), // another inode
            file(2, 42, 4, 100, 200), // size changed
            file(2, 42, 3, 101, 200), // mtime changed
            file(2, 42, 3, 100, 201), // ctime changed
        ] {
            assert_eq!(cache.get(&other), None);
        }
        let mut other_device = file(2, 42, 3, 100, 200);
        other_device.meta.device_id = 8;
        assert_eq!(cache.get(&other_device), None);
    }

    #[test]
    fn only_multiply_linked_regular_files_with_known_identity_take_part() {
        let cache = HardlinkCache::default();
        for node in [
            file(1, 42, 3, 100, 200), // a single link
            file(2, 0, 3, 100, 200),  // unknown inode
        ] {
            cache.insert(&archived(node.clone(), b"abc"));
            assert_eq!(cache.get(&node), None);
        }
        let mut no_ctime = file(2, 42, 3, 100, 200);
        no_ctime.meta.ctime = None;
        cache.insert(&archived(no_ctime.clone(), b"abc"));
        assert_eq!(cache.get(&no_ctime), None);
        let mut dir = file(2, 44, 3, 100, 200);
        dir.node_type = NodeType::Dir;
        cache.insert(&archived(dir.clone(), b"abc"));
        assert_eq!(cache.get(&dir), None);
        // a node without content (not archived) is not remembered
        cache.insert(&file(2, 45, 3, 100, 200));
        assert_eq!(cache.get(&file(2, 45, 3, 100, 200)), None);
    }
}
