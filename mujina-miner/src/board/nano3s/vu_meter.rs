//! Audio level shared by the Nano3S LED strip and its preview.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

pub(crate) const SAMPLE_RATE: u32 = 48_000;
const BYTES_PER_FRAME: usize = 4; // 16-bit little-endian stereo
const MAX_AGE: Duration = Duration::from_millis(500);

struct Reading {
    session: u64,
    bars: f32,
    sampled_at: Option<Instant>,
}

static READING: Mutex<Reading> = Mutex::new(Reading {
    session: 0,
    bars: 0.0,
    sampled_at: None,
});

pub(crate) fn bars() -> f32 {
    let reading = READING.lock().unwrap_or_else(|e| e.into_inner());
    if reading.sampled_at.is_some_and(|at| at.elapsed() < MAX_AGE) {
        reading.bars
    } else {
        0.0
    }
}

pub(crate) struct PcmMeter {
    session: u64,
    envelope: f32,
}

impl PcmMeter {
    pub(crate) fn start() -> Self {
        let mut reading = READING.lock().unwrap_or_else(|e| e.into_inner());
        reading.session = reading.session.wrapping_add(1);
        reading.sampled_at = None;
        Self {
            session: reading.session,
            envelope: 0.0,
        }
    }

    pub(crate) fn measure(&mut self, pcm: &[u8]) -> f32 {
        let samples = pcm.chunks_exact(2);
        let count = samples.len();
        if count == 0 {
            return 0.0;
        }
        let mean_square = samples
            .map(|sample| {
                let amplitude = f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.0;
                amplitude * amplitude
            })
            .sum::<f32>()
            / count as f32;
        let elapsed = (pcm.len() / BYTES_PER_FRAME) as f32 / SAMPLE_RATE as f32;
        // A steady tone reaches approximately 99% of its reading in 300 ms.
        let blend = 1.0 - (-elapsed / 0.065).exp();
        self.envelope += (mean_square.sqrt() - self.envelope) * blend;
        // The red segment starts at -10 dBFS, leaving headroom for peaks.
        ((20.0 * self.envelope.log10() + 38.0) / 3.5).clamp(0.0, 9.0)
    }

    pub(crate) fn publish(&self, bars: f32) {
        let mut reading = READING.lock().unwrap_or_else(|e| e.into_inner());
        if reading.session == self.session {
            reading.bars = bars;
            reading.sampled_at = Some(Instant::now());
        }
    }
}

impl Drop for PcmMeter {
    fn drop(&mut self) {
        let mut reading = READING.lock().unwrap_or_else(|e| e.into_inner());
        if reading.session == self.session {
            reading.sampled_at = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_stereo_energy_and_ballistics_settle() {
        let mut meter = PcmMeter {
            session: u64::MAX,
            envelope: 0.0,
        };
        let silence = [0; 3840];
        let stereo = [0, 32, 0, 224].repeat(960); // +0.25 left, -0.25 right
        assert_eq!(meter.measure(&silence), 0.0);
        let first = meter.measure(&stereo);
        for _ in 1..15 {
            meter.measure(&stereo);
        }
        assert!((meter.envelope - 0.25).abs() < 0.003);
        assert!((6.0..8.0).contains(&meter.measure(&stereo)));
        assert!((0.0..6.0).contains(&first));
        for _ in 0..25 {
            meter.measure(&silence);
        }
        assert_eq!(meter.measure(&silence), 0.0);
    }

    #[test]
    fn previous_session_cannot_replace_or_clear_current_level() {
        let old = PcmMeter::start();
        let current = PcmMeter::start();
        current.publish(6.0);
        old.publish(9.0);
        drop(old);
        assert_eq!(bars(), 6.0);
        drop(current);
        assert_eq!(bars(), 0.0);
    }
}
