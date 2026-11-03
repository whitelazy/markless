//! Fetching remote images (http/https URLs).

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use image::DynamicImage;

/// Return true when `src` is a remote image (an http or https URL).
pub fn is_remote_image(src: &str) -> bool {
    let has_scheme = |scheme: &str| {
        src.get(..scheme.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(scheme))
    };
    has_scheme("https://") || has_scheme("http://")
}

/// One HTTP agent for all fetches, so connections and TLS setup are reused.
static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .max_redirects(5)
        .build()
        .into()
});

/// Time limits for one remote image fetch.
#[derive(Debug, Clone, Copy)]
pub struct FetchTimeouts {
    /// Opening the TCP (and TLS) connection.
    pub connect: Duration,
    /// Waiting for the response headers after sending the request.
    pub first_response: Duration,
    /// The whole fetch, including a slow body.
    pub total: Duration,
}

/// Timeouts used for real remote image fetches.
pub const REMOTE_IMAGE_TIMEOUTS: FetchTimeouts = FetchTimeouts {
    connect: Duration::from_secs(10),
    first_response: Duration::from_secs(10),
    total: Duration::from_mins(1),
};

/// Largest remote image body we accept.
const MAX_BODY_BYTES: u64 = 20 * 1024 * 1024;

/// Fetch and decode a remote image within `timeouts`.
///
/// Blocking: call it off the event loop thread.
///
/// # Errors
///
/// Returns an error on network failure, timeout, non-2xx status, a body
/// over 20MB, or bytes that do not decode as an image.
pub fn fetch_remote_image(url: &str, timeouts: FetchTimeouts) -> Result<DynamicImage> {
    let bytes = AGENT
        .get(url)
        .config()
        .timeout_connect(Some(timeouts.connect))
        .timeout_recv_response(Some(timeouts.first_response))
        .timeout_global(Some(timeouts.total))
        .build()
        .call()?
        .body_mut()
        .with_config()
        // ureq rejects a body whose size equals the limit; allow exactly 20MB.
        .limit(MAX_BODY_BYTES + 1)
        .read_to_vec()?;
    Ok(image::load_from_memory(&bytes)?)
}

