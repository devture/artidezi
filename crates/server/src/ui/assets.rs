// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use std::sync::LazyLock;

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use sha2::{Digest, Sha256};

pub(super) struct Asset {
    pub url: String,
    extension: &'static str,
    content_type: &'static str,
    content: &'static str,
}

impl Asset {
    fn new(extension: &'static str, content_type: &'static str, content: &'static str) -> Self {
        let digest = hex::encode(Sha256::digest(content.as_bytes()));
        Self {
            url: format!("/assets/app.{digest}.{extension}"),
            extension,
            content_type,
            content,
        }
    }
}

pub(super) static CSS: LazyLock<Asset> =
    LazyLock::new(|| Asset::new("css", "text/css; charset=utf-8", include_str!("style.css")));
pub(super) static JS: LazyLock<Asset> = LazyLock::new(|| {
    Asset::new(
        "js",
        "text/javascript; charset=utf-8",
        include_str!("app.js"),
    )
});
pub(super) static LOGO: LazyLock<Asset> = LazyLock::new(|| {
    Asset::new(
        "svg",
        "image/svg+xml",
        include_str!("../../../../assets/branding/artidezi-bot.svg"),
    )
});

/// Only this handler can mark embedded, content-matched responses as publicly cacheable.
#[derive(Clone)]
pub(super) struct ImmutableAsset;

pub(super) async fn serve(Path(name): Path<String>) -> Response {
    for asset in [&*CSS, &*JS, &*LOGO] {
        let matching = asset.url.strip_prefix("/assets/") == Some(name.as_str());
        if matching || name == format!("app.{}", asset.extension) {
            let mut response =
                ([(header::CONTENT_TYPE, asset.content_type)], asset.content).into_response();
            if matching {
                response.extensions_mut().insert(ImmutableAsset);
            }
            return response;
        }
        // Old pages can request an asset from an earlier binary. Redirect without caching the
        // old address; never serve changed bytes under another content hash with immutable caching.
        if name
            .strip_prefix("app.")
            .and_then(|rest| rest.strip_suffix(&format!(".{}", asset.extension)))
            .is_some_and(|hash| {
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Redirect::temporary(&asset.url).into_response();
        }
    }
    (StatusCode::NOT_FOUND, "Asset not found.").into_response()
}
