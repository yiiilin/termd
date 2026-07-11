//! 嵌入式 Web 静态资源服务。
//!
//! 发布构建会把 `termui/frontend/dist` 嵌入二进制；本地未构建前端时，build script 会嵌入一个
//! 最小占位页，保证 daemon/relay 的 Rust 构建流程仍然可用。

use axum::body::Body;
use axum::extract::OriginalUri;
use axum::http::header::{
    ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, ETAG,
    HeaderName, IF_NONE_MATCH, VARY,
};
use axum::http::{HeaderMap, Method, Response, StatusCode};
use axum::response::IntoResponse;

#[derive(Clone, Copy)]
pub(crate) struct EmbeddedAsset {
    identity: &'static [u8],
    gzip: Option<&'static [u8]>,
    brotli: Option<&'static [u8]>,
    etag: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentEncoding {
    Brotli,
    Gzip,
    Identity,
}

#[derive(Clone, Copy)]
struct AvailableEncodings {
    brotli: bool,
    gzip: bool,
}

impl AvailableEncodings {
    #[cfg(test)]
    const COMPRESSIBLE: Self = Self {
        brotli: true,
        gzip: true,
    };
    #[cfg(test)]
    const IDENTITY_ONLY: Self = Self {
        brotli: false,
        gzip: false,
    };
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");

pub async fn embedded_web_handler(method: Method, uri: OriginalUri) -> Response<Body> {
    embedded_web_response_with_headers(&method, uri.0.path(), &HeaderMap::new()).await
}

pub async fn embedded_web_handler_with_headers(
    method: Method,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response<Body> {
    embedded_web_response_with_headers(&method, uri.0.path(), &headers).await
}

pub fn embedded_web_response(method: &Method, path: &str) -> Response<Body> {
    let Some((asset, asset_path)) = resolve_asset(method, path) else {
        return fallback_status(method);
    };
    asset_response(asset, &asset_path, ContentEncoding::Identity, method, false)
}

async fn embedded_web_response_with_headers(
    method: &Method,
    path: &str,
    headers: &HeaderMap,
) -> Response<Body> {
    let Some((asset, asset_path)) = resolve_asset(method, path) else {
        return fallback_status(method);
    };
    let available = AvailableEncodings {
        brotli: asset.brotli.is_some(),
        gzip: asset.gzip.is_some(),
    };
    let Some(encoding) = select_content_encoding(headers, available) else {
        return not_acceptable_response();
    };

    let not_modified = if_none_match_matches(headers.get_all(IF_NONE_MATCH).iter(), asset.etag);
    asset_response(asset, &asset_path, encoding, method, not_modified)
}

fn resolve_asset(method: &Method, path: &str) -> Option<(EmbeddedAsset, String)> {
    if method != Method::GET && method != Method::HEAD {
        return None;
    }

    let normalized = normalize_path(path);
    if let Some(asset) = embedded_asset(&normalized) {
        return Some((asset, normalized));
    }

    if let Some(asset_path) = strip_static_asset_prefix(&normalized)
        && let Some(asset) = embedded_asset(asset_path)
    {
        return Some((asset, asset_path.to_owned()));
    }

    if should_fallback_to_index(&normalized)
        && let Some(index) = embedded_asset("index.html")
    {
        return Some((index, "index.html".to_owned()));
    }

    None
}

fn fallback_status(method: &Method) -> Response<Body> {
    if method != Method::GET && method != Method::HEAD {
        StatusCode::METHOD_NOT_ALLOWED.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

fn strip_static_asset_prefix(path: &str) -> Option<&str> {
    let (_mount_prefix, asset_path) = path.split_once('/')?;
    if is_static_asset_path(asset_path) {
        Some(asset_path)
    } else {
        None
    }
}

fn is_static_asset_path(path: &str) -> bool {
    path == "index.html"
        || path == "manifest.webmanifest"
        || path == "service-worker.js"
        || path.starts_with("assets/")
        || path.starts_with("fonts/")
        || path.starts_with("icons/")
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
        || path == "api"
        || path.starts_with("api/")
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

fn asset_response(
    asset: EmbeddedAsset,
    path: &str,
    encoding: ContentEncoding,
    method: &Method,
    not_modified: bool,
) -> Response<Body> {
    let cache_control = cache_control_for(path);
    let bytes = match encoding {
        ContentEncoding::Brotli => asset.brotli.expect("selected brotli representation exists"),
        ContentEncoding::Gzip => asset.gzip.expect("selected gzip representation exists"),
        ContentEncoding::Identity => asset.identity,
    };

    let mut response = Response::builder()
        .status(if not_modified {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        })
        .header(CONTENT_TYPE, content_type_for(path))
        .header(CACHE_CONTROL, cache_control)
        .header(ETAG, asset.etag)
        .header(VARY, "accept-encoding")
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(CONTENT_LENGTH, bytes.len());
    if let Some(value) = encoding.header_value() {
        response = response.header(CONTENT_ENCODING, value);
    }
    let mut response = response
        .body(if method == Method::HEAD || not_modified {
            Body::empty()
        } else {
            Body::from(bytes)
        })
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    if not_modified {
        response.headers_mut().remove(CONTENT_LENGTH);
    }
    response
}

impl ContentEncoding {
    fn header_value(self) -> Option<&'static str> {
        match self {
            Self::Brotli => Some("br"),
            Self::Gzip => Some("gzip"),
            Self::Identity => None,
        }
    }
}

fn select_content_encoding(
    headers: &HeaderMap,
    available: AvailableEncodings,
) -> Option<ContentEncoding> {
    if headers.get_all(ACCEPT_ENCODING).iter().next().is_none() {
        return Some(ContentEncoding::Identity);
    }
    if !headers
        .get_all(ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|item| parse_encoding_member(item).is_some())
    {
        return None;
    }

    [
        (ContentEncoding::Brotli, available.brotli, "br"),
        (ContentEncoding::Gzip, available.gzip, "gzip"),
        (ContentEncoding::Identity, true, "identity"),
    ]
    .into_iter()
    .filter(|(_, available, _)| *available)
    .map(|(encoding, _, name)| (encoding_quality(headers, name), encoding))
    .filter(|(quality, _)| *quality > 0)
    .max_by_key(|(quality, encoding)| (*quality, server_preference(*encoding)))
    .map(|(_, encoding)| encoding)
}

fn server_preference(encoding: ContentEncoding) -> u8 {
    match encoding {
        ContentEncoding::Brotli => 3,
        ContentEncoding::Gzip => 2,
        ContentEncoding::Identity => 1,
    }
}

fn encoding_quality(headers: &HeaderMap, coding: &str) -> u16 {
    let values = headers.get_all(ACCEPT_ENCODING);
    if values.iter().next().is_none() {
        return 1000;
    }

    let mut specific = None;
    let mut wildcard = None;
    for value in values {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for item in value.split(',') {
            let Some((token, quality)) = parse_encoding_member(item) else {
                continue;
            };
            if token.eq_ignore_ascii_case(coding) {
                specific = Some(specific.unwrap_or(0).max(quality));
            } else if token == "*" {
                wildcard = Some(wildcard.unwrap_or(0).max(quality));
            }
        }
    }

    if let Some(quality) = specific {
        return quality;
    }
    if coding.eq_ignore_ascii_case("identity") {
        return if wildcard == Some(0) { 0 } else { 1000 };
    }
    wildcard.unwrap_or(0)
}

fn parse_encoding_member(item: &str) -> Option<(&str, u16)> {
    let mut parts = item.split(';');
    let token = parts.next()?.trim();
    if token.is_empty() || !token.bytes().all(is_http_tchar) {
        return None;
    }

    let Some(parameter) = parts.next() else {
        return Some((token, 1000));
    };
    if parts.next().is_some() {
        return None;
    }
    let (name, value) = parameter.split_once('=')?;
    if !name.trim().eq_ignore_ascii_case("q") {
        return None;
    }
    Some((token, parse_quality(value.trim())?))
}

fn is_http_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn parse_quality(value: &str) -> Option<u16> {
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if fractional.len() > 3 || !fractional.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    match whole {
        "0" => {
            let mut padded = fractional.to_owned();
            while padded.len() < 3 {
                padded.push('0');
            }
            if padded.is_empty() {
                Some(0)
            } else {
                padded.parse().ok()
            }
        }
        "1" if fractional.bytes().all(|byte| byte == b'0') => Some(1000),
        _ => None,
    }
}

fn not_acceptable_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::NOT_ACCEPTABLE)
        .header(VARY, "accept-encoding")
        .body(Body::empty())
        .expect("static response headers are valid")
}

fn if_none_match_matches<'a>(
    values: impl IntoIterator<Item = &'a axum::http::HeaderValue>,
    etag: &str,
) -> bool {
    let etag = etag.strip_prefix("W/").unwrap_or(etag);
    values.into_iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            split_etag_list(value).any(|candidate| {
                candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
            })
        })
    })
}

