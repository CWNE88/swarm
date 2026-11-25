//! The bandwidth and receive limits set from the command line (`swarm
//! bandwidth-limit`, `swarm receive-limit`) while this runs: the settings
//! file is looked at every ten seconds, and a change taken up.

use super::Ctx;
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn watch(ctx: Arc<Ctx>) {
    let mut every = tokio::time::interval(Duration::from_secs(10));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        if ctx.stopped.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let receive = crate::settings::receive_limit(&ctx.paths.settings());
        if receive != *ctx.receive_limit.borrow() {
            ctx.receive_limit.send_replace(receive);
            log!(ctx, "receive limit: {}", receive.map_or("none".to_string(), |l| format!("{}bit/s", super::format_bitrate(l))));
        }
        let set = crate::settings::bandwidth_limit(&ctx.paths.settings());
        if set != crate::transport::bandwidth_limit() {
            crate::transport::set_bandwidth_limit(set);
            log!(ctx, "bandwidth limit: {}", set.map_or("none".to_string(), |l| format!("{}bit/s", super::format_bitrate(l))));
        }
    }
}
