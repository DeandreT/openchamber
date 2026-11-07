//! The pure parts of dictation: microphone samples to the 16 kHz mono PCM16
//! chunks the server transcribes (`use-dictation-audio-source.ts`), the
//! waveform level, the recording clock and where a transcript lands in the
//! composer (`appendInlineText`).

/// The rate the server's transcription reads.
pub const OUTPUT_RATE: u32 = 16_000;
/// One chunk is about a second of audio.
pub const CHUNK_SAMPLES: usize = OUTPUT_RATE as usize;

/// `floatToInt16`.
fn to_pcm16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    if clamped < 0.0 {
        (clamped * 32768.0).round() as i16
    } else {
        (clamped * 32767.0).round() as i16
    }
}

/// Resamples a continuous mono stream linearly to 16 kHz and cuts it into
/// one-second PCM16 chunks.
#[derive(Clone, Debug)]
pub struct Chunker {
    /// Source samples per output sample.
    step: f64,
    /// Where the next output sample falls, in source samples from the start
    /// of the next buffer; negative values reach into `previous`.
    position: f64,
    previous: f32,
    pending: Vec<i16>,
}

impl Chunker {
    pub fn new(input_rate: u32) -> Self {
        Self {
            step: f64::from(input_rate.max(1)) / f64::from(OUTPUT_RATE),
            position: 0.0,
            previous: 0.0,
            pending: Vec::with_capacity(CHUNK_SAMPLES),
        }
    }

    /// Takes mono samples in -1..1 and returns every chunk they complete.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<i16>> {
        let mut chunks = Vec::new();
        if samples.is_empty() {
            return chunks;
        }
        let sample_at = |index: isize, previous: f32| {
            if index < 0 {
                previous
            } else {
                samples[index as usize]
            }
        };
        let last = samples.len() as f64 - 1.0;
        while self.position <= last {
            let floor = self.position.floor();
            let i0 = floor as isize;
            let fraction = (self.position - floor) as f32;
            let a = sample_at(i0, self.previous);
            let b = sample_at((i0 + 1).min(last as isize), self.previous);
            self.pending.push(to_pcm16(a + (b - a) * fraction));
            if self.pending.len() == CHUNK_SAMPLES {
                chunks.push(std::mem::replace(
                    &mut self.pending,
                    Vec::with_capacity(CHUNK_SAMPLES),
                ));
            }
            self.position += self.step;
        }
        self.position -= samples.len() as f64;
        self.previous = samples[samples.len() - 1];
        chunks
    }

    /// The partial chunk left when recording stops.
    pub fn finish(&mut self) -> Option<Vec<i16>> {
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }
}

/// Mixes interleaved frames to mono by averaging their channels.
pub fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// The waveform's 0..1 level: twice the buffer's RMS, as the web draws it.
pub fn level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|sample| sample * sample).sum();
    ((sum / samples.len() as f32).sqrt() * 2.0).clamp(0.0, 1.0)
}

/// Little-endian PCM16 as standard base64, the chunk's wire form.
pub fn pcm16_base64(samples: &[i16]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let value = (u32::from(group[0]) << 16)
            | (u32::from(*group.get(1).unwrap_or(&0)) << 8)
            | u32::from(*group.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= group.len() {
                out.push(char::from(
                    ALPHABET[(value >> (18 - 6 * index)) as usize & 63],
                ));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `formatDuration`: `m:ss`.
pub fn format_duration(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// `appendInlineText`: the transcript after the draft's text, separated by a
/// space unless the draft already ends in whitespace, with a trailing space.
pub fn append_inline_text(base: &str, next: &str) -> String {
    let next = next.trim();
    if next.is_empty() {
        return base.to_owned();
    }
    if base.is_empty() {
        return format!("{next} ");
    }
    let separator = if base.ends_with(char::is_whitespace) {
        ""
    } else {
        " "
    };
    format!("{base}{separator}{next} ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_48_khz_stream_becomes_one_second_chunks_at_16_khz() {
        let mut chunker = Chunker::new(48_000);
        // 1.5 s of audio in uneven buffers.
        let mut chunks = Vec::new();
        let mut sent = 0;
        for size in [4096, 1000, 30_000, 37_000] {
            let buffer: Vec<f32> = (sent..sent + size).map(|i| (i % 7) as f32 / 10.0).collect();
            sent += size;
            chunks.extend(chunker.push(&buffer));
        }
        assert_eq!(sent, 72_096);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), CHUNK_SAMPLES);
        let rest = chunker.finish().expect("the tail");
        assert_eq!(
            rest.len(),
            72_096 / 3 - CHUNK_SAMPLES,
            "every third sample survives across buffer boundaries"
        );
        assert_eq!(chunker.finish(), None);
        // Every third source sample lands exactly on an output sample.
        assert_eq!(chunks[0][1], to_pcm16(3.0 / 10.0));
    }

    #[test]
    fn upsampling_interpolates_between_buffers() {
        let mut chunker = Chunker::new(8_000);
        chunker.push(&[0.0]);
        chunker.push(&[1.0]);
        let tail = chunker.finish().expect("samples");
        assert_eq!(tail, vec![0, to_pcm16(0.5), 32767]);
    }

    #[test]
    fn samples_clamp_and_scale_like_the_web() {
        assert_eq!(to_pcm16(-2.0), -32768);
        assert_eq!(to_pcm16(1.5), 32767);
        assert_eq!(downmix(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        assert_eq!(level(&[]), 0.0);
        assert_eq!(level(&[0.25, -0.25]), 0.5);
        assert_eq!(level(&[1.0]), 1.0);
    }

    #[test]
    fn chunks_encode_as_standard_base64() {
        assert_eq!(pcm16_base64(&[]), "");
        assert_eq!(pcm16_base64(&[0x0201]), "AQI=");
        assert_eq!(pcm16_base64(&[0x0201, 0x0003]), "AQIDAA==");
        assert_eq!(pcm16_base64(&[-1, -1, -1]), "////////");
    }

    #[test]
    fn transcripts_append_like_append_inline_text() {
        assert_eq!(append_inline_text("", " hello "), "hello ");
        assert_eq!(append_inline_text("Fix", "the bug"), "Fix the bug ");
        assert_eq!(append_inline_text("Fix\n", "the bug"), "Fix\nthe bug ");
        assert_eq!(append_inline_text("keep", "  "), "keep");
        assert_eq!(format_duration(0), "0:00");
        assert_eq!(format_duration(75), "1:15");
    }
}
