//! Exercise WSS in a separate test process: plain WS mocks never initialize TLS.
use clap::Parser;
use note_taker::{
    cli::{Cli, Command},
    stt::elevenlabs::Connection,
};
use std::time::Duration;
use tokio::{io::AsyncReadExt, net::TcpListener, time::timeout};

#[tokio::test]
async fn wss_initializes_crypto_and_sends_a_tls_client_hello_without_panicking() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut record_header = [0; 5];
        timeout(
            Duration::from_secs(5),
            stream.read_exact(&mut record_header),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(record_header[0], 22, "expected a TLS handshake record");
        assert_eq!(record_header[1], 3, "expected a TLS protocol version");
        // No trusted server certificate is supplied. Closing must become a normal
        // connection error, rather than a provider-selection panic or plaintext WS.
    });
    let Command::Listen(args) = Cli::try_parse_from(["stt", "listen"]).unwrap().command else {
        unreachable!();
    };
    let result = timeout(
        Duration::from_secs(10),
        Connection::connect(&format!("wss://{address}"), "test-key", &args),
    )
    .await
    .unwrap();
    let failure = match result {
        Ok(_) => panic!("a server without TLS credentials must not connect"),
        Err(error) => error,
    };
    assert!(failure.retryable);
    assert!(!failure.message.contains("test-key"));
    assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    server.await.unwrap();
}
