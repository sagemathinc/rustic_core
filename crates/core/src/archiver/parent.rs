use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString},
};

use jiff::Timestamp;
use log::warn;

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
    /// The parent tree.
    trees: Vec<(Tree, usize)>,
    /// The stack of parent trees.
    stack: Vec<Vec<(Tree, usize)>>,
    /// Ignore ctime when comparing nodes.
    ignore_ctime: bool,
    /// Ignore inode number when comparing nodes.
    ignore_inode: bool,
    /// The source was restored from the parent, finishing at this time (see
    /// `ParentOptions::restored_before`).
    restored_before: Option<Timestamp>,
    /// What the last file processed was vouched for by its parent match.
    last_vouch: Vouch,
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
    /// * `restored_before` - The source was restored from this parent, and the
    ///   restore finished at this time.
    pub(crate) fn new(
        be: &impl DecryptReadBackend,
        index: &impl ReadGlobalIndex,
        tree_id: impl IntoIterator<Item = TreeId>,
        ignore_ctime: bool,
        ignore_inode: bool,
        restored_before: Option<Timestamp>,
    ) -> Self {
        // if tree_id is given, try to load tree from backend.
        let (trees, tree_ids) = tree_id
            .into_iter()
            .filter_map(|tree_id| match Tree::from_backend(be, index, tree_id) {
                Ok(tree) => Some(((tree, 0), tree_id)),
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
            ignore_ctime,
            ignore_inode,
            restored_before,
            last_vouch: Vouch::Nothing,
        }
    }

    /// Whether `node` was last changed before the restore it came from
    /// finished: its ctime is present and strictly before
    /// [`Parent::restored_before`]. Such a file still holds what the restore
    /// wrote, i.e. the parent's content at its path, so its ctime and inode
    /// (new on the restored tree) are not compared with the parent's. Any
    /// later change moves its ctime past that time.
    fn restored_unchanged(&self, node: &Node) -> bool {
        self.restored_before
            .zip(node.meta.ctime)
            .is_some_and(|(restored_before, ctime)| ctime < restored_before)
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
    fn p_node(&mut self, name: &OsStr) -> impl Iterator<Item = &Node> {
        self.trees.iter_mut().filter_map(|(tree, idx)| {
            let p_nodes = &tree.nodes;
            loop {
                match p_nodes.get(*idx) {
                    None => break None,
                    Some(p_node) => match (*p_node.name()).cmp(name) {
                        Ordering::Less => *idx += 1,
                        Ordering::Equal => {
                            break Some(p_node);
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
    /// # Note
    ///
    /// TODO: This function does not check whether the given node is a directory.
    fn is_parent(&mut self, node: &Node, name: &OsStr) -> ParentResult<&Node> {
        // use new variables as the mutable borrow is used later
        let restored = self.restored_unchanged(node);
        let ignore_ctime = self.ignore_ctime || restored;
        let ignore_inode = self.ignore_inode || restored;

        let mut p_node = self.p_node(name).peekable();
        if p_node.peek().is_none() {
            return ParentResult::NotFound;
        }

        p_node
            .find(|p_node| {
                let p_meta = &p_node.meta;
                let meta = &node.meta;

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
            .map_or(ParentResult::NotMatched, ParentResult::Matched)
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
    ) {
        let mut new_ids: Vec<_> = self
            .p_node(name)
            .filter_map(|p_node| {
                p_node.subtree.or_else(|| {
                    warn!("ignoring parent node {}: is no tree!", p_node.name);
                    None
                })
            })
            .collect();

        // remove potentially identical trees
        new_ids.sort();
        new_ids.dedup();

        let new_tree = new_ids
            .into_iter()
            .filter_map(|tree_id| match Tree::from_backend(be, index, tree_id) {
                Ok(tree) => Some((tree, 0)),
                Err(err) => {
                    warn!(
                        "ignoring error when loading parent tree {tree_id}: {}",
                        err.display_log()
                    );
                    None
                }
            })
            .collect();
        let old_tree = std::mem::replace(&mut self.trees, new_tree);
        self.stack.push(old_tree);
    }

    // TODO: add documentation!
    ///
    /// # Errors
    ///
    /// * If the tree stack is empty.
    fn finish_dir(&mut self) -> Result<(), TreeStackEmptyError> {
        let tree = self.stack.pop().ok_or(TreeStackEmptyError)?;
        self.trees = tree;

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
                self.set_dir(be, index, &tree);
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
            trees: vec![(parent_tree, 0)],
            stack: Vec::new(),
            ignore_ctime: false,
            ignore_inode,
            restored_before: None,
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
