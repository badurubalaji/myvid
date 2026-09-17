//! Optional sound processing, applied in the decoder just before the sink.
//!
//! Everything here is off by default, and when it is off the samples are not
//! touched at all — not even multiplied by one. Each stage works sample-accurate
//! with no lookahead, so none of it adds latency or disturbs A/V sync.
//!
//! The chain, in order:
//! 1. **Clear dialogue** — lift the centre channel of a surround track, where
//!    films put the voices, and lower the rest. Whatever downmixes it later
//!    (PipeWire for headphones, or nothing for real speakers) keeps the balance.
//! 2. **Night mode** — a gentle compressor that brings quiet scenes up and loud
//!    ones down, so the volume can stay put.
//! 3. **Boost** — the part of the volume above 100%.
//! 4. **Limiter** — catches whatever the stages above push past full scale, so
//!    none of them can clip.

use super::AudioEffects;

/// `FRONT_CENTER` in GStreamer's channel positions.
const FRONT_CENTER_BIT: u32 = 2;

/// +3 dB on voices and -3 dB on everything else: a 6 dB shift in favour of
/// dialogue that leaves overall loudness about where it was.
const CENTRE_GAIN: f32 = std::f32::consts::SQRT_2;
const OTHER_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

const NIGHT_THRESHOLD_DB: f32 = -30.0;
const NIGHT_RATIO: f32 = 3.0;
const NIGHT_KNEE_DB: f32 = 6.0;
const NIGHT_MAKEUP_DB: f32 = 9.0;
const NIGHT_ATTACK_S: f32 = 0.005;
const NIGHT_RELEASE_S: f32 = 0.250;

/// Just under full scale, leaving room for the sink's own conversion.
const LIMIT_CEILING: f32 = 0.977; // -0.2 dBFS
const LIMIT_RELEASE_S: f32 = 0.100;

pub struct AudioFx {
    effects: AudioEffects,
    boost: f32,
    channels: usize,
    rate: u32,
    /// Index of the centre channel within a frame, when the track has one and
    /// has more than two channels.
    centre: Option<usize>,
    /// Night-mode gain currently applied, in dB (makeup excluded).
    compress_db: f32,
    /// Limiter gain currently applied, linear.
    limit: f32,
}

impl Default for AudioFx {
    fn default() -> Self {
        Self {
            effects: AudioEffects::default(),
            boost: 1.0,
            channels: 2,
            rate: 48_000,
            centre: None,
            compress_db: 0.0,
            limit: 1.0,
        }
    }
}

impl AudioFx {
    pub fn set_effects(&mut self, effects: AudioEffects) {
        self.effects = effects;
    }

    /// Linear gain for the part of the volume above 100%; never below one.
    pub fn set_boost(&mut self, boost: f32) {
        self.boost = boost.max(1.0);
    }

    /// Describe the stream that is about to arrive. `channel_mask` is the
    /// GStreamer positions bitmask, or zero when the track has none.
    pub fn set_format(&mut self, channels: usize, rate: u32, channel_mask: u64) {
        self.channels = channels.max(1);
        self.rate = rate.max(1);
        self.centre = centre_index(channels, channel_mask);
        self.reset();
    }

    fn reset(&mut self) {
        self.compress_db = 0.0;
        self.limit = 1.0;
    }

    pub fn is_active(&self) -> bool {
        self.effects.dialogue || self.effects.night || self.boost > 1.0
    }

    /// Process interleaved float samples in place.
    pub fn process(&mut self, samples: &mut [f32]) {
        if !self.is_active() {
            // Let the next activation start from rest, not from a stale state.
            self.reset();
            return;
        }

        let channels = self.channels;
        let rate = self.rate as f32;
        let centre = self.centre.filter(|_| self.effects.dialogue);
        let night = self.effects.night;
        let boost = self.boost;

        let attack = smoothing(NIGHT_ATTACK_S, rate);
        let release = smoothing(NIGHT_RELEASE_S, rate);
        let limit_release = smoothing(LIMIT_RELEASE_S, rate);
        let makeup = db_to_gain(NIGHT_MAKEUP_DB);

        for frame in samples.chunks_exact_mut(channels) {
            if let Some(centre) = centre {
                for (index, sample) in frame.iter_mut().enumerate() {
                    *sample *= if index == centre { CENTRE_GAIN } else { OTHER_GAIN };
                }
            }

            let mut gain = boost;

            if night {
                // Linked across channels, so the stereo image does not wander.
                let target = compression_db(gain_to_db(peak(frame)));
                let coefficient = if target < self.compress_db { attack } else { release };
                self.compress_db = target + coefficient * (self.compress_db - target);
                gain *= db_to_gain(self.compress_db) * makeup;
            }

            // Instant attack, so nothing ever crosses the ceiling; a slow release,
            // so the gain does not flutter between transients.
            let loudest = peak(frame) * gain;
            let wanted = if loudest > LIMIT_CEILING { LIMIT_CEILING / loudest } else { 1.0 };
            self.limit = if wanted < self.limit {
                wanted
            } else {
                wanted + limit_release * (self.limit - wanted)
            };
            gain *= self.limit;

            for sample in frame.iter_mut() {
                *sample *= gain;
            }
        }
    }
}

