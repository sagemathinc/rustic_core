use std::{
    collections::HashMap,
    fs::File,
    io::Read,
    path::Path,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
};

use jiff::Timestamp;

use crate::{
    archiver::{
        extent_reuse::{ExtentReuse, SeedTurn},
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
    /// Content by extent signature (`--reuse-extents`), if enabled and pinned.
    extents: Option<Arc<ExtentReuse>>,
}

/// A file opened for `--reuse-extents`, with its signature.
struct Signed<'a> {
    extents: &'a ExtentReuse,
    /// The file (a duplicate of the reader's descriptor), for the post-read scan.
    file: File,
    signature: Vec<u8>,
    /// Parent seeds numbered below this may serve the file.
    before: u64,
}

/// The identity of an inode in this run: device and inode number.
type InodeKey = (u64, u64);

/// What a hard-linked file looked like when its content was archived. A path
/// of the same inode reuses that content only if it looks exactly the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HardlinkState {
    size: u64,
    mtime: Timestamp,
    ctime: Timestamp,
}

/// One key in this run: its archived content, or the read in progress.
#[derive(Debug)]
enum Slot<S> {
    Done(S, Vec<DataId>),
    Reading(S, Arc<Pending>),
}

/// The state of a read that other paths wait for.
#[derive(Debug, Default)]
enum Outcome {
    #[default]
    Reading,
    /// The archived content, or `None` if the read failed.
    Finished(Option<Vec<DataId>>),
}

/// A read in progress that other paths of the same inode wait for.
#[derive(Debug, Default)]
struct Pending {
    outcome: Mutex<Outcome>,
    finished: Condvar,
}

impl Pending {
    fn wait(&self) -> Option<Vec<DataId>> {
        let mut outcome = self.outcome.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Outcome::Finished(content) = &*outcome {
                return content.clone();
            }
            outcome = self
                .finished
                .wait(outcome)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    fn finish(&self, content: Option<Vec<DataId>>) {
        *self.outcome.lock().unwrap_or_else(PoisonError::into_inner) = Outcome::Finished(content);
        self.finished.notify_all();
    }
}

/// What a worker does with a file (see [`ContentCache::claim_key`]).
#[derive(Debug)]
pub(crate) enum Claim<'c, K: CacheKey, S: CacheState> {
    /// Read the file, as without the cache.
    Read,
    /// Use this content: the same key and state, archived in this run.
    Reuse(Vec<DataId>),
    /// Read the file for every path with this key; others wait for it.
    Own(OwnedRead<'c, K, S>),
}

/// The duty to read a file for all paths with its key. Finishing it publishes
/// the content; dropping it unfinished (an error, a panic) lets the waiting
/// paths read the file themselves, and nothing is cached.
#[derive(Debug)]
pub(crate) struct OwnedRead<'c, K: CacheKey, S: CacheState> {
    cache: &'c ContentCache<K, S>,
    key: Option<K>,
    state: S,
    pending: Arc<Pending>,
}

impl<K: CacheKey, S: CacheState> OwnedRead<'_, K, S> {
    /// Publish the content (or, with `None`, withdraw the read).
    pub(crate) fn finish(mut self, content: Option<&Vec<DataId>>) {
        if let Some(key) = self.key.take() {
            self.cache
                .finish(key, &self.state, &self.pending, content.cloned());
        }
    }
}

impl<K: CacheKey, S: CacheState> Drop for OwnedRead<'_, K, S> {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.cache.finish(key, &self.state, &self.pending, None);
        }
    }
}

/// A cache key: an inode, or an exact extent signature.
pub(crate) trait CacheKey: std::hash::Hash + Eq + Clone + std::fmt::Debug {}
impl<T: std::hash::Hash + Eq + Clone + std::fmt::Debug> CacheKey for T {}

