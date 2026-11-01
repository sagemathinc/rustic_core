//! `--reuse-extents`: a file whose btrfs extents are exactly those of a file
//! archived in this run (or of an unchanged file vouched for by the parent
//! snapshot) reuses that file's content without being read.
//!
//! The signatures come from [`rustic_extents`]; this module holds the run's
//! state: the pinned source, the signature cache, the order in which parent
//! matches seed the cache, and the statistics.

use std::{
    collections::BTreeSet,
    fs::File,
    path::Path,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use log::warn;

use crate::{
    archiver::file_archiver::{ExtentCache, ExtentOrigin},
    backend::ReadSourceOpen,
    blob::DataId,
    error::{ErrorKind, RusticError, RusticResult},
};

/// Where signatures come from: the btrfs scanner (or a test fake).
trait Scanner: std::fmt::Debug + Send + Sync {
    /// The signature of an open file, or `None` to read it normally.
    fn signature_of(&self, file: &File) -> Option<Vec<u8>>;
    /// Whether the file still has `expected` (an owner, after its read).
    fn still(&self, file: &File, expected: &[u8]) -> bool;
    /// Whether the source is still the pinned, unchanged one.
    fn verify_end(&self) -> Result<(), String>;
}

#[cfg(target_os = "linux")]
impl Scanner for rustic_extents::linux::ExtentScanner {
    fn signature_of(&self, file: &File) -> Option<Vec<u8>> {
        self.signature_of(file)
    }
    fn still(&self, file: &File, expected: &[u8]) -> bool {
        self.still(file, expected)
    }
    fn verify_end(&self) -> Result<(), String> {
        self.verify_end()
    }
}

/// Pin the source, or say why reuse is a no-op.
fn pin(root: &Path, hooks: &hooks::Hooks) -> Result<Box<dyn Scanner>, &'static str> {
    if let Some(fake) = hooks.fake_scanner(root) {
        return Ok(fake);
    }
    let root = File::open(root).map_err(|_| "cannot open source")?;
    #[cfg(target_os = "linux")]
    {
        rustic_extents::linux::ExtentScanner::pin(root, hooks.skip_barrier())
            .map(|scanner| -> Box<dyn Scanner> { Box::new(scanner) })
    }
    #[cfg(not(target_os = "linux"))]
    {
        drop(root);
        Err("not btrfs")
    }
}

/// The state of `--reuse-extents` for one run.
#[derive(Debug)]
pub(crate) struct ExtentReuse {
    scanner: Box<dyn Scanner>,
    /// Content by exact extent signature.
    pub(crate) cache: ExtentCache,
    seeds: SeedOrder,
    files_reused: AtomicU64,
    bytes_reused: AtomicU64,
    hooks: hooks::Hooks,
}

impl ExtentReuse {
    /// Pin the backup source, or log why the option does nothing in this run.
    pub(crate) fn new(root: &Path) -> Option<Self> {
        let hooks = hooks::Hooks::from_env();
        match pin(root, &hooks) {
            Ok(scanner) => Some(Self {
                scanner,
                cache: ExtentCache::default(),
                seeds: SeedOrder::default(),
                files_reused: AtomicU64::new(0),
                bytes_reused: AtomicU64::new(0),
                hooks,
            }),
            Err(reason) => {
                warn!("reuse-extents: no-op ({reason})");
                None
            }
        }
    }

    /// The signature of an open file (writeback barrier, two equal scans).
    pub(crate) fn signature(&self, file: &File) -> Option<Vec<u8>> {
        let signature = self.scanner.signature_of(file);
        self.hooks.scanned();
        signature
    }

    /// Whether an owner's file still has the signature after its read.
    pub(crate) fn still(&self, file: &File, signature: &[u8]) -> bool {
        self.scanner.still(file, signature)
    }

    /// Cache the parent's content of an unchanged file under the file's
    /// current signature. Only for a parent match that vouches for the inode.
    pub(crate) fn seed<O: ReadSourceOpen>(
        &self,
        open: Option<O>,
        content: Option<&[DataId]>,
        ticket: u64,
    ) {
        let (Some(open), Some(content)) = (open, content) else {
            return;
        };
        let Ok(reader) = open.open_with_holes() else {
            return;
        };
        if let Some(signature) = reader.as_file().and_then(|file| self.signature(file)) {
            self.cache
                .seed_new_key(signature, ExtentOrigin::Seed(ticket), content);
        }
    }

    /// Count a file whose content was reused.
    pub(crate) fn reused(&self, size: u64) {
        _ = self.files_reused.fetch_add(1, Ordering::Relaxed);
        _ = self.bytes_reused.fetch_add(size, Ordering::Relaxed);
    }