fn split_etag_list(value: &str) -> impl Iterator<Item = &str> {
    let mut in_quotes = false;
    value
        .split(move |character| {
            if character == '"' {
                in_quotes = !in_quotes;
            }
            character == ',' && !in_quotes
        })
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
}

fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "webmanifest" => "application/manifest+json; charset=utf-8",
        "svg" => "image/svg+xml",
        "txt" => "text/plain; charset=utf-8",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn cache_control_for(path: &str) -> &'static str {
    if is_vite_hashed_asset(path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

fn is_vite_hashed_asset(path: &str) -> bool {
    let Some(file_name) = path.strip_prefix("assets/") else {
        return false;
    };
    if file_name.contains('/') {
        return false;
    }
    let Some((stem, extension)) = file_name.rsplit_once('.') else {
        return false;
    };
    if extension.is_empty() || stem.len() < 10 {
        return false;
    }

    let hash_start = stem.len() - 8;
    stem.as_bytes().get(hash_start.wrapping_sub(1)) == Some(&b'-')
        && stem[hash_start..]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::header::{
        ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, ETAG, IF_NONE_MATCH, VARY,
    };
    use axum::http::{HeaderMap, HeaderValue};
    use brotli::Decompressor;
    use flate2::read::GzDecoder;
    use std::io::Read;
    use std::time::{Duration, Instant};

    const BODY_LIMIT: usize = 32 * 1024 * 1024;

    fn headers(entries: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in entries {
            headers.insert(*name, HeaderValue::from_static(value));
        }
        headers
    }

    async fn body_bytes(response: Response<Body>) -> Vec<u8> {
        to_bytes(response.into_body(), BODY_LIMIT)
            .await
            .expect("response body should be readable")
            .to_vec()
    }

    fn decode_gzip(bytes: &[u8]) -> Vec<u8> {
        let mut decoded = Vec::new();
        GzDecoder::new(bytes)
            .read_to_end(&mut decoded)
            .expect("gzip body should decode");
        decoded
    }

    fn decode_brotli(bytes: &[u8]) -> Vec<u8> {
        let mut decoded = Vec::new();
        Decompressor::new(bytes, 4096)
            .read_to_end(&mut decoded)
            .expect("brotli body should decode");
        decoded
    }

    #[tokio::test]
    async fn root_serves_embedded_index() {
        let response =
            embedded_web_response_with_headers(&Method::GET, "/", &HeaderMap::new()).await;

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
            response
                .headers()
                .get(CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap(),
            "no-cache"
        );
    }

    #[test]
    fn cache_policy_only_marks_vite_hashed_assets_immutable() {
        assert_eq!(cache_control_for("index.html"), "no-cache");
        assert_eq!(cache_control_for("service-worker.js"), "no-cache");
        assert_eq!(cache_control_for("manifest.webmanifest"), "no-cache");
        assert_eq!(
            cache_control_for("fonts/HarmonyOS_Sans_SC_LICENSE.txt"),
            "no-cache"
        );
        assert_eq!(cache_control_for("assets/index.js"), "no-cache");
        assert_eq!(
            cache_control_for("assets/index-BUQLnKZk.js"),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn vite_hash_matcher_accepts_url_safe_dash_and_rejects_false_positives() {
        assert!(is_vite_hashed_asset("assets/index-Abc-12_x.js"));
        assert!(is_vite_hashed_asset("assets/index-BD_xmwCJ.js"));
        assert!(!is_vite_hashed_asset("index-BUQLnKZk.js"));
        assert!(!is_vite_hashed_asset("assets/index.js"));
        assert!(!is_vite_hashed_asset("assets/index.bundle.js"));
        assert!(!is_vite_hashed_asset("assets/not-hashed-file.js"));
        assert!(!is_vite_hashed_asset("assets/index-abc.js"));
        assert!(!is_vite_hashed_asset("fonts/font-BUQLnKZk.ttf"));
    }

    #[test]
    fn text_files_use_utf8_plain_text_mime() {
        assert_eq!(content_type_for("NOTICE.txt"), "text/plain; charset=utf-8");
    }

    #[tokio::test]
    async fn known_api_and_websocket_prefixes_do_not_fallback_to_index() {
        assert_eq!(
            embedded_web_response_with_headers(&Method::GET, "/ws", &HeaderMap::new())
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            embedded_web_response_with_headers(&Method::GET, "/api", &HeaderMap::new())
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            embedded_web_response_with_headers(&Method::GET, "/api/", &HeaderMap::new())
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            embedded_web_response_with_headers(
                &Method::GET,
                "/api/control/session/list",
                &HeaderMap::new(),
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            embedded_web_response_with_headers(
                &Method::GET,
                "/local/pairing-token",
                &HeaderMap::new(),
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn extensionless_browser_paths_fallback_to_index() {
        assert_eq!(
            embedded_web_response_with_headers(&Method::GET, "/terminal", &HeaderMap::new())
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn prefixed_static_assets_are_served() {
        assert_eq!(
            embedded_web_response_with_headers(
                &Method::GET,
                "/termd/index.html",
                &HeaderMap::new(),
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            strip_static_asset_prefix("termd/service-worker.js"),
            Some("service-worker.js")
        );
        assert_eq!(
            strip_static_asset_prefix("termd/fonts/HarmonyOS_Sans_SC_Regular.ttf"),
            Some("fonts/HarmonyOS_Sans_SC_Regular.ttf")
        );
        assert_eq!(
            strip_static_asset_prefix("termd/assets/index.js"),
            Some("assets/index.js")
        );
        assert_eq!(
            strip_static_asset_prefix("termd/icons/termd.svg"),
            Some("icons/termd.svg")
        );
        assert_eq!(
            strip_static_asset_prefix("termd/manifest.webmanifest"),
            Some("manifest.webmanifest")
        );
        assert_eq!(
            strip_static_asset_prefix("nested/termd/assets/index.js"),
            None
        );
    }

    #[tokio::test]
    async fn etag_miss_returns_body_and_hit_returns_header_complete_304() {
        let response =
            embedded_web_response_with_headers(&Method::GET, "/", &HeaderMap::new()).await;
        let etag = response.headers().get(ETAG).cloned().expect("ETag");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!body_bytes(response).await.is_empty());

        let mut request_headers = HeaderMap::new();
        request_headers.insert(IF_NONE_MATCH, etag.clone());
        let response =
            embedded_web_response_with_headers(&Method::GET, "/", &request_headers).await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers().get(ETAG), Some(&etag));
        assert_eq!(response.headers().get(CACHE_CONTROL).unwrap(), "no-cache");
        assert_eq!(response.headers().get(VARY).unwrap(), "accept-encoding");
        assert!(response.headers().contains_key(CONTENT_TYPE));
        assert!(!response.headers().contains_key(CONTENT_LENGTH));
        assert_eq!(
            response.headers().get(X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        assert!(body_bytes(response).await.is_empty());
    }

    #[tokio::test]
    async fn weak_etag_revalidates_across_content_encodings() {
        let identity = embedded_web_response_with_headers(
            &Method::GET,
            "/",
            &headers(&[(ACCEPT_ENCODING.as_str(), "identity")]),
        )
        .await;
        let etag = identity.headers().get(ETAG).cloned().expect("ETag");
        assert!(
            etag.as_bytes().starts_with(b"W/\"sha256-"),
            "a validator shared by encoded representations must be weak"
        );

        for encoding in ["gzip", "br"] {
            let mut request_headers = headers(&[(ACCEPT_ENCODING.as_str(), encoding)]);
            request_headers.insert(IF_NONE_MATCH, etag.clone());
            let response =
                embedded_web_response_with_headers(&Method::GET, "/", &request_headers).await;
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{encoding}");
            assert_eq!(response.headers().get(ETAG), Some(&etag), "{encoding}");
            assert!(body_bytes(response).await.is_empty(), "{encoding}");
        }
    }

    #[tokio::test]
    async fn rejects_requests_when_no_supported_representation_is_acceptable() {
        for value in [
            "identity;q=0",
            "identity;q=0, gzip;q=0, br;q=0",
            "*;q=0",
            "identity;q=0, *;q=0",
            "identity;q=0, gzip;q=0, br;q=0, *;q=1",
        ] {
            let response = embedded_web_response_with_headers(
                &Method::GET,
                "/",
                &headers(&[(ACCEPT_ENCODING.as_str(), value)]),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_ACCEPTABLE,
                "Accept-Encoding: {value}"
            );
            assert!(body_bytes(response).await.is_empty(), "{value}");
        }

        let gzip = embedded_web_response_with_headers(
            &Method::GET,
            "/",
            &headers(&[(ACCEPT_ENCODING.as_str(), "identity;q=0, gzip;q=1, br;q=0")]),
        )
        .await;
        assert_eq!(gzip.status(), StatusCode::OK);
        assert_eq!(gzip.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
    }

    #[test]
    fn accept_encoding_selector_honors_explicit_wildcard_and_server_preference() {
        for (value, expected) in [
            ("identity;q=0, *;q=1", Some(ContentEncoding::Brotli)),
            (
                "identity;q=0, gzip;q=0.8, br;q=0.8",
                Some(ContentEncoding::Brotli),
            ),
            ("gzip;q=1, br;q=0.5", Some(ContentEncoding::Gzip)),
            ("br;q=0, gzip;q=0, identity;q=0", None),
        ] {
            assert_eq!(
                select_content_encoding(
                    &headers(&[(ACCEPT_ENCODING.as_str(), value)]),
                    AvailableEncodings::COMPRESSIBLE,
                ),
                expected,
                "Accept-Encoding: {value}"
            );
        }

        assert_eq!(
            select_content_encoding(
                &headers(&[(ACCEPT_ENCODING.as_str(), "identity;q=0, *;q=1")]),
                AvailableEncodings::IDENTITY_ONLY,
            ),
            None,
        );
    }

    #[test]
    fn accept_encoding_selector_discards_members_with_invalid_parameters() {
        for value in [
            "identity;q=0, br;q=0, gzip;q",
            "identity;q=0, br;q=0, gzip;foo",
            "identity;q=0, br;q=0, gzip;q=1;q=0",
            "identity;q=0, br;q=0, gzip;q=bogus",
        ] {
            assert_eq!(
                select_content_encoding(
                    &headers(&[(ACCEPT_ENCODING.as_str(), value)]),
                    AvailableEncodings::COMPRESSIBLE,
                ),
                None,
                "Accept-Encoding: {value}"
            );
        }

        for (value, expected) in [
            (
                "identity;q=0, gzip;q, Br ; Q = 0.123",
                Some(ContentEncoding::Brotli),
            ),
            (
                "identity;q=0, gzip;foo, * ; q = 0.001",
                Some(ContentEncoding::Brotli),
            ),
            (
                "identity;q=0, gzip;q=1;q=0, GZIP ; q=0.999",
                Some(ContentEncoding::Gzip),
            ),
        ] {
            assert_eq!(
                select_content_encoding(
                    &headers(&[(ACCEPT_ENCODING.as_str(), value)]),
                    AvailableEncodings::COMPRESSIBLE,
                ),
                expected,
                "Accept-Encoding: {value}"
            );
        }
    }

    #[test]
    fn accept_encoding_member_requires_an_http_tchar_coding() {
        for invalid in ["", " ", "gzip q=1", "gzip()", "gzip(", "压缩"] {
            assert_eq!(parse_encoding_member(invalid), None, "member: {invalid:?}");
        }

        for valid in [
            "gzip",
            "GZIP ; q=0.123",
            "!#$%&'*+-.^_`|~",
            "abcXYZ012",
            "*;q=1",
        ] {
            assert!(parse_encoding_member(valid).is_some(), "member: {valid:?}");
        }
    }

    #[tokio::test]
    async fn invalid_accept_encoding_members_produce_406_when_nothing_else_is_acceptable() {
        for value in [
            "gzip;q",
            "gzip;foo",
            "gzip;q=1;q=0",
            "gzip;q=bogus",
            "gzip q=1",
        ] {
            let response = embedded_web_response_with_headers(
                &Method::GET,
                "/",
                &headers(&[(ACCEPT_ENCODING.as_str(), value)]),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_ACCEPTABLE,
                "Accept-Encoding: {value}"
            );
        }

        let missing =
            embedded_web_response_with_headers(&Method::GET, "/", &HeaderMap::new()).await;
        assert_eq!(missing.status(), StatusCode::OK);
        assert!(missing.headers().get(CONTENT_ENCODING).is_none());

        for (value, expected) in [
            ("identity;q=0, gzip;q, br;q=0.5", "br"),
            ("identity;q=0, gzip;foo, gzip;q=0.5", "gzip"),
            ("identity;q=0, gzip q=1, br;q=0.5", "br"),
        ] {
            let response = embedded_web_response_with_headers(
                &Method::GET,
                "/",
                &headers(&[(ACCEPT_ENCODING.as_str(), value)]),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{value}");
            assert_eq!(response.headers().get(CONTENT_ENCODING).unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn if_none_match_supports_lists_wildcard_and_weak_comparison() {
        let initial =
            embedded_web_response_with_headers(&Method::GET, "/", &HeaderMap::new()).await;
        let etag = initial.headers().get(ETAG).unwrap().to_str().unwrap();
        let strong_candidate = etag.strip_prefix("W/").expect("asset ETag should be weak");
        let weak_list = format!("\"other\", {strong_candidate}, \"last\"");

        for value in [weak_list.as_str(), "*"] {
            let mut request_headers = HeaderMap::new();
            request_headers.insert(IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
            let response =
                embedded_web_response_with_headers(&Method::HEAD, "/", &request_headers).await;
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{value}");
        }

        let miss = headers(&[(IF_NONE_MATCH.as_str(), "\"not-this-asset\"")]);
        let response = embedded_web_response_with_headers(&Method::GET, "/", &miss).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn uncompressible_font_rejects_an_unacceptable_identity_result() {
        let request_headers =
            headers(&[(ACCEPT_ENCODING.as_str(), "identity;q=0, gzip;q=1, br;q=0")]);
        assert_eq!(
            select_content_encoding(&request_headers, AvailableEncodings::IDENTITY_ONLY),
            None
        );
    }

    #[tokio::test]
    async fn head_matches_uncompressed_get_length_without_a_body() {
        let request_headers = headers(&[(ACCEPT_ENCODING.as_str(), "identity")]);
        let get = embedded_web_response_with_headers(&Method::GET, "/", &request_headers).await;
        let get_length = get.headers().get(CONTENT_LENGTH).cloned().unwrap();
        let get_body = body_bytes(get).await;
        assert_eq!(
            get_length.to_str().unwrap().parse::<usize>().unwrap(),
            get_body.len()
        );

        let head = embedded_web_response_with_headers(&Method::HEAD, "/", &request_headers).await;
        assert_eq!(head.headers().get(CONTENT_LENGTH), Some(&get_length));
        assert!(head.headers().get(CONTENT_ENCODING).is_none());
        assert!(body_bytes(head).await.is_empty());
    }

    #[tokio::test]
    async fn head_matches_compressed_get_length_without_a_body() {
        let request_headers = headers(&[(ACCEPT_ENCODING.as_str(), "gzip")]);
        let get = embedded_web_response_with_headers(&Method::GET, "/", &request_headers).await;
        assert_eq!(get.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
        let get_length = get.headers().get(CONTENT_LENGTH).cloned().unwrap();
        let get_body = body_bytes(get).await;
        assert_eq!(
            get_length.to_str().unwrap().parse::<usize>().unwrap(),
            get_body.len()
        );

        let head = embedded_web_response_with_headers(&Method::HEAD, "/", &request_headers).await;
        assert_eq!(head.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
        assert_eq!(head.headers().get(CONTENT_LENGTH), Some(&get_length));
        assert!(body_bytes(head).await.is_empty());
    }

    #[tokio::test]
    async fn gzip_and_brotli_bodies_decode_to_the_original_asset() {
        let identity = embedded_web_response_with_headers(
            &Method::GET,
            "/",
            &headers(&[(ACCEPT_ENCODING.as_str(), "identity")]),
        )
        .await;
        let original = body_bytes(identity).await;

        let gzip = embedded_web_response_with_headers(
            &Method::GET,
            "/",
            &headers(&[(ACCEPT_ENCODING.as_str(), "gzip")]),
        )
        .await;
        assert_eq!(gzip.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
        assert_eq!(decode_gzip(&body_bytes(gzip).await), original);

        let brotli = embedded_web_response_with_headers(
            &Method::GET,
            "/",
            &headers(&[(ACCEPT_ENCODING.as_str(), "br")]),
        )
        .await;
        assert_eq!(brotli.headers().get(CONTENT_ENCODING).unwrap(), "br");
        assert_eq!(decode_brotli(&body_bytes(brotli).await), original);
    }

    #[tokio::test]
    async fn large_asset_head_and_304_use_static_representations_without_runtime_work() {
        const LARGE_ASSET: &str = "/assets/ts.worker-BH9nVgjN.js";
        let identity = embedded_web_response_with_headers(
            &Method::GET,
            LARGE_ASSET,
            &headers(&[(ACCEPT_ENCODING.as_str(), "identity")]),
        )
        .await;
        assert!(
            identity
                .headers()
                .get(CONTENT_LENGTH)
                .unwrap()
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap()
                > 7_000_000
        );
        let etag = identity.headers().get(ETAG).cloned().unwrap();

        let started = Instant::now();
        let head = embedded_web_response_with_headers(
            &Method::HEAD,
            LARGE_ASSET,
            &headers(&[(ACCEPT_ENCODING.as_str(), "gzip")]),
        )
        .await;
        let mut not_modified_headers = headers(&[(ACCEPT_ENCODING.as_str(), "br")]);
        not_modified_headers.insert(IF_NONE_MATCH, etag);
        let not_modified =
            embedded_web_response_with_headers(&Method::GET, LARGE_ASSET, &not_modified_headers)
                .await;

        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers().get(CONTENT_ENCODING).unwrap(), "gzip");
        assert!(body_bytes(head).await.is_empty());
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(not_modified.headers().get(CONTENT_ENCODING).unwrap(), "br");
        assert!(body_bytes(not_modified).await.is_empty());
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "HEAD and 304 performed runtime-sized work: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn font_license_and_notice_paths_have_stable_web_metadata() {
        assert_eq!(
            content_type_for("fonts/HarmonyOS_Sans_SC_LICENSE.txt"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            cache_control_for("fonts/HarmonyOS_Sans_SC_LICENSE.txt"),
            "no-cache"
        );
        assert!(include_str!("../../THIRD_PARTY_NOTICES.md").contains("HarmonyOS Sans"));
        assert!(include_str!("../../README.md").contains("THIRD_PARTY_NOTICES.md"));
    }
}
