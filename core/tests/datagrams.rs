//! Cover slots are QUIC datagrams (see `protocol::emit_cover_slot`). When
//! they're made faster than the link carries them — 4 Mbit/s of cover over
//! a lossy link, where QUIC's congestion control sent less — QUIC's send
//! buffer fills, and older datagrams are dropped to make room. That must
//! never panic inside the connection, which would poison its lock and
//! take the whole process down.

use std::sync::Arc;

#[tokio::test]
async fn datagrams_queued_faster_than_they_go_are_dropped_not_a_crash() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec!["swarm".into()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
    let mut transport = quinn::TransportConfig::default();
    transport.datagram_send_buffer_size(4000);
    let transport = Arc::new(transport);
    let mut server_config = quinn::ServerConfig::with_single_cert(vec![cert.cert.der().clone()], key).unwrap();
    server_config.transport_config(transport.clone());
    let server = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = server.local_addr().unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let mut client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
    client_config.transport_config(transport);
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(client_config);
    let accept = tokio::spawn(async move { server.accept().await.unwrap().await.unwrap() });
    let conn = client.connect(addr, "swarm").unwrap().await.unwrap();
    let theirs = accept.await.unwrap();

    for i in 0..20u8 {
        conn.send_datagram(vec![i; 1000].into()).unwrap();
    }
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), theirs.read_datagram()).await.unwrap().unwrap();
    assert_eq!(got.len(), 1000);
}
