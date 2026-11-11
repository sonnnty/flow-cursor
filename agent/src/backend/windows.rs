//! Windows backend (filled in later).
use crate::backend::{Backend, BackendOptions, EventRx};
use std::sync::Arc;

pub async fn create(_opts: &BackendOptions) -> anyhow::Result<(Arc<dyn Backend>, EventRx)> {
    anyhow::bail!("not implemented yet")
}
