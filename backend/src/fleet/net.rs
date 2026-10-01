//! Outbound connections: TLS (rustls + ring + webpki roots, no OpenSSL) and
//! a tiny HTTP POST for alert notifications.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;

fn connector() -> anyhow::Result<tokio_rustls::TlsConnector> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    if let Some(c) = CONFIG.get() {
        return Ok(tokio_rustls::TlsConnector::from(c.clone()));
    }
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let config = CONFIG.get_or_init(|| Arc::new(config)).clone();
    Ok(tokio_rustls::TlsConnector::from(config))
}

pub async fn tls(host: &str, tcp: TcpStream) -> anyhow::Result<TlsStream<TcpStream>> {
    let name = rustls::pki_types::ServerName::try_from(host.to_string())?;
    Ok(connector()?.connect(name, tcp).await?)
}

/// POST `body` to an http(s) URL; returns the status code.
pub async fn post(url: &str, headers: &[(&str, &str)], body: &[u8]) -> anyhow::Result<u16> {
    let (tls_on, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        anyhow::bail!("unsupported URL {url}");
    };
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse()?),
        None => (hostport, if tls_on { 443 } else { 80 }),
    };
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: {hostport}\r\nUser-Agent: serious-server\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let exchange = async {
        let tcp = TcpStream::connect((host, port)).await?;
        let mut out = Vec::new();
        if tls_on {
            let mut s = tls(host, tcp).await?;
            s.write_all(head.as_bytes()).await?;
            s.write_all(body).await?;
            let _ = s.read_to_end(&mut out).await;
        } else {
            let mut s = tcp;
            s.write_all(head.as_bytes()).await?;
            s.write_all(body).await?;
            s.read_to_end(&mut out).await?;
        }
        anyhow::Ok(out)
    };
    let out = tokio::time::timeout(Duration::from_secs(20), exchange)
        .await
        .map_err(|_| anyhow::anyhow!("timed out"))??;
    let status = String::from_utf8_lossy(&out)
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("no HTTP status in reply"))?;
    Ok(status)
}
