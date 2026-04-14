// TLS support for the RESP protocol.
//
// Provides a unified `TlsAcceptor` that wraps either rustls or native-tls,
// selected at compile time via feature flags:
// - `tls-rustls` (recommended): pure Rust, no C dependencies
// - `tls-native-tls`: platform-native TLS (OpenSSL/SChannel)
//
// When neither feature is enabled, `TlsAcceptor` is a zero-sized type and
// all TLS operations return errors (plaintext only).
//
// Configuration: `tls_cert` and `tls_key` paths in the config file or
// `--tls-cert` / `--tls-key` CLI flags. Both must be set to enable TLS.

// ---------------------------------------------------------------------------
#[cfg(feature = "tls-rustls")]
mod imp {
    use std::fs::File;
    use std::io::BufReader;
    use std::sync::Arc;

    use tokio_rustls::TlsAcceptor as RustlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

    use super::super::error::RespError;

    /// TLS acceptor wrapping rustls.
    #[derive(Clone)]
    pub struct TlsAcceptor {
        inner: RustlsAcceptor,
    }

    impl std::fmt::Debug for TlsAcceptor {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TlsAcceptor")
                .field("backend", &"rustls")
                .finish_non_exhaustive()
        }
    }

    /// A TLS-wrapped or plaintext TCP stream.
    #[allow(clippy::large_enum_variant)]
    pub enum TlsStream {
        Tls(tokio_rustls::server::TlsStream<tokio::net::TcpStream>),
        Plain(tokio::net::TcpStream),
    }

    impl TlsAcceptor {
        /// Create a new rustls TLS acceptor from PEM cert and key files.
        pub fn new(cert_path: &str, key_path: &str) -> Result<Self, String> {
            // Install the ring crypto provider as the default if not already installed.
            // This is required for rustls 0.23+ which removed the built-in provider.
            let _ = rustls::crypto::ring::default_provider().install_default();

            let certs = load_certs(cert_path)?;
            let key = load_key(key_path)?;

            let config = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .map_err(|e| format!("failed to build rustls ServerConfig: {e}"))?;

            Ok(Self {
                inner: RustlsAcceptor::from(Arc::new(config)),
            })
        }

        /// TLS is always available when the feature is compiled in.
        pub fn is_available(&self) -> bool {
            true
        }

        /// Accept a TLS handshake on the given TCP stream.
        pub async fn accept(&self, stream: tokio::net::TcpStream) -> Result<TlsStream, RespError> {
            self.inner
                .accept(stream)
                .await
                .map(TlsStream::Tls)
                .map_err(|e| RespError::Tls(format!("TLS handshake failed: {e}")))
        }
    }

    fn load_certs(path: &str) -> Result<Vec<CertificateDer<'static>>, String> {
        let file =
            File::open(path).map_err(|e| format!("failed to open TLS cert file '{path}': {e}"))?;
        let mut reader = BufReader::new(file);

        rustls_pemfile::certs(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to parse TLS cert file '{path}': {e}"))
    }

    fn load_key(path: &str) -> Result<PrivateKeyDer<'static>, String> {
        // Try PKCS8 first
        let file =
            File::open(path).map_err(|e| format!("failed to open TLS key file '{path}': {e}"))?;
        let mut reader = BufReader::new(file);

        let keys: Vec<_> = rustls_pemfile::pkcs8_private_keys(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to parse PKCS8 key file '{path}': {e}"))?;

        if let Some(key) = keys.into_iter().next() {
            return Ok(PrivateKeyDer::from(key));
        }

        // Try RSA keys
        let file =
            File::open(path).map_err(|e| format!("failed to reopen TLS key file '{path}': {e}"))?;
        let mut reader = BufReader::new(file);

        let keys: Vec<_> = rustls_pemfile::rsa_private_keys(&mut reader)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to parse RSA key file '{path}': {e}"))?;

        if let Some(key) = keys.into_iter().next() {
            return Ok(PrivateKeyDer::from(key));
        }

        Err(format!(
            "no private key found in '{path}' (tried PKCS8 and RSA formats)"
        ))
    }

    // Implement AsyncRead + AsyncWrite for TlsStream
    impl tokio::io::AsyncRead for TlsStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                TlsStream::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
                TlsStream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            }
        }
    }

    impl tokio::io::AsyncWrite for TlsStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            match self.get_mut() {
                TlsStream::Tls(s) => std::pin::Pin::new(s).poll_write(cx, buf),
                TlsStream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                TlsStream::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
                TlsStream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            }
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                TlsStream::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
                TlsStream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// native-tls implementation
// ---------------------------------------------------------------------------
#[cfg(feature = "tls-native-tls")]
mod imp {
    use std::fs;

    use native_tls::Identity;
    use tokio_native_tls::TlsAcceptor as NativeTlsAcceptor;

    use super::super::error::RespError;

    /// TLS acceptor wrapping native-tls.
    #[derive(Clone)]
    pub struct TlsAcceptor {
        inner: NativeTlsAcceptor,
    }

