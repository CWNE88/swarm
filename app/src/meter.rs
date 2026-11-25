//! A voice's level as a meter shows it: up at once to a frame's level,
//! falling back slowly after, so a word's seen and a pause isn't a
//! flicker. Ours from the microphone (what's sent), each other member's
//! from their voice as it's decoded (see `crate::talking`).

use std::time::Instant;

/// The meter's bottom (dBFS): quieter shows nothing.
const FLOOR_DB: f32 = -60.0;
/// How fast it falls once the voice is quieter (dB a second).
const FALLS_DB_PER_SECOND: f32 = 30.0;

#[derive(Clone, Copy, Debug)]
pub struct Meter {
    /// The level last reached, and when.
    db: f32,
    at: Option<Instant>,
}

impl Default for Meter {
    fn default() -> Meter {
        Meter { db: FLOOR_DB, at: None }
    }
}

impl Meter {
    /// A frame of voice, just heard.
    pub fn hear(&mut self, pcm: &[f32], now: Instant) {
        if pcm.is_empty() {
            return;
        }
        let power = pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32;
        let db = 10.0 * power.max(1e-12).log10();
        let shown = self.db_at(now);
        self.db = db.max(shown);
        self.at = Some(now);
    }

    /// Where it stands at `now`, from 0 (silent) to 1 (as loud as can be).
    pub fn shown(&self, now: Instant) -> f32 {
        ((self.db_at(now) - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
    }

    fn db_at(&self, now: Instant) -> f32 {
        let Some(at) = self.at else { return FLOOR_DB };
        (self.db - now.saturating_duration_since(at).as_secs_f32() * FALLS_DB_PER_SECOND).max(FLOOR_DB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use swarm_core::call::FRAME_SAMPLES;

    /// A 20 ms frame at this level (dBFS, as a sine's RMS).
    fn voice(db: f32) -> Vec<f32> {
        let amplitude = 10f32.powf(db / 20.0) * std::f32::consts::SQRT_2;
        (0..FRAME_SAMPLES).map(|i| (i as f32 * 0.07).sin() * amplitude).collect()
    }

    #[test]
    fn a_meter_goes_up_at_once_with_the_voice_and_higher_the_louder() {
        let now = Instant::now();
        let mut meter = Meter::default();
        assert_eq!(meter.shown(now), 0.0);
        meter.hear(&voice(-30.0), now);
        let quiet = meter.shown(now);
        assert!((quiet - 0.5).abs() < 0.02, "{quiet}");
        meter.hear(&voice(-6.0), now);
        let loud = meter.shown(now);
        assert!((loud - 0.9).abs() < 0.02, "{loud}");
    }

    #[test]
    fn a_meter_falls_slowly_once_the_voice_stops_and_is_down_in_two_seconds() {
        let mut now = Instant::now();
        let mut meter = Meter::default();
        meter.hear(&voice(-10.0), now);
        let was = meter.shown(now);
        for _ in 0..10 {
            now += Duration::from_millis(20);
            meter.hear(&voice(-90.0), now);
        }
        let after = meter.shown(now);
        assert!(after < was && after > was - 0.15, "{was} then {after} a fifth of a second on");
        now += Duration::from_millis(1800);
        assert_eq!(meter.shown(now), 0.0);
    }

    #[test]
    fn a_meter_left_alone_falls_as_if_it_heard_silence() {
        let now = Instant::now();
        let mut meter = Meter::default();
        meter.hear(&voice(-10.0), now);
        assert_eq!(meter.shown(now + Duration::from_secs(2)), 0.0);
    }
}
