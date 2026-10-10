//! Embedded UI assets served under content-hashed names.
//!
//! Every asset is compiled into the binary. Its public name carries a hash of
//! its bytes (`/assets/<stem>-<hash>.<ext>`), so a response can be cached
//! forever: a changed file is a changed URL. A request for any other name,
//! including a stale hash, is a 404.

use std::sync::LazyLock;

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// The cache policy for a content-hashed asset.
pub(crate) const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// Hex digits of the content hash kept in a public asset name.
const HASH_LEN: usize = 16;

/// One embedded file, named by its logical name in templates.
struct EmbeddedAsset {
    logical: &'static str,
    content_type: &'static str,
    bytes: &'static [u8],
}

const EMBEDDED: &[EmbeddedAsset] = &[
    EmbeddedAsset {
        logical: "style.css",
        content_type: "text/css; charset=utf-8",
        bytes: include_bytes!("assets/style.css"),
    },
    EmbeddedAsset {
        logical: "htmx.min.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("assets/htmx.min.js"),
    },
    EmbeddedAsset {
        logical: "htmx-sse.min.js",
        content_type: "text/javascript; charset=utf-8",
        bytes: include_bytes!("assets/htmx-sse.min.js"),
    },
];

/// An embedded asset with its hashed public file name.
pub(crate) struct HashedAsset {
    pub(crate) logical: &'static str,
    /// File name under `/assets/`, for example `style-0123456789abcdef.css`.
    pub(crate) file_name: String,
    /// Absolute path the page references.
    pub(crate) url: String,
    content_type: &'static str,
    bytes: &'static [u8],
}

static ASSETS: LazyLock<Vec<HashedAsset>> =
    LazyLock::new(|| EMBEDDED.iter().map(hash_asset).collect());

fn hash_asset(asset: &EmbeddedAsset) -> HashedAsset {
    let digest = hex::encode(Sha256::digest(asset.bytes));
    let hash = &digest[..HASH_LEN];
    let file_name = match asset.logical.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}-{hash}.{ext}"),
        None => format!("{}-{hash}", asset.logical),
    };
    HashedAsset {
        logical: asset.logical,
        url: format!("/assets/{file_name}"),
        file_name,
        content_type: asset.content_type,
        bytes: asset.bytes,
    }
}

/// Every served asset, hashed once per process.
pub(crate) fn all() -> &'static [HashedAsset] {
    &ASSETS
}

/// The hashed URL for a logical asset name. Panics on an unknown name: the
/// names are compile-time constants in the templates, so a miss is a
/// programming error caught by any test that renders the shell.
pub(crate) fn url(logical: &str) -> &'static str {
    all()
        .iter()
        .find(|asset| asset.logical == logical)
        .map(|asset| asset.url.as_str())
        .unwrap_or_else(|| panic!("unknown UI asset {logical}"))
}

/// `GET /assets/{file}`: the exact hashed name or 404.
pub(crate) async fn serve(Path(file): Path<String>) -> Response {
    match all().iter().find(|asset| asset.file_name == file) {
        Some(asset) => (
            [
                (header::CONTENT_TYPE, asset.content_type),
                (header::CACHE_CONTROL, IMMUTABLE_CACHE_CONTROL),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            asset.bytes,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "no-store")],
            "not found",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_carry_a_hash_of_their_bytes() {
        for asset in all() {
            let digest = hex::encode(Sha256::digest(asset.bytes));
            assert!(
                asset.file_name.contains(&digest[..HASH_LEN]),
                "{} must embed its content hash",
                asset.file_name
            );
            assert_eq!(asset.url, format!("/assets/{}", asset.file_name));
        }
        assert!(url("style.css").starts_with("/assets/style-"));
        assert!(url("style.css").ends_with(".css"));
        assert!(url("htmx.min.js").starts_with("/assets/htmx.min-"));
    }
}
