use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
};

use jiff::Timestamp;
use log::{info, warn};
use smallvec::SmallVec;

use crate::{
    archiver::{TreeStackEmptyError, tree::TreeType},
    backend::{decrypt::DecryptReadBackend, node::Node},
    blob::tree::{Tree, TreeId},
    index::ReadGlobalIndex,
};

/// The `ItemWithParent` is a `TreeType` wrapping the result of a parent search and a type `O`.
///
/// # Type Parameters
///
/// * `O` - The type of the `TreeType`.
pub(crate) type ItemWithParent<O> = TreeType<(O, ParentResult<()>), ParentResult<TreeId>>;

/// The `Parent` is responsible for finding the parent tree of a given tree.
#[derive(Debug)]
pub struct Parent {
    /// The tree id of the parent tree.
    tree_ids: Vec<TreeId>,
    /// The parent trees of the current directory.
    trees: Vec<ParentTree>,
    /// The stack of parent trees, with whether that level was below a
    /// filesystem boundary.
    stack: Vec<(Vec<ParentTree>, bool)>,
    /// The current directory is on another filesystem than the backup root,
    /// or below such a directory: nothing from the parent applies.
    below_boundary: bool,
    /// Ignore ctime when comparing nodes.
    ignore_ctime: bool,
    /// Ignore inode number when comparing nodes.
    ignore_inode: bool,
    /// The source was restored from the parent: files with a ctime before
    /// this cutoff were written by the restore (see
    /// `ParentOptions::restored_before`).
    restored_before: Option<Timestamp>,
    /// Find a moved directory's parent trees by its inode (see
    /// `ParentOptions::detect_moves`), with at most this many entries
    /// (directories indexed plus trees visited) in the directory index;
    /// above it, move detection is off.
    detect_moves: Option<usize>,
    /// The parent trees' directories by inode, built on first use (empty if
    /// it would have exceeded `detect_moves_max_entries`).
    dir_inodes: Option<HashMap<u64, SmallVec<[TreeId; 1]>>>,
    /// What the last file processed was vouched for by its parent match.
    last_vouch: Vouch,
}

/// A parent tree of the current directory, with the position reached in it.
#[derive(Debug)]
struct ParentTree {
    tree: Tree,
    idx: usize,
    /// Found by a directory's inode, not at its path (or below such a tree):
    /// its nodes match only with full identity (see [`Parent::is_parent`]).
    moved: bool,
}

/// What a parent match vouches for, beyond its own path (ordered: an inode
/// vouch includes a content vouch).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Vouch {
    /// Nothing: trusted for this path only (or no match).
    #[default]
    Nothing,
    /// The file's content (see [`Parent::vouches_for_content`]).
    Content,
    /// The content of every path of its inode (see [`Parent::vouches_for_inode`]).
    Inode,
}

/// The result of a parent search.
///
/// # Type Parameters
///
/// * `T` - The type of the matched parent.
#[derive(Clone, Debug)]
pub(crate) enum ParentResult<T> {
    /// The parent was found and matches.
    Matched(T),
    /// The parent was not found.
    NotFound,
    /// The parent was found but doesn't match.
    NotMatched,
}

impl<T> ParentResult<T> {
    /// Maps a `ParentResult<T>` to a `ParentResult<R>` by applying a function to a contained value.
    ///
    /// # Type Parameters
    ///
    /// * `R` - The type of the returned `ParentResult`.
    ///
    /// # Arguments
    ///
    /// * `f` - The function to apply.
    ///
    /// # Returns
    ///
    /// A `ParentResult<R>` with the result of the function for each `ParentResult<T>`.
    fn map<R>(self, f: impl FnOnce(T) -> R) -> ParentResult<R> {
        match self {
            Self::Matched(t) => ParentResult::Matched(f(t)),
            Self::NotFound => ParentResult::NotFound,
            Self::NotMatched => ParentResult::NotMatched,
        }
    }
}