    /// Files and bytes reused in this run.
    pub(crate) fn stats(&self) -> (u64, u64) {
        (
            self.files_reused.load(Ordering::Relaxed),
            self.bytes_reused.load(Ordering::Relaxed),
        )
    }

    /// End of run: the source must still be the pinned read-only subvolume,
    /// unchanged. Otherwise no snapshot may be saved.
    pub(crate) fn verify_end(&self) -> RusticResult<()> {
        self.scanner.verify_end().map_err(|reason| {
            RusticError::new(
                ErrorKind::InputOutput,
                "{reason}. --reuse-extents requires an unchanged read-only source; no snapshot was saved.",
            )
            .attach_context("reason", reason)
        })
    }

    /// The turn of a parent match that seeds the cache.
    pub(crate) fn seed_turn(self: &Arc<Self>) -> SeedTurn {
        SeedTurn::Seed(SeedGuard {
            reuse: Arc::clone(self),
            ticket: self.seeds.issue(),
        })
    }

    /// The turn of a file to read: after every seed issued before it.
    pub(crate) fn read_turn(&self) -> SeedTurn {
        SeedTurn::After(self.seeds.issued())
    }

    /// Wait until every seed issued before `turn` is done.
    pub(crate) fn wait_for(&self, turn: &SeedTurn) {
        if let SeedTurn::After(before) = turn {
            self.seeds.wait_before(*before);
        }
    }
}

/// Seeds finish in any order, but a file must not claim its signature before
/// the parent matches that came before it in the tree have seeded the cache:
/// otherwise whether it is read would depend on thread timing.
#[derive(Debug, Default)]
struct SeedOrder {
    state: Mutex<SeedState>,
    done: Condvar,
}

#[derive(Debug, Default)]
struct SeedState {
    issued: u64,
    outstanding: BTreeSet<u64>,
}

impl SeedOrder {
    fn state(&self) -> MutexGuard<'_, SeedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn issue(&self) -> u64 {
        let mut state = self.state();
        let ticket = state.issued;
        state.issued += 1;
        _ = state.outstanding.insert(ticket);
        ticket
    }

    fn issued(&self) -> u64 {
        self.state().issued
    }

    fn complete(&self, ticket: u64) {
        _ = self.state().outstanding.remove(&ticket);
        self.done.notify_all();
    }

    fn wait_before(&self, before: u64) {
        let mut state = self.state();
        while state.outstanding.first().is_some_and(|&t| t < before) {
            state = self
                .done
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        drop(state);
    }
}

/// A file's place in the seed order (see [`SeedOrder`]).
#[derive(Debug)]
pub(crate) enum SeedTurn {
    /// Not taking part.
    None,
    /// A parent match that seeds the cache.
    Seed(SeedGuard),
    /// A file to read, after the seeds issued before it.
    After(u64),
}

impl SeedTurn {
    /// The seeds whose content may serve this file: those numbered below.
    pub(crate) const fn before(&self) -> u64 {
        match self {
            Self::After(before) => *before,
            Self::None | Self::Seed(_) => 0,
        }
    }
}

/// A seed in progress. It completes when dropped, also if its item is never
/// processed (an aborted run), so that no file waits for it forever.
#[derive(Debug)]
pub(crate) struct SeedGuard {
    reuse: Arc<ExtentReuse>,
    ticket: u64,
}

impl SeedGuard {
    /// This seed's number in tree order.
    pub(crate) const fn ticket(&self) -> u64 {
        self.ticket
    }
}

impl Drop for SeedGuard {
    fn drop(&mut self) {
        self.reuse.seeds.complete(self.ticket);
    }
}

/// Test-only switches (cargo feature `test-hooks`, never in a release build).
#[cfg(all(feature = "test-hooks", unix))]
mod hooks {
    use std::{
        env,
        fs::File,
        os::unix::fs::FileExt,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        thread,
        time::{Duration, Instant},
    };

    use log::warn;

    use super::Scanner;

    /// `RUSTIC_TEST_SKIP_EXTENT_BARRIER=1` skips the writeback barrier;
    /// `RUSTIC_TEST_PAUSE_AFTER_SCANS=<n>` with `RUSTIC_TEST_PAUSE_DIR=<dir>`
    /// creates `<dir>/paused` after the n-th signature scan and waits until
    /// `<dir>/release` exists (at most 20 minutes). A source root holding
    /// `.rustic-test-fake-extents` uses [`FakeScanner`] (a file, not an
    /// environment variable, so that tests in one process can differ).
    #[derive(Debug)]
    pub(super) struct Hooks {
        skip_barrier: bool,
        pause: Option<(u64, PathBuf)>,
        scans: AtomicU64,
    }

