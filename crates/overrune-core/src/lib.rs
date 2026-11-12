//! Shared model for overrune: GPU reads, capability-derived validation,
//! profiles, the software clock curve, and the CLI/GUI ↔ daemon protocol.

pub mod caps;
pub mod curve;
pub mod gpu;
pub mod ipc;
pub mod profile;

pub use caps::{Capabilities, OutOfRange, PStateLimits};
pub use curve::CurveState;
pub use gpu::{Gpu, GpuError, Sample};
pub use profile::{Curve, ProfileError, ProfileSettings};