impl Parent {
    /// Creates a new `Parent`.
    ///
    /// # Type Parameters
    ///
    /// * `BE` - The type of the backend.
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to read from.
    /// * `tree_id` - The tree id of the parent tree.
    /// * `ignore_ctime` - Ignore ctime when comparing nodes.
    /// * `ignore_inode` - Ignore inode number when comparing nodes.
    /// * `restored_before` - The source was restored from this parent, and
    ///   files with a ctime before this cutoff were written by the restore.
    /// * `detect_moves` - Find a moved directory's parent trees by its inode,
    ///   with at most this many entries in the directory index.
    pub(crate) fn new(
        be: &impl DecryptReadBackend,
        index: &impl ReadGlobalIndex,
        tree_id: impl IntoIterator<Item = TreeId>,
        ignore_ctime: bool,
        ignore_inode: bool,
        restored_before: Option<Timestamp>,
        detect_moves: Option<usize>,
    ) -> Self {
        // if tree_id is given, try to load tree from backend.
        let (trees, tree_ids) = tree_id
            .into_iter()
            .filter_map(|tree_id| match Tree::from_backend(be, index, tree_id) {
                Ok(tree) => Some((
                    ParentTree {
                        tree,
                        idx: 0,
                        moved: false,
                    },
                    tree_id,
                )),
                Err(err) => {
                    warn!(
                        "ignoring error when loading parent tree {tree_id:?}: {}",
                        err.display_log()
                    );
                    None
                }
            })
            .unzip();

        Self {
            tree_ids,
            trees,
            stack: Vec::new(),
            below_boundary: false,
            ignore_ctime,
            ignore_inode,
            restored_before,
            detect_moves,
            dir_inodes: None,
            last_vouch: Vouch::Nothing,
        }
    }

    /// Whether `node` was last changed by the restore it came from: its ctime
    /// is present and strictly before the cutoff [`Parent::restored_before`].
    /// Such a file still holds what the restore wrote, i.e. the parent's
    /// content at its path, so its ctime and inode (new on the restored tree)
    /// are not compared with the parent's. Any later change moves its ctime
    /// past the cutoff.
    fn restored_unchanged(&self, node: &Node) -> bool {
        self.restored_before
            .zip(node.meta.ctime)
            .is_some_and(|(restored_before, ctime)| ctime < restored_before)
    }

    /// Maps the inode of every directory in the parent trees to its subtrees
    /// (several, if several parents have a directory with that inode). Trees
    /// that can't be loaded are skipped with a warning. Returns `None` once the
    /// subtrees indexed plus the trees visited exceed `max_entries`: the
    /// partial index is dropped.
    fn dir_inode_map(
        be: &impl DecryptReadBackend,
        index: &impl ReadGlobalIndex,
        roots: &[TreeId],
        max_entries: usize,
    ) -> Option<HashMap<u64, SmallVec<[TreeId; 1]>>> {
        let mut map: HashMap<u64, SmallVec<[TreeId; 1]>> = HashMap::new();
        let mut seen: HashSet<TreeId> = HashSet::new();
        let mut entries = 0_usize;
        let mut count = || {
            entries += 1;
            (entries <= max_entries).then_some(())
        };
        let mut todo = Vec::new();
        for root in roots {
            if seen.insert(*root) {
                count()?;
                todo.push(*root);
            }
        }
        while let Some(tree_id) = todo.pop() {
            let tree = match Tree::from_backend(be, index, tree_id) {
                Ok(tree) => tree,
                Err(err) => {
                    warn!(
                        "detect-moves: ignoring parent tree {tree_id}: {}",
                        err.display_log()
                    );
                    continue;
                }
            };
            for node in tree.nodes {
                let Some(subtree) = node.subtree.filter(|_| node.is_dir()) else {
                    continue;
                };
                if node.meta.inode != 0 {
                    let subtrees = map.entry(node.meta.inode).or_default();
                    if !subtrees.contains(&subtree) {
                        count()?;
                        subtrees.push(subtree);
                    }
                }
                if seen.insert(subtree) {
                    count()?;
                    todo.push(subtree);
                }
            }
        }
        info!(
            "detect-moves: indexed {} parent directories by inode ({entries} entries)",
            map.len()
        );
        Some(map)
    }