/// Host part of a remote image URL, for messages; the whole string if it does not parse.
pub fn host_of(url: &str) -> String {
    url.parse::<ureq::http::Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// Short reason for a failed [`fetch_remote_image`], such as `404` or `timeout`.
pub fn failure_reason(err: &anyhow::Error) -> String {
    if err.downcast_ref::<image::ImageError>().is_some() {
        return "not an image".to_string();
    }
    match err.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::StatusCode(code)) => code.to_string(),
        Some(ureq::Error::Timeout(_)) => "timeout".to_string(),
        Some(ureq::Error::Io(io)) if io.kind() == std::io::ErrorKind::TimedOut => {
            "timeout".to_string()
        }
        Some(ureq::Error::Io(io)) if io.kind() == std::io::ErrorKind::ConnectionRefused => {
            "connection failed".to_string()
        }
        Some(ureq::Error::BodyExceedsLimit(_)) => "too large".to_string(),
        Some(ureq::Error::TooManyRedirects) => "too many redirects".to_string(),
        Some(ureq::Error::HostNotFound) => "host not found".to_string(),
        Some(ureq::Error::ConnectionFailed) => "connection failed".to_string(),
        Some(_) => "network error".to_string(),
        None => "error".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

    /// The same limit for connect, first response, and whole transfer.
    fn all(t: Duration) -> FetchTimeouts {
        FetchTimeouts {
            connect: t,
            first_response: t,
            total: t,
        }
    }

    /// Accept one connection and hand the stream to `respond` after reading the request.
    fn serve_stream(respond: impl FnOnce(std::net::TcpStream) + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = [0_u8; 1024];
            let _ = stream.read(&mut req);
            respond(stream);
        });
        format!("http://{addr}/img.png")
    }

    #[test]
    fn test_fetch_remote_image_first_response_after_limit_fails() {
        let url = serve_stream(|mut stream| {
            std::thread::sleep(Duration::from_secs(2));
            let _ = stream.write_all(&response("image/png", &png_bytes()));
        });
        let start = std::time::Instant::now();
        let result = fetch_remote_image(
            &url,
            FetchTimeouts {
                connect: Duration::from_secs(5),
                first_response: Duration::from_millis(200),
                total: Duration::from_secs(5),
            },
        );
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn test_fetch_remote_image_slow_body_within_total_succeeds() {
        let url = serve_stream(|mut stream| {
            let bytes = response("image/png", &png_bytes());
            let split = bytes.len() - 20;
            stream.write_all(&bytes[..split]).unwrap();
            // Body trickles in well past the first-response limit.
            for chunk in bytes[split..].chunks(4) {
                std::thread::sleep(Duration::from_millis(150));
                stream.write_all(chunk).unwrap();
            }
        });
        let result = fetch_remote_image(
            &url,
            FetchTimeouts {
                connect: Duration::from_secs(5),
                first_response: Duration::from_millis(200),
                total: Duration::from_secs(5),
            },
        );
        assert!(result.is_ok(), "{result:?}");
    }

    fn png_bytes() -> Vec<u8> {
        let img = DynamicImage::new_rgba8(3, 2);
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
        buf.into_inner()
    }

    /// Serve `body` once on a local port and return the URL.
    fn serve_once(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = [0_u8; 1024];
            let _ = stream.read(&mut req);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        format!("http://{addr}/tux.png")
    }

    /// Serve every connection with `respond(path)` (raw HTTP response bytes).
    fn serve_with(respond: impl Fn(&str) -> Vec<u8> + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut req = [0_u8; 1024];
                let n = stream.read(&mut req).unwrap_or(0);
                let req = String::from_utf8_lossy(&req[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                // Client may hang up early (limits); ignore write errors.
                let _ = stream.write_all(&respond(&path));
            }
        });
        format!("http://{addr}")
    }

    fn response(content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// `/r/N` redirects to `/r/N-1`; `/r/0` serves a PNG.
    fn redirect_chain_url(hops: usize) -> String {
        let base = serve_with(|path| {
            let n: usize = path.trim_start_matches("/r/").parse().unwrap_or(0);
            if n == 0 {
                response("image/png", &png_bytes())
            } else {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: /r/{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    n - 1
                )
                .into_bytes()
            }
        });
        format!("{base}/r/{hops}")
    }

    #[test]
    fn test_fetch_remote_image_body_over_20mb_fails() {
        // A valid PNG padded past 20MB: only the size limit can reject it.
        let mut big = png_bytes();
        big.resize(20 * 1024 * 1024 + 1, 0);
        let base = serve_with(move |_| response("image/png", &big));
        assert!(
            fetch_remote_image(&format!("{base}/big.png"), all(Duration::from_secs(5))).is_err()
        );
    }

    #[test]
    fn test_fetch_remote_image_body_at_20mb_decodes() {
        let mut big = png_bytes();
        big.resize(20 * 1024 * 1024, 0);
        let base = serve_with(move |_| response("image/png", &big));
        fetch_remote_image(&format!("{base}/big.png"), all(Duration::from_secs(5))).unwrap();
    }

    #[test]
    fn test_fetch_remote_image_non_image_body_fails() {
        let base = serve_with(|_| response("text/html", b"<html>not an image</html>"));
        assert!(fetch_remote_image(&format!("{base}/page"), all(Duration::from_secs(5))).is_err());
    }

    #[test]
    fn test_fetch_remote_image_slow_server_times_out() {
        let base = serve_with(|_| {
            std::thread::sleep(Duration::from_secs(3));
            response("image/png", &png_bytes())
        });
        let start = std::time::Instant::now();
        let result =
            fetch_remote_image(&format!("{base}/slow.png"), all(Duration::from_millis(200)));
        assert!(result.is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn test_fetch_remote_image_five_redirects_succeeds() {
        assert!(fetch_remote_image(&redirect_chain_url(5), all(Duration::from_secs(5))).is_ok());
    }

    #[test]
    fn test_fetch_remote_image_six_redirects_fails() {
        assert!(fetch_remote_image(&redirect_chain_url(6), all(Duration::from_secs(5))).is_err());
    }

    #[test]
    fn test_fetch_remote_image_non_http_scheme_fails() {
        assert!(fetch_remote_image("ftp://127.0.0.1/a.png", all(Duration::from_secs(1))).is_err());
        assert!(fetch_remote_image("file:///etc/hosts", all(Duration::from_secs(1))).is_err());
    }

    #[test]
    fn test_fetch_remote_image_from_local_server_decodes_png() {
        let url = serve_once(png_bytes());
        let img = fetch_remote_image(&url, all(Duration::from_secs(5))).unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
    }

    fn reason_for(url: &str, timeout: Duration) -> String {
        failure_reason(&fetch_remote_image(url, all(timeout)).unwrap_err())
    }

    #[test]
    fn test_failure_reason_names_status_timeout_size_and_format() {
        let base = serve_with(|path| match path {
            "/missing.png" => {
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
            }
            "/slow.png" => {
                std::thread::sleep(Duration::from_secs(2));
                response("image/png", &png_bytes())
            }
            "/big.png" => response("image/png", &vec![0; 20 * 1024 * 1024 + 1]),
            _ => response("text/html", b"<html></html>"),
        });
        let t = Duration::from_secs(5);
        assert_eq!(reason_for(&format!("{base}/missing.png"), t), "404");
        assert_eq!(
            reason_for(&format!("{base}/slow.png"), Duration::from_millis(200)),
            "timeout"
        );
        assert_eq!(reason_for(&format!("{base}/big.png"), t), "too large");
        assert_eq!(reason_for(&format!("{base}/page"), t), "not an image");
    }

    #[test]
    fn test_failure_reason_connection_refused() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = format!("http://127.0.0.1:{port}/x.png");
        assert_eq!(
            reason_for(&url, Duration::from_secs(5)),
            "connection failed"
        );
    }

    #[test]
    fn test_host_of_returns_url_host() {
        assert_eq!(
            host_of("https://example.com:8080/a/b.png?x=1"),
            "example.com"
        );
        assert_eq!(host_of("not a url"), "not a url");
    }

    #[test]
    fn test_is_remote_image_matches_http_and_https_only() {
        assert!(is_remote_image("https://example.com/a.png"));
        assert!(is_remote_image("http://example.com/a.png"));
        assert!(!is_remote_image("images/a.png"));
        assert!(!is_remote_image("mermaid://0"));
        assert!(!is_remote_image("file:///tmp/a.png"));
    }

    #[test]
    fn test_is_remote_image_scheme_case_insensitive() {
        assert!(is_remote_image("HTTPS://example.com/a.png"));
        assert!(is_remote_image("Http://example.com/a.png"));
        assert!(!is_remote_image("//example.com/a.png"));
        assert!(!is_remote_image("HTTP:/x"));
    }

    #[test]
    fn test_fetch_remote_image_uppercase_scheme_decodes_png() {
        let url = serve_once(png_bytes()).replacen("http://", "HTTP://", 1);
        assert!(fetch_remote_image(&url, all(Duration::from_secs(5))).is_ok());
    }
}
