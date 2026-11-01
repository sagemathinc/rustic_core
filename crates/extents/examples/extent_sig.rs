//! Print the extent signature of files below a pinned read-only btrfs root.
//!
//! `extent_sig ROOT FILE...` (FILE relative to ROOT); prints one line per file:
//! `<path> <signature hex | ->`, then the end-of-run check.

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn main() {
    use rustic_extents::linux::ExtentScanner;
    use std::{fmt::Write, fs::File};

    let mut args = std::env::args().skip(1);
    let root = args.next().expect("usage: extent_sig ROOT FILE...");
    let scanner = match ExtentScanner::pin(File::open(&root).expect("open root"), false) {
        Ok(scanner) => scanner,
        Err(reason) => {
            println!("no-op: {reason}");
            return;
        }
    };
    for path in args {
        let sig = File::open(std::path::Path::new(&root).join(&path))
            .ok()
            .and_then(|f| scanner.signature_of(&f));
        let hex = sig.map_or_else(
            || "-".to_string(),
            |s| {
                s.iter().fold(String::new(), |mut out, b| {
                    let _ = write!(out, "{b:02x}");
                    out
                })
            },
        );
        println!("{path} {hex}");
    }
    println!("end: {:?}", scanner.verify_end());
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
fn main() {}
