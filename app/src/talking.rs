//! Who's talking in a call, and who's the speaker: the one a speaker
//! view shows large. Each member's level is taken from their voice as
//! it's decoded; the speaker changes at most once every
//! `SWITCH_AT_MOST_EVERY`, and only to someone talking once the speaker
//! has stopped, or who's clearly louder — not at every cough or
//! word said over them.

use std::collections::HashMap;
use std::time::{Duration, Instant};
use swarm_core::call::SAMPLE_RATE;

use crate::meter::Meter;

/// A voice this loud (dBFS, a frame's own level) counts as talking ...
const TALKING_DB: f32 = -40.0;
/// ... and for this long after, through the gaps between words.
const TALKING_HOLDS: Duration = Duration::from_millis(300);
/// How quickly a level, for telling who's louder, follows the voice.
const SMOOTHING: Duration = Duration::from_millis(150);
/// The speaker shown changes no more often than this.
pub const SWITCH_AT_MOST_EVERY: Duration = Duration::from_millis(500);
/// While the speaker's still talking, someone else takes over only this
/// much louder (dB).
const LOUDER_BY_DB: f32 = 6.0;

#[derive(Default)]
pub struct Talking {
    levels: HashMap<[u8; 32], Level>,
    speaker: Option<[u8; 32]>,
    switched: Option<Instant>,
}

struct Level {
    /// Smoothed power (mean square).
    power: f32,
    loud_at: Option<Instant>,
    /// As a meter shows it.
    meter: Meter,
}

impl Level {
    fn db(&self) -> f32 {
        10.0 * self.power.max(1e-12).log10()
    }

    fn talking(&self, now: Instant) -> bool {
        self.loud_at.is_some_and(|at| now.saturating_duration_since(at) <= TALKING_HOLDS)
    }
}

impl Talking {
    /// `from`'s voice, just decoded.
    pub fn hear(&mut self, from: [u8; 32], pcm: &[f32], now: Instant) {
        if pcm.is_empty() {
            return;
        }
        let power = pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32;
        let lasts = Duration::from_secs_f64(pcm.len() as f64 / SAMPLE_RATE as f64);
        let follow = 1.0 - (-lasts.as_secs_f32() / SMOOTHING.as_secs_f32()).exp();
        let level = self.levels.entry(from).or_insert(Level { power, loud_at: None, meter: Meter::default() });
        level.power += (power - level.power) * follow;
        level.meter.hear(pcm, now);
        if 10.0 * power.max(1e-12).log10() >= TALKING_DB {
            level.loud_at = Some(now);
        }
        self.choose(now);
    }

    /// Whether `who` is talking.
    pub fn is_talking(&self, who: &[u8; 32], now: Instant) -> bool {
        self.levels.get(who).is_some_and(|l| l.talking(now))
    }

    /// `who`'s level as a meter shows it, 0 to 1 (see `Meter::shown`).
    pub fn level(&self, who: &[u8; 32], now: Instant) -> f32 {
        self.levels.get(who).map_or(0.0, |l| l.meter.shown(now))
    }

    /// Who's shown large: whoever last took over, kept while nobody
    /// else talks.
    pub fn speaker(&self) -> Option<[u8; 32]> {
        self.speaker
    }

    /// Only these are in the call now: anyone else is forgotten, and
    /// stops being the speaker.
    pub fn keep_only(&mut self, members: &[[u8; 32]]) {
        self.levels.retain(|key, _| members.contains(key));
        if self.speaker.is_some_and(|s| !members.contains(&s)) {
            self.speaker = None;
        }
    }

