//! HTTP client that puts the same bytes on the wire as the TypeScript client, which runs on Bun 1.4.4:
//! Bun's BoringSSL ClientHello, plus the header casing and ordering of Bun's `fetch`.

use std::collections::HashSet;
use std::sync::OnceLock;
use std::time::Duration;

use wreq::header::{HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use wreq::tls::{AlpnProtocol, KeyShare, TlsOptions, TlsVersion};

const CIPHERS: &str = "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:\
ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:\
ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:\
AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA";

const CURVES: &str = "X25519MLKEM768:X25519:P-256:P-384";

const SIGALGS: &str = "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:\
ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:\
rsa_pss_rsae_sha512:rsa_pkcs1_sha512:ed25519:ecdsa_secp521r1_sha512:rsa_pkcs1_sha1";

const KEY_SHARES: &[KeyShare] = &[KeyShare::X25519_MLKEM768, KeyShare::X25519];

/// Header names Bun knows (WebCore's `HTTPHeaderNames`), in the casing it writes them. Any other
/// name goes out as given, which for these requests means lowercase.
const KNOWN_NAMES: &[&str] = &[
    "Accept", "Accept-Charset", "Accept-Encoding", "Accept-Language", "Accept-Ranges",
    "Access-Control-Allow-Credentials", "Access-Control-Allow-Headers", "Access-Control-Allow-Methods",
    "Access-Control-Allow-Origin", "Access-Control-Expose-Headers", "Access-Control-Max-Age",
    "Access-Control-Request-Headers", "Access-Control-Request-Method", "Age", "Authorization",
    "Cache-Control", "Connection", "Content-Disposition", "Content-Encoding", "Content-Language",
    "Content-Length", "Content-Location", "Content-Range", "Content-Security-Policy",
    "Content-Security-Policy-Report-Only", "Content-Type", "Cookie", "Cookie2",
    "Cross-Origin-Embedder-Policy", "Cross-Origin-Embedder-Policy-Report-Only", "Cross-Origin-Opener-Policy",
    "Cross-Origin-Opener-Policy-Report-Only", "Cross-Origin-Resource-Policy", "DNT", "Date", "Default-Style",
    "ETag", "Expect", "Expires", "Host", "If-Match", "If-Modified-Since", "If-None-Match", "If-Range",
    "If-Unmodified-Since", "Keep-Alive", "Last-Event-Id", "Last-Modified", "Link", "Location", "Origin",
    "Ping-From", "Ping-To", "Pragma", "Proxy-Authorization", "Proxy-Connection", "Purpose", "Range", "Referer",
    "Referrer-Policy", "Refresh", "Report-To", "Sec-Fetch-Dest", "Sec-Fetch-Mode", "Sec-WebSocket-Accept",
    "Sec-WebSocket-Extensions", "Sec-WebSocket-Key", "Sec-WebSocket-Protocol", "Sec-WebSocket-Version",
    "Server-Timing", "Service-Worker", "Service-Worker-Allowed", "Service-Worker-Navigation-Preload",
    "Set-Cookie2", "Sourcemap", "Strict-Transport-Security", "TE", "Timing-Allow-Origin", "Trailer",
    "Transfer-Encoding", "Upgrade", "Upgrade-Insecure-Requests", "User-Agent", "Vary", "Via",
    "X-Content-Type-Options", "X-DNS-Prefetch-Control", "X-Frame-Options", "X-Sourcemap", "X-Temp-Tablet",
    "X-XSS-Protection",
];

/// Headers Bun writes after the request's own, in this order. The first four are defaults added only
/// when the request hasn't set them; `None` takes the value the HTTP layer computed.
const TRAILING: &[(&str, Option<&str>)] = &[
    ("Connection", Some("keep-alive")),
    ("Accept", Some("*/*")),
    ("Host", None),
    ("Accept-Encoding", Some("gzip, deflate, br, zstd")),
    ("Content-Length", None),
    ("Transfer-Encoding", None),
];

/// Body framing headers, always written last from the computed values.
fn framing(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-length") || name.eq_ignore_ascii_case("transfer-encoding")
}

pub fn client() -> wreq::Client {
    static CLIENT: OnceLock<wreq::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            // Bun honours NODE_TLS_REJECT_UNAUTHORIZED=0 the same way.
            let verify = std::env::var("NODE_TLS_REJECT_UNAUTHORIZED").map_or(true, |v| v != "0");
            wreq::Client::builder()
                .tls_options(tls_options())
                .tls_cert_verification(verify)
                .tls_verify_hostname(verify)
                .http1_only()
                .redirect(wreq::redirect::Policy::limited(20))
                .referer(false)
                .connect_timeout(Duration::from_secs(30))
                .build()
                .expect("http client")
        })
        .clone()
}

fn tls_options() -> TlsOptions {
    TlsOptions::builder()
        .alpn_protocols([AlpnProtocol::HTTP1])
        .min_tls_version(TlsVersion::TLS_1_2)
        .max_tls_version(TlsVersion::TLS_1_3)
        .cipher_list(CIPHERS)
        .curves_list(CURVES)
        .key_shares(KEY_SHARES)
        .sigalgs_list(SIGALGS)
        .enable_ocsp_stapling(true)
        .enable_signed_cert_timestamps(true)
        .session_ticket(true)
        .pre_shared_key(false)
        .grease_enabled(false)
        .permute_extensions(false)
        // BoringSSL orders the TLS 1.3 suites by whether the CPU has AES instructions.
        .aes_hw_override(aes_hw())
        .build()
}