    /// A stand-in for btrfs, to test the archiver anywhere: a file whose
    /// first bytes are `SIG:` and a newline-terminated name has the signature
    /// `SIG:<name>`; any other file has none. The end check fails if the
    /// source root holds `.rustic-test-end-fails`.
    #[derive(Debug)]
    struct FakeScanner {
        root: PathBuf,
    }

    impl FakeScanner {
        fn read_signature(file: &File) -> Option<Vec<u8>> {
            let mut head = [0; 64];
            let len = file.read_at(&mut head, 0).ok()?;
            let head = &head[..len];
            let end = head.iter().position(|&b| b == b'\n')?;
            head.starts_with(b"SIG:").then(|| head[..end].to_vec())
        }
    }

    impl Scanner for FakeScanner {
        fn signature_of(&self, file: &File) -> Option<Vec<u8>> {
            Self::read_signature(file)
        }
        fn still(&self, file: &File, expected: &[u8]) -> bool {
            Self::read_signature(file).is_some_and(|s| s == expected)
        }
        fn verify_end(&self) -> Result<(), String> {
            if self.root.join(".rustic-test-end-fails").exists() {
                Err("the test source changed".to_string())
            } else {
                Ok(())
            }
        }
    }

    impl Hooks {
        pub(super) fn from_env() -> Self {
            let pause = env::var("RUSTIC_TEST_PAUSE_AFTER_SCANS")
                .ok()
                .and_then(|n| n.parse().ok())
                .zip(env::var_os("RUSTIC_TEST_PAUSE_DIR").map(PathBuf::from));
            Self {
                skip_barrier: env::var_os("RUSTIC_TEST_SKIP_EXTENT_BARRIER")
                    .is_some_and(|v| v == "1"),
                pause,
                scans: AtomicU64::new(0),
            }
        }

        #[allow(clippy::unused_self)]
        pub(super) fn fake_scanner(&self, root: &Path) -> Option<Box<dyn Scanner>> {
            root.join(".rustic-test-fake-extents")
                .exists()
                .then(|| -> Box<dyn Scanner> {
                    warn!("fake extent signatures (test-hooks)");
                    Box::new(FakeScanner {
                        root: root.to_path_buf(),
                    })
                })
        }

        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pub(super) fn skip_barrier(&self) -> bool {
            if self.skip_barrier {
                warn!("extent barrier disabled (test-hooks)");
            }
            self.skip_barrier
        }

        pub(super) fn scanned(&self) {
            let scans = self.scans.fetch_add(1, Ordering::Relaxed) + 1;
            let Some((after, dir)) = &self.pause else {
                return;
            };
            if scans != *after {
                return;
            }
            warn!("extent scans paused after {scans} (test-hooks)");
            if std::fs::write(dir.join("paused"), b"").is_err() {
                return;
            }
            let start = Instant::now();
            while !dir.join("release").exists() && start.elapsed() < Duration::from_mins(20) {
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Release builds ignore the test switches.
#[cfg(not(all(feature = "test-hooks", unix)))]
mod hooks {
    use std::path::Path;

    use super::Scanner;

    #[derive(Debug)]
    pub(super) struct Hooks;

    impl Hooks {
        pub(super) const fn from_env() -> Self {
            Self
        }
        #[allow(clippy::unused_self)]
        pub(super) const fn fake_scanner(&self, _root: &Path) -> Option<Box<dyn Scanner>> {
            None
        }
        #[allow(clippy::unused_self)]
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pub(super) const fn skip_barrier(&self) -> bool {
            false
        }
        #[allow(clippy::unused_self)]
        pub(super) const fn scanned(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Duration};

    #[test]
    fn a_read_waits_for_earlier_seeds_only() {
        let order = Arc::new(SeedOrder::default());
        let first = order.issue();
        let before_second = order.issued();
        let second = order.issue();
        // a file between the two seeds waits for the first only
        let (sender, receiver) = mpsc::channel();
        let waiter = {
            let order = Arc::clone(&order);
            thread::spawn(move || {
                order.wait_before(before_second);
                sender.send(()).unwrap();
            })
        };
        assert!(receiver.recv_timeout(Duration::from_millis(200)).is_err());
        order.complete(first);
        receiver.recv().unwrap();
        waiter.join().unwrap();
        // the later seed doesn't block it, and a file after both waits for it
        order.wait_before(before_second);
        assert_eq!(order.issued(), 2);
        order.complete(second);
        order.wait_before(order.issued());
    }
}