    /// Whether a matched parent node is strong evidence that the file still
    /// has the parent's content. The match itself already compared the type
    /// (a regular file here), the size and the mtime; on top of that, the
    /// inode number (nonzero) and the ctime (present, to the nanosecond) must
    /// be equal, with no option ignoring them. A weaker match is trusted for
    /// this path only and vouches for nothing.
    ///
    /// The ctime is what makes this meaningful: unlike the mtime it can't be
    /// set from userspace, and any write or metadata change moves it to the
    /// current time. A file deleted and recreated, even with the same inode
    /// number, size and mtime, has a new ctime.
    ///
    /// The device is not part of this: `--reuse-extents` keys reused content
    /// on the file's own current extent signature, not on its identity, and
    /// devices are often not recorded (`set-devid = hardlink` stores 0 for
    /// files with one link) or differ between snapshots of the same tree.
    fn vouches_for_content(
        ignore_ctime: bool,
        ignore_inode: bool,
        p_node: &Node,
        node: &Node,
    ) -> bool {
        let (p_meta, meta) = (&p_node.meta, &node.meta);
        !ignore_ctime
            && !ignore_inode
            && p_meta.inode != 0
            && p_meta.inode == meta.inode
            && p_meta.ctime.is_some()
            && p_meta.ctime == meta.ctime
    }

    /// Whether a matched parent node proves the content of the inode itself,
    /// not only of this path: it vouches for the content, and the device was
    /// recorded (nonzero) on both sides and equal, so `(device, inode)`
    /// identifies the inode. A match without a device vouches for no other
    /// path.
    fn vouches_for_inode(
        ignore_ctime: bool,
        ignore_inode: bool,
        p_node: &Node,
        node: &Node,
    ) -> bool {
        Self::vouches_for_content(ignore_ctime, ignore_inode, p_node, node)
            && p_node.meta.device_id != 0
            && p_node.meta.device_id == node.meta.device_id
    }

    /// Whether the last file passed to [`Parent::process`] matched its parent
    /// node strongly enough to vouch for every path of its inode.
    pub(crate) fn last_match_vouches_for_inode(&self) -> bool {
        self.last_vouch >= Vouch::Inode
    }

    /// Whether the last file passed to [`Parent::process`] matched its parent
    /// node strongly enough to vouch for its content.
    pub(crate) fn last_match_vouches_for_content(&self) -> bool {
        self.last_vouch >= Vouch::Content
    }

    /// Returns the parent node with the given name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the parent node.
    ///
    /// # Returns
    ///
    /// The parent node with the given name, or `None` if the parent node is not found.
    fn p_node(&mut self, name: &OsStr) -> impl Iterator<Item = (&Node, bool)> {
        self.trees.iter_mut().filter_map(|parent| {
            let p_nodes = &parent.tree.nodes;
            loop {
                match p_nodes.get(parent.idx) {
                    None => break None,
                    Some(p_node) => match (*p_node.name()).cmp(name) {
                        Ordering::Less => parent.idx += 1,
                        Ordering::Equal => {
                            break Some((p_node, parent.moved));
                        }
                        Ordering::Greater => {
                            break None;
                        }
                    },
                }
            }
        })
    }

