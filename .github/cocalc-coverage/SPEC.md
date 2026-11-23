# Coverage measurement (cocalc fork)

This file defines how `cocalc-coverage.yml` measures coverage, so that measurements before and after a change are comparable. Any change to the definition below starts a new series; numbers from different versions of this file are not compared.

## What is measured
- LLVM **line, region and function** coverage of this workspace's own crates. Regions are not branches; there is no branch-coverage claim.
- Run **A**: `cargo llvm-cov --workspace --locked --no-fail-fast --json --output-path A.json` (default features).
- Run **A-hooks**: the same with `--features rustic_core/test-hooks`, reported separately.
- No test filter, no `--ignored`, no `--doctests`, no `--release`.
- The repository's committed `[profile.dev]` is used as is, with `CARGO_INCREMENTAL=0` and `RUST_BACKTRACE=0`.
- From the same profile data: `cargo llvm-cov report --lcov` and `cargo llvm-cov report --summary-only`.

## Tools
- Runner `ubuntu-24.04` (x86_64). Toolchain `1.95.0` with `llvm-tools-preview`.
- `cargo install cargo-llvm-cov --version 0.9.2 --locked`, built in the job; the job checks the reported version.
- Actions are pinned by commit sha in the workflow. There is no build cache: every run builds from scratch.

## The measured commit
`INTENDED_SHA` is the pull request's head commit (not GitHub's merge ref), or the pushed/dispatched commit for other events. The job checks it out, records `git rev-parse HEAD`, and the run is DIAGNOSTIC unless the two are equal.

## Verdict
A run is a **CANDIDATE** when all of these hold:
- every setup step after checkout succeeded: the tool install, the self-tests of these helpers, and the changed-path listing (plus, for the CLI, the rustic_core lock entry and, for C, btrfs-progs). No such step aborts the job; each records its exit status;
- HEAD is the intended commit;
- every selected test passed;
- Cargo.lock is unchanged;
- the tool version line is exactly `cargo-llvm-cov 0.9.2`;
- the raw profiles were cleaned;
- both reports were written;
- the totals check out: (a) per kind, the files inside and outside the repository add up to the export's totals; (b) the export's totals equal the summary's TOTAL row; (c) the export is this run's own.

Otherwise the run is **DIAGNOSTIC**, and the job fails after uploading every output.

A CANDIDATE becomes a reference measurement only after a separate review of its artifacts; the job never claims that by itself.

## Recorded (`meta.json`, `record.env`)
- uname; host triple; resolved paths and sha256 of cargo, rustc, cargo-llvm-cov, llvm-profdata and llvm-cov;
- tool versions; this file's sha256;
- the intended and checked-out commits, the PR base, and the changed paths;
- Cargo.lock sha256 before and after;
- start/end time, nproc, disk before and after;
- test counts and the names of failing tests;
- the problems that made a run DIAGNOSTIC.

## Network
The run uses GitHub Actions' own traffic (checkout, toolchain setup, artifact upload), crates.io for the locked dependencies and for building cargo-llvm-cov, and the rustup component download. This is what the measurement permits; it is not enforced.
