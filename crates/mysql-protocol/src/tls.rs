//! TLS for client connections (rustls, ring provider): server configuration from PEM files or
//! an auto-generated self-signed certificate (TiDB `util.LoadTLSCertificates` / `auto-tls`), and
//! the stream types used to upgrade a connection after an SSLRequest.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Common name TiDB uses for auto-generated certificates.
pub const AUTO_CERT_COMMON_NAME: &str = "TiDB_Server_Auto_Generated_Server_Certificate";

/// Validity of auto-generated certificates (TiDB: 90 days).
const AUTO_CERT_VALIDITY_DAYS: i64 = 90;

/// A rustls server configuration shared by all connections.
pub type TlsConfig = Arc<rustls::ServerConfig>;

fn tls_error(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn builder() -> io::Result<rustls::ConfigBuilder<rustls::ServerConfig, rustls::WantsVerifier>> {
    // TLS 1.2 and 1.3 (TiDB's default minimum is TLS 1.2); rustls only offers secure suites.
    rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| tls_error(e.to_string()))
}

fn finish(
    builder: rustls::ConfigBuilder<rustls::ServerConfig, rustls::WantsVerifier>,
    ca: Option<&Path>,
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> io::Result<TlsConfig> {
    let builder = match ca {
        None => builder.with_no_client_auth(),
        Some(ca) => {
            // Like TiDB without require-secure-transport: verify a client certificate if one is
            // presented (tls.VerifyClientCertIfGiven).
            let mut roots = rustls::RootCertStore::empty();
            for cert in CertificateDer::pem_file_iter(ca)
                .map_err(|e| tls_error(format!("{}: {e}", ca.display())))?
            {
                roots
                    .add(cert.map_err(|e| tls_error(format!("{}: {e}", ca.display())))?)
                    .map_err(|e| tls_error(format!("{}: {e}", ca.display())))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                Arc::new(roots),
                Arc::new(rustls::crypto::ring::default_provider()),
            )
            .allow_unauthenticated()
            .build()
            .map_err(|e| tls_error(e.to_string()))?;
            builder.with_client_cert_verifier(verifier)
        }
    };
    let config = builder
        .with_single_cert(certs, key)
        .map_err(|e| tls_error(e.to_string()))?;
    Ok(Arc::new(config))
}

/// Server TLS configuration from PEM files (`--ssl-cert`, `--ssl-key`, optional `--ssl-ca`).
pub fn load_tls_config(cert: &Path, key: &Path, ca: Option<&Path>) -> io::Result<TlsConfig> {
    let certs = CertificateDer::pem_file_iter(cert)
        .map_err(|e| tls_error(format!("{}: {e}", cert.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tls_error(format!("{}: {e}", cert.display())))?;
    if certs.is_empty() {
        return Err(tls_error(format!(
            "{}: no certificate found",
            cert.display()
        )));
    }
    let key = PrivateKeyDer::from_pem_file(key)
        .map_err(|e| tls_error(format!("{}: {e}", key.display())))?;
    finish(builder()?, ca, certs, key)
}

/// A freshly generated self-signed certificate (PEM), as TiDB's `auto-tls` creates at startup:
/// CN `TiDB_Server_Auto_Generated_Server_Certificate`, 90 days validity. TiDB puts its host
/// name in the DNS SAN; callers pass the names to use. The key is ECDSA P-256 rather than
/// TiDB's RSA (ring cannot generate RSA keys).
#[derive(Debug)]
pub struct GeneratedCert {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generates a self-signed certificate with `names` (DNS names or IP addresses) as SANs.
pub fn generate_self_signed(names: &[&str]) -> io::Result<GeneratedCert> {
    let names = names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    let mut params = rcgen::CertificateParams::new(names).map_err(|e| tls_error(e.to_string()))?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, AUTO_CERT_COMMON_NAME);
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(AUTO_CERT_VALIDITY_DAYS);
    let key = rcgen::KeyPair::generate().map_err(|e| tls_error(e.to_string()))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| tls_error(e.to_string()))?;
    Ok(GeneratedCert {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    })
}

/// Server TLS configuration from in-memory PEM data (certificate chain and private key).
pub fn tls_config_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> io::Result<TlsConfig> {
    let certs = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| tls_error(e.to_string()))?;
    if certs.is_empty() {
        return Err(tls_error("no certificate found"));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| tls_error(e.to_string()))?;
    finish(builder()?, None, certs, key)
}

/// Server TLS configuration with an auto-generated self-signed certificate (kept in memory).
pub fn auto_tls_config(names: &[&str]) -> io::Result<TlsConfig> {
    let generated = generate_self_signed(names)?;
    tls_config_from_pem(generated.cert_pem.as_bytes(), generated.key_pem.as_bytes())
}

/// A stream that first yields `prefix` (bytes already read into a buffer before a TLS upgrade,
/// e.g. a ClientHello sent right behind the SSLRequest), then reads from `inner`.
#[derive(Debug)]
pub struct Rewind<S> {
    prefix: Vec<u8>,
    pos: usize,
    inner: S,
}

impl<S> Rewind<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Rewind {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Rewind<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.pos < self.prefix.len() {
            let n = (self.prefix.len() - self.pos).min(buf.remaining());
            let start = self.pos;
            buf.put_slice(&self.prefix[start..start + n]);
            self.pos += n;
            if self.pos == self.prefix.len() {
                self.prefix = Vec::new();
                self.pos = 0;
            }
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Rewind<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A client connection: plain, or TLS after an SSLRequest.
#[derive(Debug)]
pub enum MaybeTls<S> {
    Plain(S),
    Tls(Box<tokio_rustls::server::TlsStream<Rewind<S>>>),
}

impl<S> MaybeTls<S> {
    /// Negotiated cipher suite name (IANA name, e.g. `TLS13_AES_128_GCM_SHA256`), if TLS.
    pub fn cipher(&self) -> Option<String> {
        match self {
            MaybeTls::Plain(_) => None,
            MaybeTls::Tls(t) => t
                .get_ref()
                .1
                .negotiated_cipher_suite()
                .map(|s| format!("{:?}", s.suite())),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for MaybeTls<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MaybeTls::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for MaybeTls<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MaybeTls::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_flush(cx),
            MaybeTls::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MaybeTls::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn generated_cert_loads() {
        let g = generate_self_signed(&["localhost", "127.0.0.1"]).unwrap();
        assert!(g.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(g.key_pem.contains("PRIVATE KEY"));
        auto_tls_config(&["localhost"]).unwrap();

        let dir = std::env::temp_dir().join(format!("tidb-rs-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        std::fs::write(&cert, &g.cert_pem).unwrap();
        std::fs::write(&key, &g.key_pem).unwrap();
        load_tls_config(&cert, &key, None).unwrap();
        // The certificate as CA (self-signed) is accepted for client verification.
        load_tls_config(&cert, &key, Some(&cert)).unwrap();
        assert!(load_tls_config(&key, &key, None).is_err());
        assert!(load_tls_config(&dir.join("missing.pem"), &key, None).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn rewind_yields_prefix_first() {
        let (mut a, b) = tokio::io::duplex(64);
        a.write_all(b"world").await.unwrap();
        drop(a);
        let mut r = Rewind::new(b"hello ".to_vec(), b);
        let mut out = String::new();
        r.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "hello world");
    }
}
