//! OCI image support for VeynodeVM: references, registry pulls, a
//! content-addressed store, whiteout-aware layer flattening and conversion of
//! an image into a read-only squashfs root for micro-VMs.
pub mod flatten;
pub mod reference;
pub mod registry;
pub mod store;

pub use reference::Reference;
pub use store::{ImageConfig, ImageRecord, Store};
