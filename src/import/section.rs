//! A stretch of a stream: from an in point to an out point, once or round and
//! round with no gap.

use cpal::{Error, ErrorKind, SampleFormat};

use super::AudioStream;
use crate::output::SampleType;

/// Part of a stream — frames `start` up to `end` — played once, or looped.
///
/// Looping, the end of one pass runs straight into the start of the next
/// within a single [`read`](AudioStream::read) sequence: the stream seeks back
/// to `start` the moment `end` is reached, so whatever schedules the samples
/// sees one unbroken run and the loop is seamless. A looping section never
/// ends; stop reading it.
///
/// ```no_run
/// # fn main() -> Result<(), cpal::Error> {
/// use atome::import::{self, Section};
///
/// let stream = import::stream::<f32>("bed.flac".as_ref())?;
/// // 1.5 s to 9 s at 48 kHz, round and round.
/// let mut bed = Section::new(stream, 72_000, Some(432_000), true)?;
/// # let _ = &mut bed;
/// # Ok(()) }
/// ```
pub struct Section<S: SampleType> {
    stream: Box<dyn AudioStream<S>>,
    start: u64,
    end: Option<u64>,
    looping: bool,
    /// The frame the next read starts at, in the whole stream's terms.
    at: u64,
    /// Passes finished.
    passes: u64,
}

impl<S: SampleType> Section<S> {
    /// `stream` from frame `start` to frame `end` (exclusive; `None` is its
    /// end), once or with `looping` round and round. Moves the stream to
    /// `start` now.
    ///
    /// # Errors
    ///
    /// An `end` that is not after `start`, and whatever the stream's
    /// [`seek`](AudioStream::seek) refuses — a section that does not start at
    /// zero, or that loops, needs a stream that can seek.
    pub fn new(mut stream: Box<dyn AudioStream<S>>, start: u64, end: Option<u64>, looping: bool) -> Result<Self, Error> {
        if end.is_some_and(|end| end <= start) {
            return Err(Error::with_message(
                ErrorKind::InvalidInput,
                format!("a section must end after it starts, not at frame {} from {start}", end.unwrap_or(0)),
            ));
        }
        let at = if start > 0 { stream.seek(start)? } else { 0 };
        Ok(Section { stream, start, end, looping, at, passes: 0 })
    }

    /// How many times it has run to its end and gone back to its start.
    pub fn passes(&self) -> u64 {
        self.passes
    }

    /// Frames in one pass, where that is known.
    pub fn pass_frames(&self) -> Option<u64> {
        self.end.or(self.stream.total_frames()).map(|end| end.saturating_sub(self.start))
    }

    /// Back to the start for the next pass. False if a pass would be empty,
    /// which would otherwise spin.
    fn wrap(&mut self) -> Result<bool, Error> {
        if self.at <= self.start {
            return Ok(false);
        }
        self.at = self.stream.seek(self.start)?;
        self.passes += 1;
        Ok(true)
    }
}

impl<S: SampleType> AudioStream<S> for Section<S> {
    fn sample_rate(&self) -> u32 {
        self.stream.sample_rate()
    }

    fn channels(&self) -> u16 {
        self.stream.channels()
    }

    fn source_format(&self) -> SampleFormat {
        self.stream.source_format()
    }

    /// One pass's length; none while looping, which never ends.
    fn total_frames(&self) -> Option<u64> {
        if self.looping {
            None
        } else {
            self.pass_frames()
        }
    }

    fn read(&mut self, out: &mut [S]) -> Result<usize, Error> {
        let channels = usize::from(self.stream.channels().max(1));
        loop {
            let mut room = out.len() / channels * channels;
            if let Some(end) = self.end {
                let left = end.saturating_sub(self.at) as usize;
                if left == 0 {
                    if self.looping && self.wrap()? {
                        continue;
                    }
                    return Ok(0);
                }
                room = room.min(left.saturating_mul(channels));
            }
            if room == 0 {
                return Ok(0);
            }

            let read = self.stream.read(&mut out[..room])?;
            if read == 0 {
                if self.looping && self.wrap()? {
                    continue;
                }
                return Ok(0);
            }
            self.at += (read / channels) as u64;
            return Ok(read);
        }
    }

    /// `frame` counted from the section's start.
    fn seek(&mut self, frame: u64) -> Result<u64, Error> {
        let target = self.start.saturating_add(frame);
        let target = self.end.map_or(target, |end| target.min(end));
        self.at = self.stream.seek(target)?;
        Ok(self.at - self.start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mono frames counting up from 0, as `f32`, seekable.
    struct Ramp {
        len: u64,
        at: u64,
    }

    impl AudioStream<f32> for Ramp {
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u16 {
            1
        }
        fn source_format(&self) -> SampleFormat {
            SampleFormat::F32
        }
        fn total_frames(&self) -> Option<u64> {
            Some(self.len)
        }
        fn read(&mut self, out: &mut [f32]) -> Result<usize, Error> {
            // Short reads on purpose: at most 7 at a time.
            let count = out.len().min(7).min((self.len - self.at) as usize);
            for (offset, slot) in out[..count].iter_mut().enumerate() {
                *slot = (self.at + offset as u64) as f32;
            }
            self.at += count as u64;
            Ok(count)
        }
        fn seek(&mut self, frame: u64) -> Result<u64, Error> {
            self.at = frame.min(self.len);
            Ok(self.at)
        }
    }

    fn take(section: &mut Section<f32>, frames: usize) -> Vec<u32> {
        let mut got = Vec::new();
        let mut block = [0.0_f32; 16];
        while got.len() < frames {
            let read = section.read(&mut block).unwrap();
            if read == 0 {
                break;
            }
            got.extend(block[..read].iter().map(|sample| *sample as u32));
        }
        got.truncate(frames);
        got
    }

    #[test]
    fn a_section_plays_from_its_in_to_its_out() {
        let mut section = Section::new(Box::new(Ramp { len: 100, at: 0 }), 10, Some(20), false).unwrap();
        assert_eq!(take(&mut section, 100), (10..20).collect::<Vec<_>>());
        assert_eq!(section.total_frames(), Some(10));
    }

    #[test]
    fn a_looping_section_runs_into_its_start_with_nothing_between() {
        let mut section = Section::new(Box::new(Ramp { len: 100, at: 0 }), 10, Some(13), true).unwrap();
        assert_eq!(take(&mut section, 9), [10, 11, 12, 10, 11, 12, 10, 11, 12]);
        assert_eq!(section.passes(), 2);
        assert_eq!(section.total_frames(), None, "a loop never ends");
    }

    #[test]
    fn with_no_out_point_a_loop_wraps_at_the_end() {
        let mut section = Section::new(Box::new(Ramp { len: 5, at: 0 }), 3, None, true).unwrap();
        assert_eq!(take(&mut section, 6), [3, 4, 3, 4, 3, 4]);
    }

    #[test]
    fn an_out_before_the_in_is_refused() {
        assert!(Section::new(Box::new(Ramp { len: 5, at: 0 }), 3, Some(3), false).is_err());
    }
}