    impl std::fmt::Debug for TlsAcceptor {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TlsAcceptor")
                .field("backend", &"native-tls")
                .finish_non_exhaustive()
        }
    }

    /// A TLS-wrapped or plaintext TCP stream.
    #[allow(clippy::large_enum_variant)]
    pub enum TlsStream {
        Tls(tokio_native_tls::TlsStream<tokio::net::TcpStream>),
        Plain(tokio::net::TcpStream),
    }

    impl TlsAcceptor {
        /// Create a new native-tls TLS acceptor from PEM cert and key files.
        pub fn new(cert_path: &str, key_path: &str) -> Result<Self, String> {
            let cert_data = fs::read(cert_path)
                .map_err(|e| format!("failed to read TLS cert file '{cert_path}': {e}"))?;
            let key_data = fs::read(key_path)
                .map_err(|e| format!("failed to read TLS key file '{key_path}': {e}"))?;

            // native-tls Identity::from_pkcs8 takes DER-encoded cert and key
            let identity = Identity::from_pkcs8(&cert_data, &key_data)
                .map_err(|e| format!("failed to create TLS identity from cert/key: {e}"))?;

            let acceptor = native_tls::TlsAcceptor::builder(identity)
                .build()
                .map_err(|e| format!("failed to build native-tls acceptor: {e}"))?;

            Ok(Self {
                inner: NativeTlsAcceptor::from(acceptor),
            })
        }

        /// TLS is always available when the feature is compiled in.
        pub fn is_available(&self) -> bool {
            true
        }

        /// Accept a TLS handshake on the given TCP stream.
        pub async fn accept(&self, stream: tokio::net::TcpStream) -> Result<TlsStream, RespError> {
            self.inner
                .accept(stream)
                .await
                .map(TlsStream::Tls)
                .map_err(|e| RespError::Tls(format!("TLS handshake failed: {e}")))
        }
    }

    // Implement AsyncRead + AsyncWrite for TlsStream
    impl tokio::io::AsyncRead for TlsStream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                TlsStream::Tls(s) => std::pin::Pin::new(s).poll_read(cx, buf),
                TlsStream::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            }
        }
    }

    impl tokio::io::AsyncWrite for TlsStream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            match self.get_mut() {
                TlsStream::Tls(s) => std::pin::Pin::new(s).poll_write(cx, buf),
                TlsStream::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                TlsStream::Tls(s) => std::pin::Pin::new(s).poll_flush(cx),
                TlsStream::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            }
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            match self.get_mut() {
                TlsStream::Tls(s) => std::pin::Pin::new(s).poll_shutdown(cx),
                TlsStream::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// No-TLS fallback (when neither feature is enabled)
// ---------------------------------------------------------------------------
#[cfg(not(any(feature = "tls-rustls", feature = "tls-native-tls")))]
mod imp {
    use super::super::error::RespError;

    /// No-op TLS acceptor (TLS not compiled in).
    #[derive(Clone, Debug)]
    pub struct TlsAcceptor;

    /// Placeholder stream type - never instantiated without TLS features.
    pub enum TlsStream {}

    impl TlsAcceptor {
        /// Always returns an error when TLS is not compiled in.
        pub fn new(_cert_path: &str, _key_path: &str) -> Result<Self, String> {
            Err(
                "TLS support not compiled in. Enable feature 'tls-rustls' or 'tls-native-tls'."
                    .to_string(),
            )
        }

        /// TLS is never available without a feature flag.
        pub fn is_available(&self) -> bool {
            false
        }
    }
}

// Re-export from the implementation module
pub use imp::{TlsAcceptor, TlsStream};

/// Build a TlsAcceptor from config paths, if both cert and key are provided.
/// Returns None if TLS is not configured (no cert/key paths).
/// Returns an error if TLS is configured but the acceptor cannot be built.
#[cfg(any(feature = "tls-rustls", feature = "tls-native-tls"))]
pub fn build_acceptor(
    tls_cert: Option<&str>,
    tls_key: Option<&str>,
) -> Result<Option<TlsAcceptor>, String> {
    match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => {
            let acceptor = TlsAcceptor::new(cert, key)?;
            tracing::info!("TLS enabled for RESP protocol");
            Ok(Some(acceptor))
        }
        (Some(_), None) => Err("tls_cert is set but tls_key is missing".to_string()),
        (None, Some(_)) => Err("tls_key is set but tls_cert is missing".to_string()),
        (None, None) => Ok(None),
    }
}

/// Build a TlsAcceptor from config paths (no-TLS stub).
#[cfg(not(any(feature = "tls-rustls", feature = "tls-native-tls")))]
pub fn build_acceptor(
    tls_cert: Option<&str>,
    tls_key: Option<&str>,
) -> Result<Option<TlsAcceptor>, String> {
    match (tls_cert, tls_key) {
        (Some(_), Some(_)) => {
            Err("TLS cert/key configured but TLS support not compiled in. Enable feature 'tls-rustls' or 'tls-native-tls'.".to_string())
        }
        (Some(_), None) => Err("tls_cert is set but tls_key is missing".to_string()),
        (None, Some(_)) => Err("tls_key is set but tls_cert is missing".to_string()),
        (None, None) => Ok(None),
    }
}
