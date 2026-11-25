//! Verifies the mechanism `swarm listen`'s background heartbeat loop
//! relies on: one dial, then multiple pings reused over the same
//! connection (not a fresh handshake per ping). Runs on two throwaway
//! ports bound to `::1` — doesn't touch `transport::DEFAULT_PORT`, so it
//! can run alongside a real `swarm listen` on 1977 without conflicting.

use swarm_core::identity::Identity;
use swarm_core::{protocol, transport};

#[tokio::test]
async fn repeated_pings_succeed_over_one_persistent_connection() {
    let listener_identity = Identity::generate();
    let dialer_identity = Identity::generate();
    let listener_pubkey = listener_identity.public_key_bytes();

    let listener_endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let listener_addr = listener_endpoint.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok(mut conn) = transport::accept(&listener_endpoint, swarm_core::transport::Keys::new_device(&listener_identity)).await else {
                return;
            };
            tokio::spawn(async move {
                let _ = protocol::serve_connection(&mut conn, |_key, _name| false, |_since| swarm_core::posts::Fetched::default(), || None, |_, _| {}, |_| {}).await;
            });
        }
    });

    let dialer_endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let mut conn = transport::dial(&dialer_endpoint, listener_addr, swarm_core::transport::Keys::new_device(&dialer_identity), listener_pubkey)
        .await
        .expect("dial + Noise handshake should succeed");
    assert_eq!(conn.remote_public_key, swarm_core::identity::ed25519_public_key_to_x25519(&listener_pubkey).unwrap());

    for _ in 0..3 {
        protocol::ping(&mut conn).await.expect("ping should succeed on the persistent connection");
    }
}

#[tokio::test]
async fn ping_fails_once_the_listener_is_gone() {
    let listener_identity = Identity::generate();
    let dialer_identity = Identity::generate();
    let listener_pubkey = listener_identity.public_key_bytes();

    let listener_endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let listener_addr = listener_endpoint.local_addr().unwrap();

    let accept_task = tokio::spawn(async move {
        let mut conn = transport::accept(&listener_endpoint, swarm_core::transport::Keys::new_device(&listener_identity)).await.unwrap();
        let _ = protocol::serve_connection(&mut conn, |_key, _name| false, |_since| swarm_core::posts::Fetched::default(), || None, |_, _| {}, |_| {}).await;
    });

    let dialer_endpoint = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let mut conn = transport::dial(&dialer_endpoint, listener_addr, swarm_core::transport::Keys::new_device(&dialer_identity), listener_pubkey)
        .await
        .unwrap();

    protocol::ping(&mut conn).await.expect("first ping should succeed");
    conn.connection.close(0u32.into(), b"simulated peer gone");
    let _ = accept_task.await;

    assert!(protocol::ping(&mut conn).await.is_err(), "ping over a closed connection should fail");
}
