//! The beep an alarm falls back to when its station doesn't play: a WAV
//! made on the fly, so the engine plays it like any station
//!
//! Four short beeps, then a pause, again and again. The file says it is
//! about a day long; the alarm stops it long before.

use std::f32::consts::TAU;
use std::io::{self, Read, Seek, SeekFrom};

const RATE: u32 = 22_050;
const HEADER_LEN: u64 = 44;
/// 16-bit mono samples, a little over a day of them
const DATA_LEN: u32 = 0x7FFF_0000;
const PITCH: f32 = 880.0;
const LOUDNESS: f32 = 0.5;

/// One cycle: four beeps of 0.1 s with 0.1 s gaps, then 0.6 s quiet
const BEEP: u32 = RATE / 10;
const BEEPS: u32 = 4;
const CYCLE: u32 = BEEP * 2 * BEEPS + RATE * 6 / 10;
/// Rise and fall of each beep, so it doesn't click
const EDGE: u32 = RATE / 200;

/// Reads as a WAV file of the alarm beep
pub struct AlarmTone {
    pos: u64,
}

impl AlarmTone {
    pub fn new() -> Self {
        Self { pos: 0 }
    }

    fn len() -> u64 {
        HEADER_LEN + u64::from(DATA_LEN)
    }

    fn header() -> [u8; HEADER_LEN as usize] {
        let mut h = [0u8; HEADER_LEN as usize];
        h[0..4].copy_from_slice(b"RIFF");
        h[4..8].copy_from_slice(&(DATA_LEN + 36).to_le_bytes());
        h[8..12].copy_from_slice(b"WAVE");
        h[12..16].copy_from_slice(b"fmt ");
        h[16..20].copy_from_slice(&16u32.to_le_bytes());
        h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
        h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
        h[24..28].copy_from_slice(&RATE.to_le_bytes());
        h[28..32].copy_from_slice(&(RATE * 2).to_le_bytes());
        h[32..34].copy_from_slice(&2u16.to_le_bytes());
        h[34..36].copy_from_slice(&16u16.to_le_bytes());
        h[36..40].copy_from_slice(b"data");
        h[40..44].copy_from_slice(&DATA_LEN.to_le_bytes());
        h
    }

    /// The sample at this index, from -1 to 1
    fn sample(index: u64) -> f32 {
        let at = (index % u64::from(CYCLE)) as u32;
        if at >= BEEP * 2 * BEEPS || at % (BEEP * 2) >= BEEP {
            return 0.0;
        }
        let into = at % (BEEP * 2);
        let edge = into.min(BEEP - 1 - into).min(EDGE) as f32 / EDGE as f32;
        (TAU * PITCH * into as f32 / RATE as f32).sin() * LOUDNESS * edge
    }

    fn byte(pos: u64) -> u8 {
        if pos < HEADER_LEN {
            return Self::header()[pos as usize];
        }
        let offset = pos - HEADER_LEN;
        let value = (Self::sample(offset / 2) * f32::from(i16::MAX)) as i16;
        value.to_le_bytes()[(offset % 2) as usize]
    }
}

impl Read for AlarmTone {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = Self::len().saturating_sub(self.pos);
        let n = (buf.len() as u64).min(left) as usize;
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = Self::byte(self.pos + i as u64);
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for AlarmTone {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(n) => Self::len().checked_add_signed(n),
            SeekFrom::Current(n) => self.pos.checked_add_signed(n),
        };
        let pos = pos.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "before start"))?;
        self.pos = pos;
        Ok(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_is_a_wav_that_beeps_and_pauses() {
        let mut tone = AlarmTone::new();
        let mut head = [0u8; 44];
        tone.read_exact(&mut head).unwrap();
        assert_eq!(&head[0..4], b"RIFF");
        assert_eq!(&head[36..40], b"data");
        assert_eq!(u32::from_le_bytes(head[24..28].try_into().unwrap()), RATE);

        let loud = |from: u32, to: u32| {
            (from..to)
                .map(|i| AlarmTone::sample(i.into()).abs())
                .fold(0.0, f32::max)
        };
        // The first beep, the gap after it, and the pause at the end
        assert!(loud(0, BEEP) > 0.4);
        assert_eq!(loud(BEEP, BEEP * 2), 0.0);
        assert_eq!(loud(BEEP * 2 * BEEPS, CYCLE), 0.0);
        // Its edges are soft
        assert!(AlarmTone::sample(0).abs() < 0.01);
    }

    #[test]
    fn the_engine_decoder_plays_it() {
        let source =
            radiotrope::audio::SymphoniaSource::new_with_hint(AlarmTone::new(), Some("wav"))
                .unwrap();
        // The first beep, as the engine would hear it
        let peak = source.take(BEEP as usize).map(f32::abs).fold(0.0, f32::max);
        assert!(peak > 0.4, "{peak}");
    }

    #[test]
    fn it_seeks_like_a_file() {
        let mut tone = AlarmTone::new();
        assert_eq!(tone.seek(SeekFrom::End(0)).unwrap(), AlarmTone::len());
        let mut buf = [0u8; 8];
        assert_eq!(tone.read(&mut buf).unwrap(), 0);
        tone.seek(SeekFrom::Start(46)).unwrap();
        tone.read_exact(&mut buf).unwrap();
        assert_eq!(buf[0], AlarmTone::byte(46));
        assert!(tone.seek(SeekFrom::Current(-1000)).is_err());
    }
}
