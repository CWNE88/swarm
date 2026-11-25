//! The bandwidth limit (`transport::set_bandwidth_limit`): nothing goes
//! faster than it, and with it taken away, things go as fast as they can.
//! A file of its own: the limit is the whole process's.
use std::time::{Duration, Instant};
use swarm_core::identity::Identity;
use swarm_core::transport::{self, Keys};

/// How long sending `bytes` on a fresh connection over loopback takes,
/// to the other side having them all.
async fn time_to_send(bytes: usize) -> Duration {
    let (a, b) = (Identity::generate(), std::sync::Arc::new(Identity::generate()));
    let b_key = b.public_key_bytes();
    let b_ep = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let addr = b_ep.local_addr().unwrap();
    let got = tokio::spawn(async move {
        let mut conn = transport::accept(&b_ep, Keys::new_device(&b)).await.unwrap();
        let mut buf = vec![0u8; 65536];
        let mut total = 0;
        while total < bytes {
            match conn.recv.read(&mut buf).await.unwrap() {
                Some(n) => total += n,
                None => break,
            }
        }
        Instant::now()
    });
    let a_ep = transport::bind("[::1]:0".parse().unwrap()).unwrap();
    let mut conn = transport::dial(&a_ep, addr, Keys::new_device(&a), b_key).await.unwrap();
    let start = Instant::now();
    conn.send.write_all(&vec![7u8; bytes]).await.unwrap();
    let done = tokio::time::timeout(Duration::from_secs(60), got).await.expect("arrived").unwrap();
    done - start
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_goes_faster_than_the_bandwidth_limit_and_without_it_all_goes_at_once() {
    transport::set_bandwidth_limit(Some(2_000_000));
    let limited = time_to_send(500_000).await;
    assert!(limited >= Duration::from_millis(1_900), "500 kB in {limited:?} is over 2 Mbit/s");
    assert!(limited < Duration::from_secs(4), "500 kB took {limited:?}: well under the limit");
    transport::set_bandwidth_limit(None);
    let free = time_to_send(500_000).await;
    assert!(free < Duration::from_millis(1_000), "500 kB took {free:?} with no limit");
}
