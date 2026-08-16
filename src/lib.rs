//! sanctions-ingest — hardened one-shot parser for the DFAT Consolidated List.
//! The platform server is strictly query-only; this crate is the only list-WRITE
//! surface. Library exposes the pieces the binary and the parity/golden tests
//! share.

pub mod chain;
pub mod neo;
pub mod normalize;
pub mod parse;
pub mod r2;
