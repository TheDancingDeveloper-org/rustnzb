//! Article decoding (yEnc) and file assembly.
//!
//! This crate contains:
//! - `yenc` — yEnc decoder
//! - `cache` — Bounded article cache with disk spill
//! - `assembler` — Write decoded articles into final files

pub mod assembler;
pub mod cache;
pub mod repair_fold;
pub mod slice_map;
pub mod yenc;

pub use assembler::FileAssembler;
#[cfg(any(test, feature = "test-support"))]
pub use assembler::WriteStall;
pub use cache::ArticleCache;
pub use repair_fold::{FoldError, RepairFold, SharedFold};
pub use slice_map::{FileSlices, SliceDigest, SliceLayout, SliceState, SliceVerifier};
pub use yenc::{YencDecodeResult, decode_yenc};
