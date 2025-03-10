use std::sync::Arc;

use axum::Extension;
use hyper::StatusCode;
use tokio::sync::RwLock;

use crate::worker::cache::admin::AdminCache;

pub async fn purge_cache_handler<Cache: AdminCache>(
    Extension(cache): Extension<Arc<RwLock<Cache>>>,
) -> StatusCode {
    cache.write().await.purge();
    StatusCode::OK
}
