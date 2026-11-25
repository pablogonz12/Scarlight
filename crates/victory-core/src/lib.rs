//! Shared plumbing for every VIctory crate and tool.
//!
//! * [`config`]: where the user's files live (never inside the repository).
//! * [`ctl`]: the debug tool's control folder, which a running game reads.
//! * [`policy`]: the runtime half of rule #1, refusing to write
//!   game-derived data into the source tree.
//! * [`profile`]: what one title's build needs, read from the user's config
//!   directory and selected by the image's own hash.
//! * [`logging`]: one tracing setup for all binaries.

pub mod config;
pub mod ctl;
pub mod logging;
pub mod policy;
pub mod profile;

pub use config::Config;
pub use policy::{PolicyError, ensure_outside_repo};
pub use profile::Profile;
