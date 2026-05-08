//! 嵌入式 Web 静态资源服务。
//!
//! 发布构建会把 `termui/frontend/dist` 嵌入二进制；本地未构建前端时，build script 会嵌入一个
//! 最小占位页，保证 daemon/relay 的 Rust 构建流程仍然可用。

use axum::body::Body;
use axum::extract::OriginalUri;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName};
use axum::http::{Method, Response, StatusCode};
use axum::response::IntoResponse;

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");

pub async fn embedded_web_handler(method: Method, uri: OriginalUri) -> Response<Body> {
    embedded_web_response(&method, uri.0.path())
}

pub fn embedded_web_response(method: &Method, path: &str) -> Response<Body> {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let normalized = normalize_path(path);
    if let Some(asset) = embedded_asset(&normalized) {
        return asset_response(asset, &normalized, method == Method::HEAD);
    }

    if should_fallback_to_index(&normalized) {
        if let Some(index) = embedded_asset("index.html") {
            return asset_response(index, "index.html", method == Method::HEAD);
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        "index.html".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn should_fallback_to_index(path: &str) -> bool {
    if path.starts_with("assets/")
        || path.starts_with("ws")
        || path.starts_with("healthz")
        || path.starts_with("local/")
    {
        return false;
    }

    path == "index.html"
        || path.ends_with('/')
        || path
            .rsplit('/')
            .next()
            .is_some_and(|segment| !segment.contains('.'))
}

fn asset_response(asset: &'static [u8], path: &str, head_only: bool) -> Response<Body> {
    let content_type = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();
    let cache_control = cache_control_for(path);

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, cache_control)
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(if head_only {
            Body::empty()
        } else {
            Body::from(asset)
        })
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn cache_control_for(_path: &str) -> &'static str {
    // Web 资源随 termd/termrelay 二进制一起发布，MVP 阶段优先避免浏览器长期持有旧 bundle。
    // 旧 JS 一旦被 immutable 缓存，会让已经修复的前端逻辑继续在用户浏览器里报错。
    "no-store"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_serves_embedded_index() {
        let response = embedded_web_response(&Method::GET, "/");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/html")
        );
        assert_eq!(
            response.headers().get(CACHE_CONTROL).unwrap().to_str().unwrap(),
            "no-store"
        );
    }

    #[test]
    fn embedded_assets_do_not_use_long_lived_cache() {
        assert_eq!(cache_control_for("index.html"), "no-store");
        assert_eq!(cache_control_for("assets/index.js"), "no-store");
        assert_eq!(cache_control_for("assets/index.css"), "no-store");
    }

    #[test]
    fn known_api_prefixes_do_not_fallback_to_index() {
        assert_eq!(
            embedded_web_response(&Method::GET, "/ws").status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            embedded_web_response(&Method::GET, "/local/pairing-token").status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn extensionless_browser_paths_fallback_to_index() {
        assert_eq!(
            embedded_web_response(&Method::GET, "/terminal").status(),
            StatusCode::OK
        );
    }
}