/// Where the centre channel sits in an interleaved frame. Channels are laid out
/// in ascending position order, so its index is the number of positions below
/// it that are present.
fn centre_index(channels: usize, mask: u64) -> Option<usize> {
    if channels <= 2 || mask & (1 << FRONT_CENTER_BIT) == 0 {
        return None;
    }
    let index = (mask & ((1 << FRONT_CENTER_BIT) - 1)).count_ones() as usize;
    (index < channels).then_some(index)
}

/// Gain change, in dB, a soft-knee downward compressor applies at this level.
fn compression_db(level_db: f32) -> f32 {
    let over = level_db - NIGHT_THRESHOLD_DB;
    let slope = 1.0 / NIGHT_RATIO - 1.0;
    if 2.0 * over < -NIGHT_KNEE_DB {
        0.0
    } else if 2.0 * over > NIGHT_KNEE_DB {
        slope * over
    } else {
        let into_knee = over + NIGHT_KNEE_DB / 2.0;
        slope * into_knee * into_knee / (2.0 * NIGHT_KNEE_DB)
    }
}

fn peak(frame: &[f32]) -> f32 {
    frame.iter().fold(0.0f32, |loudest, s| loudest.max(s.abs()))
}

/// One-pole coefficient reaching ~63% of a step in `seconds`.
fn smoothing(seconds: f32, rate: f32) -> f32 {
    (-1.0 / (seconds * rate)).exp()
}

fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(1e-6).log10()
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIVE_ONE: u64 = 0b11_1111; // FL FR FC LFE RL RR

    fn fx(effects: AudioEffects, channels: usize, mask: u64) -> AudioFx {
        let mut fx = AudioFx::default();
        fx.set_format(channels, 48_000, mask);
        fx.set_effects(effects);
        fx
    }

    #[test]
    fn leaves_samples_bit_exact_when_off() {
        let mut fx = fx(AudioEffects::default(), 2, 0b11);
        let original: Vec<f32> = (0..960).map(|i| (i as f32 * 0.37).sin() * 1.3).collect();
        let mut samples = original.clone();
        fx.process(&mut samples);
        assert_eq!(samples, original);
    }

    #[test]
    fn finds_the_centre_channel() {
        assert_eq!(centre_index(6, FIVE_ONE), Some(2));
        assert_eq!(centre_index(2, 0b11), None);
        // 4.0 without a centre.
        assert_eq!(centre_index(4, 0b11_0011), None);
        // Unpositioned channels give nothing to go on.
        assert_eq!(centre_index(6, 0), None);
    }

    #[test]
    fn dialogue_lifts_the_centre_over_the_rest() {
        let effects = AudioEffects { dialogue: true, night: false };
        let mut fx = fx(effects, 6, FIVE_ONE);
        let mut samples = [0.1f32; 6];
        fx.process(&mut samples);
        let ratio = samples[2] / samples[0];
        assert!((ratio - 2.0).abs() < 1e-3, "centre should be 6 dB up, got {ratio}");
    }

    #[test]
    fn dialogue_leaves_stereo_alone() {
        let effects = AudioEffects { dialogue: true, night: false };
        let mut fx = fx(effects, 2, 0b11);
        let mut samples = [0.25f32, -0.5];
        fx.process(&mut samples);
        assert_eq!(samples, [0.25, -0.5]);
    }

    #[test]
    fn boost_never_clips() {
        let mut fx = fx(AudioEffects::default(), 2, 0b11);
        fx.set_boost(1.5);
        let mut samples: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.05).sin() * 0.95).collect();
        fx.process(&mut samples);
        let loudest = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(loudest <= LIMIT_CEILING + 1e-6, "peaked at {loudest}");
    }

    #[test]
    fn boost_raises_quiet_material_cleanly() {
        let mut fx = fx(AudioEffects::default(), 1, 0);
        fx.set_boost(1.5);
        let mut samples = vec![0.2f32; 100];
        fx.process(&mut samples);
        assert!(samples.iter().all(|s| (s - 0.3).abs() < 1e-6));
    }

    #[test]
    fn night_mode_narrows_the_gap_between_quiet_and_loud() {
        let effects = AudioEffects { dialogue: false, night: true };
        let settle = |level: f32| {
            let mut fx = fx(effects, 1, 0);
            let mut samples = vec![level; 48_000];
            fx.process(&mut samples);
            *samples.last().unwrap()
        };
        let (quiet, loud) = (settle(0.01), settle(0.8));
        let before = gain_to_db(0.8) - gain_to_db(0.01);
        let after = gain_to_db(loud) - gain_to_db(quiet);
        assert!(after < before - 8.0, "range {before:.1} dB became {after:.1} dB");
        assert!(quiet > 0.01, "quiet material should come up");
        assert!(loud <= LIMIT_CEILING + 1e-6);
    }
}