/// What must also be equal for a key's content to be reused.
pub(crate) trait CacheState: Eq + Clone + std::fmt::Debug {}
impl<T: Eq + Clone + std::fmt::Debug> CacheState for T {}

/// Content archived in this run, by key, so that each key is read once: one
/// path reads it, the others wait and reuse.
#[derive(Debug)]
pub(crate) struct ContentCache<K, S> {
    slots: Mutex<HashMap<K, Slot<S>>>,
}

impl<K, S> Default for ContentCache<K, S> {
    fn default() -> Self {
        Self {
            slots: Mutex::default(),
        }
    }
}

/// Content of multiply hard-linked files, by inode.
pub(crate) type HardlinkCache = ContentCache<InodeKey, HardlinkState>;

/// Content of files by extent signature (`--reuse-extents`). The signature is
/// exact; the state says where the content came from.
pub(crate) type ExtentCache = ContentCache<Vec<u8>, ExtentOrigin>;

/// Where content cached under a signature came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtentOrigin {
    /// A file read in this run: it serves every file.
    Read,
    /// The parent match with this seed ticket: it serves only the files after
    /// it in the tree, so that whether a file is read never depends on timing.
    Seed(u64),
}

impl ExtentOrigin {
    /// Whether this content may serve a file that comes after the seeds
    /// numbered below `before`.
    pub(crate) const fn serves(self, before: u64) -> bool {
        match self {
            Self::Read => true,
            Self::Seed(ticket) => ticket < before,
        }
    }
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

    /// Decide how to get this node's content (see [`ContentCache::claim_key`]).
    pub(crate) fn claim(&self, node: &Node) -> Claim<'_, InodeKey, HardlinkState> {
        Self::key_and_state(node).map_or(Claim::Read, |(key, state)| {
            let serves = |seen: &HardlinkState| *seen == state;
            self.claim_key(key, state.clone(), serves)
        })
    }

    /// Remember content known without reading: a file unchanged since its
    /// parent snapshot.
    pub(crate) fn seed(&self, node: &Node) {
        if let (Some((key, state)), Some(content)) = (Self::key_and_state(node), &node.content) {
            self.seed_key(key, state, content);
        }
    }
}