    fn choose(&mut self, now: Instant) {
        if self.switched.is_some_and(|at| now.saturating_duration_since(at) < SWITCH_AT_MOST_EVERY) {
            return;
        }
        let Some((&loudest, level)) = self.levels.iter().filter(|(_, l)| l.talking(now)).max_by(|a, b| a.1.power.total_cmp(&b.1.power)) else { return };
        let takes_over = match self.speaker.and_then(|s| self.levels.get(&s).map(|l| (s, l))) {
            None => true,
            Some((speaker, _)) if speaker == loudest => false,
            Some((_, current)) => !current.talking(now) || level.db() >= current.db() + LOUDER_BY_DB,
        };
        if takes_over {
            self.speaker = Some(loudest);
            self.switched = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use swarm_core::call::FRAME_SAMPLES;

    const BOB: [u8; 32] = [2; 32];
    const CAROL: [u8; 32] = [3; 32];

    /// A 20 ms frame at this level (dBFS, as a sine's RMS).
    fn voice(db: f32) -> Vec<f32> {
        let amplitude = 10f32.powf(db / 20.0) * std::f32::consts::SQRT_2;
        (0..FRAME_SAMPLES).map(|i| (i as f32 * 0.07).sin() * amplitude).collect()
    }

    /// Each speaking at their level (None: silent) for `ms`, a frame
    /// every 20 ms from `*now`, which moves on.
    fn speak(talking: &mut Talking, now: &mut Instant, ms: u64, bob: Option<f32>, carol: Option<f32>) {
        for _ in 0..ms / 20 {
            talking.hear(BOB, &voice(bob.unwrap_or(-90.0)), *now);
            talking.hear(CAROL, &voice(carol.unwrap_or(-90.0)), *now);
            *now += Duration::from_millis(20);
        }
    }

    #[test]
    fn the_first_to_talk_is_the_speaker_and_stays_it_through_silence() {
        let mut talking = Talking::default();
        let mut now = Instant::now();
        speak(&mut talking, &mut now, 400, None, None);
        assert_eq!(talking.speaker(), None);
        speak(&mut talking, &mut now, 400, Some(-20.0), None);
        assert_eq!(talking.speaker(), Some(BOB));
        assert!(talking.is_talking(&BOB, now) && !talking.is_talking(&CAROL, now));
        speak(&mut talking, &mut now, 2000, None, None);
        assert_eq!(talking.speaker(), Some(BOB));
        assert!(!talking.is_talking(&BOB, now));
    }

    #[test]
    fn the_speaker_changes_when_the_other_starts_once_they_stop() {
        let mut talking = Talking::default();
        let mut now = Instant::now();
        speak(&mut talking, &mut now, 1000, Some(-20.0), None);
        speak(&mut talking, &mut now, 1000, None, Some(-25.0));
        assert_eq!(talking.speaker(), Some(CAROL));
    }

    #[test]
    fn a_word_said_over_the_speaker_no_louder_doesnt_take_over_but_a_much_louder_one_does() {
        let mut talking = Talking::default();
        let mut now = Instant::now();
        speak(&mut talking, &mut now, 1000, Some(-20.0), None);
        speak(&mut talking, &mut now, 1000, Some(-20.0), Some(-18.0));
        assert_eq!(talking.speaker(), Some(BOB));
        speak(&mut talking, &mut now, 1000, Some(-30.0), Some(-15.0));
        assert_eq!(talking.speaker(), Some(CAROL));
    }

    #[test]
    fn talking_over_each_other_the_speaker_changes_at_most_every_half_second() {
        let mut talking = Talking::default();
        let mut now = Instant::now();
        let mut switches: Vec<Instant> = Vec::new();
        let mut last = None;
        for turn in 0..14 {
            let (bob, carol) = if turn % 2 == 0 { (-10.0, -50.0) } else { (-50.0, -10.0) };
            for _ in 0..15 {
                speak(&mut talking, &mut now, 20, Some(bob), Some(carol));
                if talking.speaker() != last {
                    last = talking.speaker();
                    switches.push(now);
                }
            }
        }
        assert!(switches.len() > 3, "switched {} times", switches.len());
        for pair in switches.windows(2) {
            assert!(pair[1] - pair[0] >= Duration::from_millis(500), "switched {:?} after the last", pair[1] - pair[0]);
        }
    }

    #[test]
    fn each_has_their_own_level_and_one_never_heard_none() {
        let mut talking = Talking::default();
        let mut now = Instant::now();
        speak(&mut talking, &mut now, 200, Some(-10.0), Some(-40.0));
        let (bob, carol) = (talking.level(&BOB, now), talking.level(&CAROL, now));
        assert!(bob > carol && carol > 0.0, "bob {bob}, carol {carol}");
        assert_eq!(talking.level(&[9; 32], now), 0.0);
    }

    #[test]
    fn talking_stops_soon_after_the_voice_does() {
        let mut talking = Talking::default();
        let mut now = Instant::now();
        speak(&mut talking, &mut now, 1000, Some(-10.0), None);
        speak(&mut talking, &mut now, 400, None, None);
        assert!(!talking.is_talking(&BOB, now));
    }

    #[test]
    fn someone_who_leaves_stops_being_the_speaker() {
        let mut talking = Talking::default();
        let mut now = Instant::now();
        speak(&mut talking, &mut now, 1000, Some(-20.0), None);
        talking.keep_only(&[CAROL]);
        assert_eq!(talking.speaker(), None);
        assert!(!talking.is_talking(&BOB, now));
    }
}
