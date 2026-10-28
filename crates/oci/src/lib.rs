//! OCI image support for VeynodeVM: references, registry pulls, a
//! content-addressed store, whiteout-aware layer flattening and conversion of
//! an image into a read-only squashfs root for micro-VMs. Tart-format macOS
//! images are materialized as VM machines instead (`tart`).
pub mod disk;
pub mod flatten;
pub mod reference;
pub mod registry;
pub mod store;
pub mod tart;

pub use reference::Reference;
pub use store::{ImageConfig, ImageRecord, Store};
