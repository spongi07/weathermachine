//! `weather-machine healthcheck`: a dependency-free HTTP probe for container
//! health checks (the distroless runtime image has no shell or curl).

use anyhow::{Context, Result, bail};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Probe URL from the environment: `WM_HEALTHCHECK_URL`, else derived from
/// `WM_HTTP_BIND` (a wildcard bind address is probed on loopback).
pub fn default_url() -> String {
    if let Ok(u) = std::env::var("WM_HEALTHCHECK_URL")
        && !u.trim().is_empty()
    {
        return u.trim().to_owned();
    }
    let bind = std::env::var("WM_HTTP_BIND")
        .ok()
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| "0.0.0.0:8080".to_owned());
    let port = bind.rsplit(':').next().unwrap_or("8080");
    let host = bind.rsplit_once(':').map_or("127.0.0.1", |(h, _)| h);
    let host = match host {
        "0.0.0.0" | "" | "[::]" | "::" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{port}/healthz")
}

/// Split `http://host:port/path` (plain HTTP only; the probe stays in-container).
pub fn parse_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("http://")
        .context("healthcheck URL must start with http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (
            h.trim_start_matches('[').trim_end_matches(']').to_owned(),
            p.parse::<u16>().context("invalid port")?,
        ),
        _ => (authority.to_owned(), 80),
    };
    if host.is_empty() {
        bail!("healthcheck URL has no host");
    }
    Ok((host, port, path.to_owned()))
}

/// GET `url`; succeed only on HTTP 200 within `timeout`.
pub async fn probe(url: &str, timeout: Duration) -> Result<()> {
    let (host, port, path) = parse_url(url)?;
    let fut = async {
        let mut stream = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("connecting to {host}:{port}"))?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: weather-machine-healthcheck\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await?;
        let mut buf = Vec::with_capacity(512);
        let mut chunk = [0u8; 512];
        // The status line is all we need.
        while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < 4096 {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let line = String::from_utf8_lossy(&buf);
        let status = line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        if status == "200" {
            Ok(())
        } else {
            bail!("unhealthy: HTTP status '{status}'")
        }
    };
    tokio::time::timeout(timeout, fut)
        .await
        .context("healthcheck timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        assert_eq!(
            parse_url("http://127.0.0.1:8080/healthz").unwrap(),
            ("127.0.0.1".into(), 8080, "/healthz".into())
        );
        assert_eq!(
            parse_url("http://localhost/").unwrap(),
            ("localhost".into(), 80, "/".into())
        );
        assert_eq!(
            parse_url("http://[::1]:9000/x").unwrap(),
            ("::1".into(), 9000, "/x".into())
        );
        assert!(parse_url("https://example.org/").is_err());
    }

    #[tokio::test]
    async fn probe_reports_status() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for status in ["200 OK", "503 Service Unavailable"] {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut b = [0u8; 256];
                let _ = s.read(&mut b).await;
                let _ = s
                    .write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").as_bytes())
                    .await;
            }
        });
        let url = format!("http://{addr}/healthz");
        assert!(probe(&url, Duration::from_secs(2)).await.is_ok());
        assert!(probe(&url, Duration::from_secs(2)).await.is_err());
    }
}