impl<K: CacheKey, S: CacheState> ContentCache<K, S> {
    fn slots(&self) -> MutexGuard<'_, HashMap<K, Slot<S>>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Decide how to get a file's content: reuse content whose state `serves`
    /// this file, wait for the path reading it now (never holding the map's
    /// lock while waiting), or read it for all paths with this key, as
    /// `state`.
    pub(crate) fn claim_key(
        &self,
        key: K,
        state: S,
        serves: impl Fn(&S) -> bool,
    ) -> Claim<'_, K, S> {
        let mut slots = self.slots();
        let pending = match slots.get(&key) {
            Some(Slot::Done(seen, content)) if serves(seen) => {
                let content = content.clone();
                drop(slots);
                return Claim::Reuse(content);
            }
            Some(Slot::Reading(seen, pending)) if serves(seen) => {
                let pending = Arc::clone(pending);
                drop(slots);
                pending
            }
            _ => {
                let pending = Arc::new(Pending::default());
                _ = slots.insert(
                    key.clone(),
                    Slot::Reading(state.clone(), Arc::clone(&pending)),
                );
                drop(slots);
                return Claim::Own(OwnedRead {
                    cache: self,
                    key: Some(key),
                    state,
                    pending,
                });
            }
        };
        pending.wait().map_or(Claim::Read, Claim::Reuse)
    }

    /// Publish (or, on failure, withdraw) an owned read, if the slot is still
    /// that read's, and wake its waiters.
    fn finish(&self, key: K, state: &S, pending: &Arc<Pending>, content: Option<Vec<DataId>>) {
        {
            let mut slots = self.slots();
            if let Some(Slot::Reading(_, current)) = slots.get(&key)
                && Arc::ptr_eq(current, pending)
            {
                match &content {
                    Some(content) => {
                        _ = slots.insert(key, Slot::Done(state.clone(), content.clone()));
                    }
                    None => {
                        _ = slots.remove(&key);
                    }
                }
            }
        }
        pending.finish(content);
    }

    /// Remember content known without reading (a file unchanged since its
    /// parent snapshot), unless a read of the key is in progress.
    pub(crate) fn seed_key(&self, key: K, state: S, content: &[DataId]) {
        let mut slots = self.slots();
        if !matches!(slots.get(&key), Some(Slot::Reading(..))) {
            _ = slots.insert(key, Slot::Done(state, content.to_vec()));
        }
    }

    /// Like [`Self::seed_key`], but only for a key not seen yet in this run:
    /// what a file got must not change for the files after it.
    pub(crate) fn seed_new_key(&self, key: K, state: S, content: &[DataId]) {
        _ = self
            .slots()
            .entry(key)
            .or_insert_with(|| Slot::Done(state, content.to_vec()));
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
            extents: None,
        })
    }

    /// Read each multiply hard-linked file only once in this run.
    pub(crate) fn reuse_hardlinks(&mut self, reuse: bool) {
        self.hardlinks = reuse.then(|| Arc::new(HardlinkCache::default()));
    }

    /// The hard-link cache, if enabled (seeded from parent matches).
    pub(crate) fn hardlink_cache(&self) -> Option<Arc<HardlinkCache>> {
        self.hardlinks.clone()
    }

    /// Reuse content by extent signature in this run.
    pub(crate) fn reuse_extents(&mut self, extents: Option<Arc<ExtentReuse>>) {
        self.extents = extents;
    }

    /// The extent reuse state, if enabled and pinned.
    pub(crate) fn extent_reuse(&self) -> Option<Arc<ExtentReuse>> {
        self.extents.clone()
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
    /// * `turn` - Its place in the order of extent cache seeds.
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
        turn: SeedTurn,
        p: &Progress,
    ) -> RusticResult<TreeItem> {
        Ok(match item {
            TreeType::NewTree(item) => TreeType::NewTree(item),
            TreeType::EndTree => TreeType::EndTree,
            TreeType::Other((path, node, (open, parent))) => {
                let (node, filesize) = if matches!(parent, ParentResult::Matched(())) {
                    if let (Some(extents), SeedTurn::Seed(seed)) = (&self.extents, turn) {
                        extents.seed(open, node.content.as_deref(), seed.ticket());
                        drop(seed);
                    }
                    let size = node.meta.size;
                    p.inc(size);
                    (node, size)
                } else if node.node_type == NodeType::File {
                    let claim = self
                        .hardlinks
                        .as_deref()
                        .map_or(Claim::Read, |cache| cache.claim(&node));
                    match claim {
                        Claim::Reuse(content) => {
                            // Another path of this inode, unchanged since its
                            // content was archived in this run.
                            let mut node = node;
                            node.content = Some(content);
                            let size = node.meta.size;
                            p.inc(size);
                            (node, size)
                        }
                        Claim::Read => self.read_file(&path, open, node, &turn, p)?,
                        Claim::Own(owned) => {
                            let result = self.read_file(&path, open, node, &turn, p);
                            owned.finish(
                                result
                                    .as_ref()
                                    .ok()
                                    .and_then(|(node, _)| node.content.as_ref()),
                            );
                            result?
                        }
                    }
                } else {
                    (node, 0)
                };
                TreeType::Other((path, node, (parent, filesize)))
            }
        })
    }

    /// Open and archive a regular file, or reuse the content of a file with
    /// the same extents.
    fn read_file<O: ReadSourceOpen>(
        &self,
        path: &Path,
        open: Option<O>,
        node: Node,
        turn: &SeedTurn,
        p: &Progress,
    ) -> RusticResult<(Node, u64)> {
        if let Some(extents) = &self.extents {
            extents.wait_for(turn);
        }
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
        let signed = self.extents.as_deref().and_then(|extents| {
            let file = r.as_file()?.try_clone().ok()?;
            let signature = extents.signature(&file)?;
            Some(Signed {
                extents,
                file,
                signature,
                before: turn.before(),
            })
        });
        let result = match signed {
            Some(signed) => self.read_by_signature(&signed, r, node, p),
            None => self.backup_reader(r, node, p),
        };
        result.map_err(|err| {
            err.prepend_guidance_line("Error while backing up `{path}`")
                .attach_context("path", path.display().to_string())
        })
    }

    /// Reuse the content of a file with the same signature, wait for the path
    /// reading it now, or read the file and publish its content for others,
    /// after a post-read scan shows the extents unchanged.
    fn read_by_signature(
        &self,
        signed: &Signed<'_>,
        r: SourceReader<impl Read + Send + 'static>,
        node: Node,
        p: &Progress,
    ) -> RusticResult<(Node, u64)> {
        let Signed {
            extents,
            file,
            signature,
            before,
        } = signed;
        let serves = |seen: &ExtentOrigin| seen.serves(*before);
        match extents
            .cache
            .claim_key(signature.clone(), ExtentOrigin::Read, serves)
        {
            Claim::Reuse(content) => {
                let mut node = node;
                node.content = Some(content);
                let size = node.meta.size;
                extents.reused(size);
                p.inc(size);
                Ok((node, size))
            }
            Claim::Read => self.backup_reader(r, node, p),
            Claim::Own(owned) => {
                let result = self.backup_reader(r, node, p);
                owned.finish(
                    result
                        .as_ref()
                        .ok()
                        .and_then(|(node, _)| node.content.as_ref())
                        .filter(|_| extents.still(file, signature)),
                );
                result
            }
        }
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
    use std::{ffi::OsStr, sync::mpsc, thread, time::Duration};

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

    fn content(bytes: &[u8]) -> Vec<DataId> {
        vec![DataId::from(hash(bytes))]
    }

    fn own<K: CacheKey, S: CacheState>(claim: Claim<'_, K, S>) -> OwnedRead<'_, K, S> {
        match claim {
            Claim::Own(owned) => owned,
            other => panic!("expected to own the read, got {other:?}"),
        }
    }

    #[test]
    fn the_first_path_reads_and_later_paths_reuse() {
        let cache = HardlinkCache::default();
        own(cache.claim(&file(2, 42, 3, 100, 200))).finish(Some(&content(b"abc")));
        assert!(matches!(
            cache.claim(&file(2, 42, 3, 100, 200)),
            Claim::Reuse(c) if c == content(b"abc")
        ));
    }

    #[test]
    fn a_path_waits_for_the_read_in_progress_and_reuses_it() {
        let cache = Arc::new(HardlinkCache::default());
        let owned = own(cache.claim(&file(2, 42, 3, 100, 200)));
        let (sender, receiver) = mpsc::channel();
        let waiter = {
            let cache = Arc::clone(&cache);
            thread::spawn(move || {
                let claim = cache.claim(&file(2, 42, 3, 100, 200));
                let result = match claim {
                    Claim::Reuse(c) => Some(c),
                    _ => None,
                };
                sender.send(result).unwrap();
            })
        };
        // the second path does not read while the first one is reading
        assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
        owned.finish(Some(&content(b"abc")));
        assert_eq!(receiver.recv().unwrap(), Some(content(b"abc")));
        waiter.join().unwrap();
    }

    #[test]
    fn a_failed_read_lets_waiters_read_and_caches_nothing() {
        let cache = Arc::new(HardlinkCache::default());
        let owned = own(cache.claim(&file(2, 42, 3, 100, 200)));
        let waiter = {
            let cache = Arc::clone(&cache);
            thread::spawn(move || matches!(cache.claim(&file(2, 42, 3, 100, 200)), Claim::Read))
        };
        thread::sleep(Duration::from_millis(100));
        drop(owned); // an error or a panic before finishing
        assert!(waiter.join().unwrap(), "the waiter reads the file itself");
        // and a later path owns a fresh read rather than reusing anything
        let again = own(cache.claim(&file(2, 42, 3, 100, 200)));
        again.finish(None);
        assert!(matches!(
            cache.claim(&file(2, 42, 3, 100, 200)),
            Claim::Own(_)
        ));
    }

    #[test]
    fn any_difference_reads_the_file() {
        let cache = HardlinkCache::default();
        own(cache.claim(&file(2, 42, 3, 100, 200))).finish(Some(&content(b"abc")));
        for other in [
            file(2, 43, 3, 100, 200), // another inode
            file(2, 42, 4, 100, 200), // size changed
            file(2, 42, 3, 101, 200), // mtime changed
            file(2, 42, 3, 100, 201), // ctime changed
        ] {
            let owned = own(cache.claim(&other));
            owned.finish(None);
        }
        let mut other_device = file(2, 42, 3, 100, 200);
        other_device.meta.device_id = 8;
        assert!(matches!(cache.claim(&other_device), Claim::Own(_)));
    }

    #[test]
    fn only_multiply_linked_regular_files_with_known_identity_take_part() {
        let cache = HardlinkCache::default();
        let mut no_ctime = file(2, 42, 3, 100, 200);
        no_ctime.meta.ctime = None;
        let mut dir = file(2, 44, 3, 100, 200);
        dir.node_type = NodeType::Dir;
        for node in [
            file(1, 42, 3, 100, 200),
            file(2, 0, 3, 100, 200),
            no_ctime,
            dir,
        ] {
            assert!(matches!(cache.claim(&node), Claim::Read));
        }
    }

    #[test]
    fn a_parent_match_seeds_the_cache() {
        let cache = HardlinkCache::default();
        let mut unchanged = file(2, 42, 3, 100, 200);
        unchanged.content = Some(content(b"abc"));
        cache.seed(&unchanged);
        assert!(matches!(
            cache.claim(&file(2, 42, 3, 100, 200)),
            Claim::Reuse(c) if c == content(b"abc")
        ));
        // a node without content (nothing archived) seeds nothing
        cache.seed(&file(2, 45, 3, 100, 200));
        assert!(matches!(
            cache.claim(&file(2, 45, 3, 100, 200)),
            Claim::Own(_)
        ));
    }

    #[test]
    fn a_seed_serves_only_the_files_after_it() {
        let cache = ExtentCache::default();
        let sig = b"sig".to_vec();
        cache.seed_new_key(sig.clone(), ExtentOrigin::Seed(3), &content(b"abc"));
        let claim = |before: u64| {
            cache.claim_key(sig.clone(), ExtentOrigin::Read, |seen: &ExtentOrigin| {
                seen.serves(before)
            })
        };
        // a file after seed 3 reuses its content
        assert!(matches!(claim(4), Claim::Reuse(c) if c == content(b"abc")));
        // a file before it reads, and then serves everyone
        own(claim(3)).finish(Some(&content(b"abc")));
        assert!(matches!(claim(0), Claim::Reuse(c) if c == content(b"abc")));
    }

    #[test]
    fn a_seed_never_replaces_what_files_already_got() {
        let cache = ExtentCache::default();
        let sig = b"sig".to_vec();
        let serves_all = |seen: &ExtentOrigin| seen.serves(u64::MAX);
        own(cache.claim_key(sig.clone(), ExtentOrigin::Read, serves_all))
            .finish(Some(&content(b"read")));
        cache.seed_new_key(sig.clone(), ExtentOrigin::Seed(0), &content(b"seed"));
        assert!(matches!(
            cache.claim_key(sig, ExtentOrigin::Read, serves_all),
            Claim::Reuse(c) if c == content(b"read")
        ));
    }
}