    /// Returns whether the given node is the parent of the given tree.
    ///
    /// # Arguments
    ///
    /// * `node` - The node to check.
    /// * `name` - The name of the tree.
    ///
    /// # Returns
    ///
    /// Whether the given node is the parent of the given tree.
    ///
    /// A node of a moved tree (found by a directory's inode, not at its path)
    /// matches only with full identity, whatever the options: type, size and
    /// mtime equal, and [`Parent::vouches_for_content`] without ignoring
    /// anything (nonzero equal inode, present equal ctime). Path identity no
    /// longer backs it, so no relaxation applies.
    ///
    /// # Note
    ///
    /// TODO: This function does not check whether the given node is a directory.
    fn is_parent(&mut self, node: &Node, name: &OsStr) -> ParentResult<&Node> {
        // use new variables as the mutable borrow is used later
        let restored = self.restored_unchanged(node);
        let ignore_ctime = self.ignore_ctime || restored;
        let ignore_inode = self.ignore_inode || restored;

        // a node on another filesystem than the backup root (see `set_dir`)
        if node.is_other_filesystem() {
            return ParentResult::NotFound;
        }
        let mut p_node = self.p_node(name).peekable();
        if p_node.peek().is_none() {
            return ParentResult::NotFound;
        }

        p_node
            .find(|(p_node, moved)| {
                let p_meta = &p_node.meta;
                let meta = &node.meta;
                if *moved {
                    return p_node.node_type == node.node_type
                        && p_meta.size == meta.size
                        && p_meta.mtime == meta.mtime
                        && Self::vouches_for_content(false, false, p_node, node);
                }

                let match_ctime =
                    ignore_ctime || p_meta.ctime.zip(meta.ctime).is_none_or(|(x, y)| x == y);
                let match_inode = ignore_inode
                    || p_meta.inode == 0
                    || meta.inode == 0
                    || p_meta.inode == meta.inode;

                p_node.node_type == node.node_type
                    && p_meta.size == meta.size
                    && p_meta.mtime == meta.mtime
                    && match_ctime
                    && match_inode
            })
            .map_or(ParentResult::NotMatched, |(p_node, _)| {
                ParentResult::Matched(p_node)
            })
    }

    // TODO: add documentation!
    ///
    /// # Type Parameters
    ///
    /// * `BE` - The type of the backend.
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to read from.
    /// * `name` - The name of the parent node.
    fn set_dir(
        &mut self,
        be: &impl DecryptReadBackend,
        index: &impl ReadGlobalIndex,
        name: &OsStr,
        node: &Node,
    ) {
        // On another filesystem than the backup root, or below one: inode
        // numbers there are another filesystem's, so neither the parent at
        // this path nor a moved candidate applies; everything is read.
        if self.below_boundary || node.is_other_filesystem() {
            let old_tree = std::mem::take(&mut self.trees);
            self.stack.push((old_tree, self.below_boundary));
            self.below_boundary = true;
            return;
        }

        let mut same_inode = false;
        // (subtree, moved): a subtree of a moved tree is moved too
        let mut new_ids: Vec<(TreeId, bool)> = self
            .p_node(name)
            .filter_map(|(p_node, moved)| {
                same_inode |= node.meta.inode != 0 && p_node.meta.inode == node.meta.inode;
                p_node.subtree.map(|id| (id, moved)).or_else(|| {
                    warn!("ignoring parent node {}: is no tree!", p_node.name);
                    None
                })
            })
            .collect();

        // A directory that is not at its parent's path (moved or renamed, or
        // new) also gets the parent trees of the directory with its inode, as
        // moved trees: their files match only with full identity.
        if let Some(max_entries) = self.detect_moves
            && node.meta.inode != 0
            && !same_inode
        {
            let roots = &self.tree_ids;
            let moved = self
                .dir_inodes
                .get_or_insert_with(|| {
                    Self::dir_inode_map(be, index, roots, max_entries).unwrap_or_else(|| {
                        warn!(
                            "detect-moves: off for this backup, the parent snapshot needs more than {max_entries} index entries"
                        );
                        HashMap::new()
                    })
                })
                .get(&node.meta.inode);
            new_ids.extend(moved.into_iter().flatten().map(|id| (*id, true)));
        }

        // remove potentially identical trees; one reached by path wins
        new_ids.sort();
        new_ids.dedup_by_key(|(id, _)| *id);

        let new_tree = new_ids
            .into_iter()
            .filter_map(
                |(tree_id, moved)| match Tree::from_backend(be, index, tree_id) {
                    Ok(tree) => Some(ParentTree {
                        tree,
                        idx: 0,
                        moved,
                    }),
                    Err(err) => {
                        warn!(
                            "ignoring error when loading parent tree {tree_id}: {}",
                            err.display_log()
                        );
                        None
                    }
                },
            )
            .collect();
        let old_tree = std::mem::replace(&mut self.trees, new_tree);
        self.stack.push((old_tree, self.below_boundary));
    }

