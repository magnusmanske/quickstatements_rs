//! The frontend files in `public_html`, built into the binary

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

const HTML: &str = "text/html; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

/// (URL path, content type, content)
type Asset = (&'static str, &'static str, &'static [u8]);

macro_rules! assets {
    ($($file:literal => $content_type:expr),* $(,)?) => {
        &[$((
            concat!("/", $file),
            $content_type,
            include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/public_html/", $file)),
        )),*]
    };
}

static ASSETS: &[Asset] = assets![
    "index.html" => HTML,
    "quickstatements.css" => CSS,
    "vue.js" => JS,
    "vue-component-loader.js" => JS,
    "vue_components/batch-commands.html" => HTML,
    "vue_components/batch.html" => HTML,
    "vue_components/batch_access_mixin.html" => HTML,
    "vue_components/batches.html" => HTML,
    "vue_components/command.html" => HTML,
    "vue_components/main-page.html" => HTML,
    "vue_components/user-page.html" => HTML,
    "vue_components/user.html" => HTML,
];

pub async fn serve(uri: Uri) -> Response {
    let path = match uri.path() {
        "/" => "/index.html",
        path => path,
    };
    match ASSETS.iter().find(|(asset_path, ..)| *asset_path == path) {
        Some((_, content_type, content)) => (
            [
                (header::CONTENT_TYPE, *content_type),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            *content,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn files_in(dir: &Path, base: &Path, files: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                files_in(&path, base, files);
            } else {
                let relative = path.strip_prefix(base).unwrap().to_string_lossy();
                files.push(format!("/{relative}"));
            }
        }
    }

    #[test]
    fn every_frontend_file_is_served() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("public_html");
        let mut files = vec![];
        files_in(&base, &base, &mut files);
        files.sort();
        let mut served: Vec<String> = ASSETS.iter().map(|(path, ..)| path.to_string()).collect();
        served.sort();
        assert_eq!(files, served);
    }

    #[tokio::test]
    async fn serves_index_and_rejects_others() {
        let response = serve(Uri::from_static("/")).await;
        assert_eq!(response.headers()[header::CONTENT_TYPE], HTML);
        let response = serve(Uri::from_static("/config.json")).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
