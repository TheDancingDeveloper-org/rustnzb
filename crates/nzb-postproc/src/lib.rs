//! Post-processing pipeline: par2 verify/repair, RAR/7z/TAR/ZIP extraction, cleanup.
//!
//! This crate contains:
//! - `detect` — File detection helpers (par2, RAR, 7z, TAR, ZIP, cleanup candidates)
//! - `par2` — Native PAR2 verify/repair via `rust-par2`
//! - `unpack` — RAR/7z extraction (system tools), TAR/ZIP (native crates)
//! - `pipeline` — Orchestrate: verify -> repair -> extract -> cleanup

pub mod detect;
pub mod par2;
pub mod pipeline;
pub mod resources;
pub mod unpack;

// Re-export nzb-core (and transitively nzb-nntp) so consumers only
// need nzb-postproc as a single dependency.
pub use nzb_core;

pub use detect::{
    ArchiveType, RarVolumeInfo, has_rar_signature, has_usable_output, parse_rar_volume,
    parse_rar_volume_at,
};
pub use par2::recovery_can_cover;
pub use pipeline::{
    PostProcConfig, PostProcResult, run_pipeline, run_pipeline_with_cleanup,
    run_pipeline_with_resources, run_repair_pipeline,
};
pub use resources::{PostProcLimits, PostProcResourcePool, PostProcResourceSnapshot};
pub use unpack::{
    extract_tar, extractor_reads_password_from_stdin, find_unrar, normalize_extracted_permissions,
};
