//! Serves the static product/download page at the site root, embedded into
//! the binary at compile time (like `VIEWER_HTML` in `main.rs`) so there's
//! nothing extra to copy or mount alongside the backend at deploy time.
//!
//! The download button on the page fetches GitHub's releases API
//! client-side for the current `framewire.exe` asset URL, so this page
//! never needs to change (or the backend redeployed) when a new version
//! ships.

use std::sync::Arc;

use axum::http::header;
use axum::response::Html;
use axum::routing::get;
use axum::Router;

use crate::AppState;

const INDEX_HTML: &str = include_str!("../site/index.html");
const STYLES_CSS: &str = include_str!("../site/styles.css");
const APP_JS: &str = include_str!("../site/app.js");
const FAVICON_PNG: &[u8] = include_bytes!("../site/favicon.png");
const FONT_LIGHT: &[u8] = include_bytes!("../site/fonts/Inter-Light.woff2");
const FONT_REGULAR: &[u8] = include_bytes!("../site/fonts/Inter-Regular.woff2");
const FONT_BOLD: &[u8] = include_bytes!("../site/fonts/Inter-Bold.woff2");

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(|| async { Html(INDEX_HTML) }))
        .route(
            "/styles.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css")], STYLES_CSS) }),
        )
        .route(
            "/app.js",
            get(|| async { ([(header::CONTENT_TYPE, "application/javascript")], APP_JS) }),
        )
        .route(
            "/favicon.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], FAVICON_PNG) }),
        )
        .route(
            "/fonts/Inter-Light.woff2",
            get(|| async { ([(header::CONTENT_TYPE, "font/woff2")], FONT_LIGHT) }),
        )
        .route(
            "/fonts/Inter-Regular.woff2",
            get(|| async { ([(header::CONTENT_TYPE, "font/woff2")], FONT_REGULAR) }),
        )
        .route(
            "/fonts/Inter-Bold.woff2",
            get(|| async { ([(header::CONTENT_TYPE, "font/woff2")], FONT_BOLD) }),
        )
}
