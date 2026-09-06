use std::io::Read;

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
        })
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
                let (node, filesize) = if matches!(parent, ParentResult::Matched(())) {
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
