//! `image.*`: the image store (`veynodevm_oci::Store`), which only the
//! daemon opens.
use super::{
    container,
    util::{flag, string},
    with_progress,
};
use crate::{
    api_server::{Ctx, StreamIo},
    home,
};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;
use veynodevm_api::{refuse, ErrorCode};
use veynodevm_oci::Store;

fn store() -> Result<Store> {
    Store::open(&home::images_dir()?)
}

pub fn ls(_: &Ctx, _: Value) -> Result<Value> {
    Ok(serde_json::to_value(store()?.list()?)?)
}

pub fn inspect(_: &Ctx, args: Value) -> Result<Value> {
    let reference = string(&args, "reference")?;
    let image = store()?
        .find(reference)?
        .ok_or_else(|| refuse(ErrorCode::NotFound, format!("no such image: {reference}")))?;
    Ok(serde_json::to_value(image)?)
}

/// `image.pull`: the image, with its root filesystem prepared for Linux
/// images; progress as events.
pub fn pull(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let reference = string(&args, "reference")?;
    let store = store()?;
    let mut report = |m: &str| {
        let _ = io.progress(m);
    };
    let image = store.pull(reference, &mut report)?;
    if image.config.os != "darwin" {
        store.rootfs(&image, &mut report)?;
    }
    Ok(serde_json::to_value(&image)?)
}

pub fn rm(_: &Ctx, args: Value) -> Result<Value> {
    let image = store()?.remove(string(&args, "reference")?)?;
    Ok(json!({"reference":image.reference}))
}

/// `image.load`: a `docker save` archive on the host; with `rootfs`, the
/// root filesystems are prepared too (what `veynodevm build` asks for).
pub fn load(_: &Ctx, args: Value, io: &StreamIo) -> Result<Value> {
    let path = Path::new(string(&args, "path")?);
    if !path.is_absolute() {
        return Err(refuse(ErrorCode::Invalid, "path must be absolute"));
    }
    let store = store()?;
    let images = store.load(path)?;
    let mut out = Vec::new();
    for image in images {
        let mut row = serde_json::to_value(&image)?;
        if flag(&args, "rootfs")? {
            let root = with_progress(io, || store.rootfs(&image, &mut |m| super::progress(m)))?;
            row["rootfs"] = json!(root);
        }
        out.push(row);
    }
    Ok(Value::Array(out))
}

pub fn gc(_: &Ctx, _: Value) -> Result<Value> {
    let freed = store()?.gc(&container::images_in_use()?)?;
    Ok(json!({"removedBytes":freed}))
}
