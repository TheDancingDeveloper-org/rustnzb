//! Post-processing pipeline: par2 verify/repair, RAR/7z/ZIP extraction, cleanup.
//!
//! This crate contains:
//! - `detect` — File detection helpers (par2, RAR, 7z, ZIP, cleanup candidates)
//! - `par2` — Native PAR2 verify/repair via `rust-par2`
//! - `unpack` — RAR extraction (unrar), 7z (7z binary), ZIP (zip crate)
//! - `pipeline` — Orchestrate: verify -> repair -> extract -> cleanup

pub mod detect;
pub mod par2;
pub mod pipeline;
pub mod unpack;

// Re-export nzb-core (and transitively nzb-nntp) so consumers only
// need nzb-postproc as a single dependency.
pub use nzb_core;

pub use detect::{ArchiveType, RarVolumeInfo, has_usable_output, parse_rar_volume};
pub use par2::recovery_can_cover;
pub use pipeline::{PostProcConfig, PostProcResult, run_pipeline};
pub use unpack::find_unrar;
