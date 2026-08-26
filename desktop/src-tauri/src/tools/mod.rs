//! The native tool surface.
//!
//! One module per family, and nothing shared between them except the agent state
//! and the guard. Adding a tool means adding a command to one of these files, a
//! line to `build.rs`, a line to each capability file, and a record in the
//! frontend registry. Four small edits in four obvious places, rather than one
//! large function that grows for ever.
//!
//! Those edits are now checked by CI rather than by memory: `.build/verify-frontend.mjs`
//! parses all five lists and compares them as sets in both directions, so a tool
//! that is missing its `build.rs` line — and is therefore not ACL-checked at all —
//! fails the build instead of shipping.

pub mod apps;
pub mod browser;
pub mod clip;
pub mod docs;
pub mod files;
pub mod input;
pub mod screen;
pub mod system;
pub mod undo;