fn aes_hw() -> bool {
    #[cfg(target_arch = "x86_64")]
    return std::arch::is_x86_feature_detected!("aes") && std::arch::is_x86_feature_detected!("pclmulqdq");
    #[cfg(target_arch = "aarch64")]
    return std::arch::is_aarch64_feature_detected!("aes");
    #[allow(unreachable_code)]
    false
}

/// Sends a request with Bun's header layout. Every request carries an explicit `User-Agent`, since the
/// TypeScript client sets one on all of its requests.
pub async fn send(rb: wreq::RequestBuilder) -> wreq::Result<wreq::Response> {
    let (client, req) = rb.build_split();
    let mut req = req?;
    if !req.headers().contains_key(USER_AGENT) {
        if let Ok(v) = HeaderValue::from_str(&crate::user_agent()) {
            req.headers_mut().insert(USER_AGENT, v);
        }
    }
    let explicit = req.headers().keys().cloned().collect();
    let mut carrier = http::Request::new(());
    wreq_proto::ext::on_preserve_header(&mut carrier, BunHeaders { explicit });
    req.extensions_mut().extend(carrier.into_parts().0.extensions);
    client.execute(req).await
}

/// Writes headers the way Bun's `fetch` does: the request's own headers sorted bytewise by written
/// name, with repeated values joined by ", ", followed by the [`TRAILING`] block.
struct BunHeaders {
    explicit: HashSet<HeaderName>,
}

impl wreq_proto::ext::OnPreserveHeaderCallback for BunHeaders {
    fn call(&self, _headers: &mut HeaderMap) {}

    fn call_visit(&self, headers: &mut HeaderMap, dst: &mut dyn FnMut(&dyn AsRef<[u8]>, &HeaderValue)) {
        let mut own: Vec<(&str, HeaderValue)> = headers
            .keys()
            .filter(|n| self.explicit.contains(*n) && !framing(n.as_str()))
            .map(|n| (wire_name(n), joined(headers, n)))
            .collect();
        own.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        for (name, value) in &own {
            dst(name, value);
        }
        for (name, default) in TRAILING {
            let key = HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()).expect("header name");
            if self.explicit.contains(&key) && !framing(name) {
                continue;
            }
            match default {
                Some(v) => dst(name, &HeaderValue::from_static(v)),
                None if headers.contains_key(&key) => dst(name, &joined(headers, &key)),
                None => {}
            }
        }
        headers.clear();
    }
}

fn wire_name(name: &HeaderName) -> &str {
    KNOWN_NAMES.iter().find(|k| k.eq_ignore_ascii_case(name.as_str())).copied().unwrap_or(name.as_str())
}

fn joined(headers: &HeaderMap, name: &HeaderName) -> HeaderValue {
    let values: Vec<&[u8]> = headers.get_all(name).iter().map(|v| v.as_bytes()).collect();
    HeaderValue::from_bytes(&values.join(&b", "[..])).unwrap_or_else(|_| HeaderValue::from_static(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit(explicit: &[&str], headers: &[(&str, &str)]) -> Vec<String> {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        let cb = BunHeaders { explicit: explicit.iter().map(|k| HeaderName::from_bytes(k.as_bytes()).unwrap()).collect() };
        let mut out = Vec::new();
        wreq_proto::ext::OnPreserveHeaderCallback::call_visit(&cb, &mut map, &mut |n, v| {
            out.push(format!("{}: {}", String::from_utf8_lossy(n.as_ref()), v.to_str().unwrap()))
        });
        out
    }

    #[test]
    fn matches_bun_model_request() {
        // Captured from the TypeScript client's request to OpenCode Zen.
        let own = [
            ("x-session-affinity", "s"), ("user-agent", "ua"), ("authorization", "Bearer public"),
            ("x-opencode-client", "cli"), ("traceparent", "t"), ("content-type", "application/json"), ("b3", "b"),
        ];
        let names: Vec<&str> = own.iter().map(|(k, _)| *k).collect();
        let mut all = own.to_vec();
        all.extend([("host", "opencode.ai"), ("accept-encoding", "gzip, br"), ("content-length", "7")]);
        assert_eq!(
            visit(&names, &all),
            [
                "Authorization: Bearer public", "Content-Type: application/json", "User-Agent: ua", "b3: b",
                "traceparent: t", "x-opencode-client: cli", "x-session-affinity: s", "Connection: keep-alive",
                "Accept: */*", "Host: opencode.ai", "Accept-Encoding: gzip, deflate, br, zstd", "Content-Length: 7",
            ]
        );
    }

    #[test]
    fn explicit_defaults_sort_with_own_headers() {
        let own = [("user-agent", "ua"), ("connection", "close"), ("accept-encoding", "identity"), ("x-dup", "1"), ("x-dup", "2")];
        let mut all = own.to_vec();
        all.push(("host", "h"));
        assert_eq!(
            visit(&["user-agent", "connection", "accept-encoding", "x-dup"], &all),
            ["Accept-Encoding: identity", "Connection: close", "User-Agent: ua", "x-dup: 1, 2", "Accept: */*", "Host: h"]
        );
    }
}
