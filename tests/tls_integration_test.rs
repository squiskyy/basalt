//! TLS integration tests for the RESP protocol.
//!
//! These tests verify that the RESP server works correctly with TLS
//! when the `tls-rustls` feature is enabled. They generate a self-signed
//! certificate, start the server with TLS, and verify basic RESP commands
//! work over the encrypted connection.

#[cfg(feature = "tls-rustls")]
mod tls_rustls_tests {
    use std::sync::Arc;

    use basalt::http::auth::AuthStore;
    use basalt::replication::ReplicationState;
    use basalt::resp::server::run_with_replication_and_tls;
    use basalt::resp::tls::TlsAcceptor;
    use basalt::store::{ConsolidationManager, KvEngine};
    use basalt::store::share::ShareStore;
    use rcgen::{CertificateParams, KeyPair, SanType};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::TlsConnector;
    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};

    /// Generate a self-signed certificate and key for testing.
    /// Returns (cert_pem, key_pem).
    fn generate_self_signed_cert() -> (String, String) {
        let mut params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        params.subject_alt_names = vec![SanType::IpAddress("127.0.0.1".parse().unwrap())];

        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();

        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        (cert_pem, key_pem)
    }

    /// A server cert verifier that accepts any certificate (for testing only).
    #[derive(Debug)]
    struct NoVerifier;

    impl ServerCertVerifier for NoVerifier {
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
            ]
        }

        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            Ok(ServerCertVerified::assertion())
        }
    }

    /// Create a TLS connector that accepts any server cert (for testing).
    fn make_tls_connector() -> TlsConnector {
        // Install the ring crypto provider as the default if not already installed.
        // Required for rustls 0.23+ which removed the built-in provider.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    }

    /// Write cert and key to temp files, return (TempDir, cert_path, key_path).
    fn write_cert_key_files(cert_pem: &str, key_pem: &str) -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert_pem).unwrap();
        std::fs::write(&key_path, key_pem).unwrap();
        let cert_str = cert_path.to_string_lossy().to_string();
        let key_str = key_path.to_string_lossy().to_string();
        (dir, cert_str, key_str)
    }

    /// Start a TLS RESP server on a random port. Returns (port, JoinHandle).
    async fn start_tls_resp_server(
        engine: Arc<KvEngine>,
        cert_path: &str,
        key_path: &str,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let acceptor =
            TlsAcceptor::new(cert_path, key_path).expect("failed to create TLS acceptor");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            drop(listener);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let auth = Arc::new(AuthStore::new());
            let share = Arc::new(ShareStore::new());
            let repl_state = Arc::new(ReplicationState::new_primary(engine.clone(), 10000));
            let (_tx, rx) = tokio::sync::watch::channel(false);
            let _ = run_with_replication_and_tls(
                "127.0.0.1",
                port,
                engine,
                auth,
                share,
                None,
                repl_state,
                rx,
                Some(acceptor),
            )
            .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        (port, handle)
    }

    /// Connect to a TLS RESP server and return a TlsStream.
    async fn connect_tls(port: u16) -> tokio_rustls::client::TlsStream<tokio::net::TcpStream> {
        let stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let connector = make_tls_connector();
        let server_name = ServerName::try_from("127.0.0.1").unwrap();
        connector.connect(server_name, stream).await.unwrap()
    }

    /// Read a single RESP response from the TLS stream.
    async fn read_resp_response(
        stream: &mut tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
    ) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            if !buf.is_empty() {
                // Try to find a complete RESP value
                if buf.starts_with(b"+") || buf.starts_with(b"-") {
                    if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                        return String::from_utf8_lossy(&buf[..pos + 2]).to_string();
                    }
                } else if buf.starts_with(b":") {
                    if let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                        return String::from_utf8_lossy(&buf[..pos + 2]).to_string();
                    }
                } else if buf.starts_with(b"$") {
                    // Bulk string
                    if let Some(first_line_end) = buf.windows(2).position(|w| w == b"\r\n") {
                        let len_str = String::from_utf8_lossy(&buf[1..first_line_end]);
                        if let Ok(len) = len_str.parse::<i64>() {
                            if len < 0 {
                                return String::from_utf8_lossy(&buf[..first_line_end + 2])
                                    .to_string();
                            }
                            let expected_end = first_line_end + 2 + len as usize + 2;
                            if buf.len() >= expected_end {
                                return String::from_utf8_lossy(&buf[..expected_end]).to_string();
                            }
                        }
                    }
                }
            }
            let n = stream.read(&mut tmp).await.unwrap();
            if n == 0 {
                panic!("TLS connection closed unexpectedly");
            }
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    fn encode_resp_command(name: &str, args: &[&str]) -> Vec<u8> {
        let mut buf =
            format!("*{}\r\n${}\r\n{}\r\n", args.len() + 1, name.len(), name).into_bytes();
        for arg in args {
            buf.extend_from_slice(format!("${}\r\n{}\r\n", arg.len(), arg).as_bytes());
        }
        buf
    }

    #[tokio::test]
    async fn test_tls_basic_ping() {
        let (cert_pem, key_pem) = generate_self_signed_cert();
        let (_tempdir, cert_path, key_path) = write_cert_key_files(&cert_pem, &key_pem);

        let engine = Arc::new(KvEngine::new(64, Arc::new(ConsolidationManager::disabled())));
        let (port, _handle) = start_tls_resp_server(engine, &cert_path, &key_path).await;

        let mut stream = connect_tls(port).await;

        // PING command
        let cmd = encode_resp_command("PING", &[]);
        stream.write_all(&cmd).await.unwrap();
        stream.flush().await.unwrap();

        let response = read_resp_response(&mut stream).await;
        assert!(
            response.contains("+PONG"),
            "Expected +PONG, got: {response}"
        );
    }

    #[tokio::test]
    async fn test_tls_set_and_get() {
        let (cert_pem, key_pem) = generate_self_signed_cert();
        let (_tempdir, cert_path, key_path) = write_cert_key_files(&cert_pem, &key_pem);

        let engine = Arc::new(KvEngine::new(64, Arc::new(ConsolidationManager::disabled())));
        let (port, _handle) = start_tls_resp_server(engine, &cert_path, &key_path).await;

        let mut stream = connect_tls(port).await;

        // SET command
        let cmd = encode_resp_command("SET", &["ns:key1", "hello_tls"]);
        stream.write_all(&cmd).await.unwrap();
        stream.flush().await.unwrap();

        let response = read_resp_response(&mut stream).await;
        assert!(response.contains("+OK"), "Expected +OK, got: {response}");

        // GET command
        let cmd = encode_resp_command("GET", &["ns:key1"]);
        stream.write_all(&cmd).await.unwrap();
        stream.flush().await.unwrap();

        let response = read_resp_response(&mut stream).await;
        assert!(
            response.contains("hello_tls"),
            "Expected hello_tls, got: {response}"
        );
    }

    #[tokio::test]
    async fn test_tls_multiple_commands() {
        let (cert_pem, key_pem) = generate_self_signed_cert();
        let (_tempdir, cert_path, key_path) = write_cert_key_files(&cert_pem, &key_pem);

        let engine = Arc::new(KvEngine::new(64, Arc::new(ConsolidationManager::disabled())));
        let (port, _handle) = start_tls_resp_server(engine, &cert_path, &key_path).await;

        let mut stream = connect_tls(port).await;

        // Send multiple commands (pipelining)
        let mut pipeline = Vec::new();
        pipeline.extend_from_slice(&encode_resp_command("SET", &["ns:a", "1"]));
        pipeline.extend_from_slice(&encode_resp_command("SET", &["ns:b", "2"]));
        pipeline.extend_from_slice(&encode_resp_command("GET", &["ns:a"]));
        pipeline.extend_from_slice(&encode_resp_command("GET", &["ns:b"]));

        stream.write_all(&pipeline).await.unwrap();
        stream.flush().await.unwrap();

        // Read all responses
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let mut buf = vec![0u8; 4096];
        let n = stream.read(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            response.contains("+OK"),
            "Expected +OK responses, got: {response}"
        );
    }

    #[tokio::test]
    async fn test_tls_acceptor_creation_fails_with_bad_cert() {
        let dir = tempfile::tempdir().unwrap();
        let bad_cert = dir.path().join("bad.pem");
        let bad_key = dir.path().join("bad-key.pem");
        std::fs::write(&bad_cert, "not a real cert").unwrap();
        std::fs::write(&bad_key, "not a real key").unwrap();

        let result = TlsAcceptor::new(&bad_cert.to_string_lossy(), &bad_key.to_string_lossy());
        assert!(result.is_err(), "Expected error with bad cert, got Ok");
    }

    #[tokio::test]
    async fn test_tls_build_acceptor_requires_both_cert_and_key() {
        // Only cert, no key
        let result = basalt::resp::tls::build_acceptor(Some("/path/to/cert"), None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("tls_key is missing"),
            "Expected 'tls_key is missing', got: {err}"
        );

        // Only key, no cert
        let result = basalt::resp::tls::build_acceptor(None, Some("/path/to/key"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("tls_cert is missing"),
            "Expected 'tls_cert is missing', got: {err}"
        );

        // Neither - should return Ok(None) (TLS not configured)
        let result = basalt::resp::tls::build_acceptor(None, None);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_tls_build_acceptor_with_valid_cert() {
        let (cert_pem, key_pem) = generate_self_signed_cert();
        let (_tempdir, cert_path, key_path) = write_cert_key_files(&cert_pem, &key_pem);

        let result = basalt::resp::tls::build_acceptor(Some(&cert_path), Some(&key_path));
        assert!(result.is_ok(), "Expected Ok, got: {:?}", result.err());
        let acceptor = result.unwrap();
        assert!(acceptor.is_some());
        assert!(acceptor.unwrap().is_available());
    }
}
