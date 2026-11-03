//! Fetching remote images (http/https URLs).

use std::time::Duration;

use anyhow::Result;
use image::DynamicImage;

/// Return true when `src` is a remote image (an http or https URL).
pub fn is_remote_image(src: &str) -> bool {
    src.starts_with("https://") || src.starts_with("http://")
}

/// Timeout used for real remote image fetches.
pub const REMOTE_IMAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest remote image body we accept.
const MAX_BODY_BYTES: u64 = 20 * 1024 * 1024;

/// Fetch and decode a remote image, giving up after `timeout`.
///
/// Blocking: call it off the event loop thread.
///
/// # Errors
///
/// Returns an error on network failure, timeout, non-2xx status, a body
/// over 20MB, or bytes that do not decode as an image.
pub fn fetch_remote_image(url: &str, timeout: Duration) -> Result<DynamicImage> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(5)
        .build()
        .into();
    let bytes = agent
        .get(url)
        .call()?
        .body_mut()
        .with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_vec()?;
    Ok(image::load_from_memory(&bytes)?)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    use super::*;

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

    #[test]
    fn test_fetch_remote_image_from_local_server_decodes_png() {
        let url = serve_once(png_bytes());
        let img = fetch_remote_image(&url, Duration::from_secs(5)).unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
    }

    #[test]
    fn test_is_remote_image_matches_http_and_https_only() {
        assert!(is_remote_image("https://example.com/a.png"));
        assert!(is_remote_image("http://example.com/a.png"));
        assert!(!is_remote_image("images/a.png"));
        assert!(!is_remote_image("mermaid://0"));
        assert!(!is_remote_image("file:///tmp/a.png"));
    }
}