    // TODO: add documentation!
    ///
    /// # Errors
    ///
    /// * If the tree stack is empty.
    fn finish_dir(&mut self) -> Result<(), TreeStackEmptyError> {
        let (tree, below_boundary) = self.stack.pop().ok_or(TreeStackEmptyError)?;
        self.trees = tree;
        self.below_boundary = below_boundary;

        Ok(())
    }

    // TODO: add documentation!
    pub(crate) fn tree_id(&self) -> Option<TreeId> {
        self.tree_ids.first().copied()
    }

    // TODO: add documentation!
    ///
    /// # Type Parameters
    ///
    /// * `BE` - The type of the backend.
    /// * `O` - The type of the tree item.
    ///
    /// # Arguments
    ///
    /// * `be` - The backend to read from.
    /// * `item` - The item to process.
    ///
    /// # Errors
    ///
    /// * If the tree stack is empty.
    pub(crate) fn process<O>(
        &mut self,
        be: &impl DecryptReadBackend,
        index: &impl ReadGlobalIndex,
        item: TreeType<O, OsString>,
    ) -> Result<ItemWithParent<O>, TreeStackEmptyError> {
        let result = match item {
            TreeType::NewTree((path, node, tree)) => {
                let parent_result = self
                    .is_parent(&node, &tree)
                    .map(|node| node.subtree.unwrap());
                self.set_dir(be, index, &tree, &node);
                TreeType::NewTree((path, node, parent_result))
            }
            TreeType::EndTree => {
                self.finish_dir()?;
                TreeType::EndTree
            }
            TreeType::Other((path, mut node, open)) => {
                let (ignore_ctime, ignore_inode) = (self.ignore_ctime, self.ignore_inode);
                let mut vouch = Vouch::Nothing;
                let parent = self.is_parent(&node, &node.name());
                let parent = match parent {
                    ParentResult::Matched(p_node) => {
                        if p_node.content.iter().flatten().all(|id| index.has_data(id)) {
                            vouch = if Self::vouches_for_inode(
                                ignore_ctime,
                                ignore_inode,
                                p_node,
                                &node,
                            ) {
                                Vouch::Inode
                            } else if Self::vouches_for_content(
                                ignore_ctime,
                                ignore_inode,
                                p_node,
                                &node,
                            ) {
                                Vouch::Content
                            } else {
                                Vouch::Nothing
                            };
                            node.content.clone_from(&p_node.content);
                            ParentResult::Matched(())
                        } else {
                            warn!(
                                "missing blobs in index for unchanged file {}; re-reading file",
                                path.display()
                            );
                            ParentResult::NotFound
                        }
                    }
                    parent_result => parent_result.map(|_| ()),
                };
                self.last_vouch = vouch;
                TreeType::Other((path, node, (open, parent)))
            }
        };
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::backend::node::{Metadata, NodeType};

    fn file_with_inode(inode: u64) -> Node {
        let meta = Metadata {
            inode,
            ..Metadata::default()
        };
        Node::new_node(OsStr::new("file"), NodeType::File, meta)
    }

    #[rstest]
    #[case::compared(false, false)]
    #[case::ignored(true, true)]
    fn is_parent_compares_inodes_unless_ignored(
        #[case] ignore_inode: bool,
        #[case] expected_match: bool,
    ) {
        let parent_tree = Tree {
            nodes: vec![file_with_inode(1)],
        };
        let mut parent = Parent {
            tree_ids: Vec::new(),
            trees: vec![ParentTree {
                tree: parent_tree,
                idx: 0,
                moved: false,
            }],
            stack: Vec::new(),
            below_boundary: false,
            ignore_ctime: false,
            ignore_inode,
            restored_before: None,
            detect_moves: None,
            dir_inodes: None,
            last_vouch: Vouch::Nothing,
        };

        let result = parent.is_parent(&file_with_inode(2), OsStr::new("file"));

        assert_eq!(matches!(result, ParentResult::Matched(_)), expected_match);
    }

    fn file_with(device_id: u64, inode: u64, ctime: Option<i64>) -> Node {
        let meta = Metadata {
            device_id,
            inode,
            ctime: ctime.map(|s| Timestamp::from_second(s).unwrap()),
            ..Metadata::default()
        };
        Node::new_node(OsStr::new("file"), NodeType::File, meta)
    }

    #[rstest]
    #[case::all_equal(false, false, (1, 5, Some(9)), (1, 5, Some(9)), true)]
    #[case::ignore_ctime(true, false, (1, 5, Some(9)), (1, 5, Some(9)), false)]
    #[case::ignore_inode(false, true, (1, 5, Some(9)), (1, 5, Some(9)), false)]
    #[case::parent_inode_zero(false, false, (1, 0, Some(9)), (1, 0, Some(9)), false)]
    #[case::inode_differs(false, false, (1, 5, Some(9)), (1, 6, Some(9)), false)]
    #[case::device_differs(false, false, (1, 5, Some(9)), (2, 5, Some(9)), false)]
    #[case::device_not_recorded(false, false, (0, 5, Some(9)), (0, 5, Some(9)), false)]
    #[case::ctime_missing(false, false, (1, 5, None), (1, 5, None), false)]
    #[case::parent_ctime_missing(false, false, (1, 5, None), (1, 5, Some(9)), false)]
    #[case::ctime_differs(false, false, (1, 5, Some(9)), (1, 5, Some(10)), false)]
    fn only_a_full_identity_match_vouches_for_the_inode(
        #[case] ignore_ctime: bool,
        #[case] ignore_inode: bool,
        #[case] parent: (u64, u64, Option<i64>),
        #[case] current: (u64, u64, Option<i64>),
        #[case] expected: bool,
    ) {
        let p_node = file_with(parent.0, parent.1, parent.2);
        let node = file_with(current.0, current.1, current.2);
        assert_eq!(
            Parent::vouches_for_inode(ignore_ctime, ignore_inode, &p_node, &node),
            expected
        );
    }

    #[rstest]
    #[case::all_equal(false, false, (1, 5, Some(9)), (1, 5, Some(9)), true)]
    #[case::device_not_recorded(false, false, (0, 5, Some(9)), (0, 5, Some(9)), true)]
    #[case::device_differs(false, false, (1, 5, Some(9)), (2, 5, Some(9)), true)]
    #[case::ignore_ctime(true, false, (0, 5, Some(9)), (0, 5, Some(9)), false)]
    #[case::ignore_inode(false, true, (0, 5, Some(9)), (0, 5, Some(9)), false)]
    #[case::inode_not_recorded(false, false, (0, 0, Some(9)), (0, 0, Some(9)), false)]
    #[case::inode_differs(false, false, (0, 5, Some(9)), (0, 6, Some(9)), false)]
    #[case::ctime_missing(false, false, (0, 5, None), (0, 5, None), false)]
    #[case::parent_ctime_missing(false, false, (0, 5, None), (0, 5, Some(9)), false)]
    #[case::ctime_differs(false, false, (0, 5, Some(9)), (0, 5, Some(10)), false)]
    // deleted and recreated: the inode number is reused, size and mtime are
    // the same (the match compared those), the ctime is new
    #[case::recreated_same_inode(false, false, (0, 5, Some(9)), (0, 5, Some(1_000)), false)]
    fn a_match_vouches_for_content_by_inode_and_ctime(
        #[case] ignore_ctime: bool,
        #[case] ignore_inode: bool,
        #[case] parent: (u64, u64, Option<i64>),
        #[case] current: (u64, u64, Option<i64>),
        #[case] expected: bool,
    ) {
        let p_node = file_with(parent.0, parent.1, parent.2);
        let node = file_with(current.0, current.1, current.2);
        assert_eq!(
            Parent::vouches_for_content(ignore_ctime, ignore_inode, &p_node, &node),
            expected
        );
    }
}
