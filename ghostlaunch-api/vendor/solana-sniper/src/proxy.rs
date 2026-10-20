//! HTTP CONNECT proxy support for gRPC.
//!
//! This sandbox routes all egress through an authenticated HTTP proxy
//! (`HTTPS_PROXY`). tonic does not honor proxy env vars, so without this the
//! gRPC client connects "directly" into the sandbox's intercepting middlebox
//! and fails with `InvalidContentType`.
//!
//! [`ProxyConnector`] performs the `CONNECT host:443` handshake (with
//! `Proxy-Authorization`) and hands the raw tunneled TCP stream back; tonic
//! then applies its normal TLS handshake over the tunnel, so certificates are
//! still validated end-to-end against the real host.

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use http::Uri;
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower_service::Service;
use tracing::info;

#[derive(Clone, Debug)]
struct ProxyInfo {
    host: String,
    port: u16,
    auth: Option<String>, // base64(user:pass)
}

fn parse_proxy(url: &str) -> Result<ProxyInfo> {
    let url = url.trim();
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("HTTP://"))
        .context("proxy URL must start with http://")?;
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };
    let auth = userinfo.map(|u| {
        base64::engine::general_purpose::STANDARD.encode(u.as_bytes())
    });
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().context("bad proxy port")?),
        None => (hostport.to_string(), 8080),
    };
    Ok(ProxyInfo { host, port, auth })
}

/// Proxy URL from the environment, if one is configured.
pub fn proxy_from_env() -> Option<String> {
    std::env::var("HTTPS_PROXY")
        .or_else(|_| std::env::var("https_proxy"))
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// tower service that tunnels to the target through an HTTP CONNECT proxy.
#[derive(Clone)]
pub struct ProxyConnector {
    proxy: ProxyInfo,
}

impl ProxyConnector {
    pub fn from_env() -> Result<Self> {
        let url = proxy_from_env().context("no HTTPS_PROXY/https_proxy in environment")?;
        Ok(Self { proxy: parse_proxy(&url)? })
    }
}

async fn connect_via_proxy(proxy: &ProxyInfo, target_host: &str, target_port: u16) -> Result<TcpStream> {
    let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .with_context(|| format!("proxy TCP connect to {}:{} failed", proxy.host, proxy.port))?;

    let mut req = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\n"
    );
    if let Some(auth) = &proxy.auth {
        req.push_str(&format!("Proxy-Authorization: Basic {auth}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await.context("proxy CONNECT write failed")?;

    // Read response headers until the blank line.
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk).await.context("proxy CONNECT read failed")?;
        if n == 0 {
            return Err(anyhow!("proxy closed connection during CONNECT"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Err(anyhow!("proxy CONNECT response too large"));
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or("");
    if !(status_line.contains(" 200 ") || status_line.contains(" 200")) {
        return Err(anyhow!("proxy CONNECT rejected: {status_line}"));
    }
    info!("proxy CONNECT established to {target_host}:{target_port}");
    Ok(stream)
}

/// Concrete error type for the proxy connector (satisfies tonic's
/// `std::error::Error` bound on custom connectors).
#[derive(Debug)]
pub struct ProxyError(pub String);

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "proxy error: {}", self.0)
    }
}

impl std::error::Error for ProxyError {}

impl From<anyhow::Error> for ProxyError {
    fn from(e: anyhow::Error) -> Self {
        ProxyError(format!("{e:?}"))
    }
}

impl Service<Uri> for ProxyConnector {
    type Response = TokioIo<TcpStream>;
    type Error = ProxyError;
    type Future =
        Pin<Box<dyn Future<Output = Result<TokioIo<TcpStream>, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let proxy = self.proxy.clone();
        Box::pin(async move {
            let host = uri.host().ok_or_else(|| ProxyError("target URI has no host".into()))?;
            let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            });
            let stream = connect_via_proxy(&proxy, host, port)
                .await
                .map_err(ProxyError::from)?;
            Ok(TokioIo::new(stream))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_url() {
        let p = parse_proxy("http://hatch-runtime:secret123@hatch-egress-proxy:3128").unwrap();
        assert_eq!(p.host, "hatch-egress-proxy");
        assert_eq!(p.port, 3128);
        assert!(p.auth.is_some());
    }

    #[test]
    fn rejects_bad_scheme() {
        assert!(parse_proxy("socks5://host:1080").is_err());
    }
}
