use cpal::traits::DeviceTrait;
use cpal::{Device, Error, ErrorKind, SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::marker::PhantomData;


pub mod mixer;
pub mod types;
pub mod utils;

pub use mixer::{ClearSignal, MixCommand, Mixer, MixerHandle};
pub use types::{OutputType, SampleRate, SampleType};
pub use utils::{default_device, find_device, list_device_names, list_devices, device_name};

/// Frames assumed when the caller asks for the device's default buffer size.
/// Only used to size the ring buffer — the stream itself still asks the device
/// for its own preferred size.
///
/// The ring holds exactly one buffer size, so this is a guess at a number the
/// device actually decides. If the device then picks a period *larger* than
/// this, the ring cannot hold a whole callback and every callback gets a
/// partially filled buffer. Pass an explicit `buffer_size` to make the two match.
const DEFAULT_BUFFER_FRAMES: i32 = 1024;

/// How many `MixCommand`s can be queued before `add_samples` starts refusing.
const COMMAND_QUEUE_LEN: usize = 256;

/// # OutputClass
///
/// Used to hold a stream to a single output.
///
/// `S` *is* the sample type this output speaks: `OutputClass<f32>`,
/// `OutputClass<i16>`, `OutputClass<cpal::I24>`, and so on. Fixing it at the
/// type level means the callback is picked once, when the stream is built,
/// instead of re-deciding on every audio callback, and `add_samples` can only
/// ever be handed matching data.

pub struct OutputClass<S: SampleType> {
    // Number of channels in this output
    channels: u16,
    // Output device to use
    device: Device,
    // Type of device
    out_type: OutputType,
    // The buffer size in frames, one for all outputs; `None` = device default
    buffer_size: Option<i32>,
    // The sample rate
    sample_rate: SampleRate,
    // stream for cpal
    stream: Option<Stream>,
    // Config the stream is (or will be) built with
    stream_config: StreamConfig,
    // Mixer input queue: `add_samples` writes indexed work here, never audio
    commands: HeapProd<MixCommand<S>>,
    // Consumer half of the audio ring buffer, sized to exactly one buffer size:
    // taken by `build_stream` and moved into the audio callback. The mixer owns
    // the producer half and keeps it topped up.
    consumer: Option<HeapCons<S>>,
    // Keeps the mixer thread running for as long as this output exists
    mixer: MixerHandle,
    // The sample type lives only in the type system; nothing is stored for it.
    sample_type: PhantomData<S>,
}

impl<S: SampleType> OutputClass<S> {
    pub fn new(device: Option<Device>, out_type: OutputType, channels: u16, sample_rate:SampleRate, buffer_size: Option<i32>) -> Self {
        // `None` lets the device pick its own buffer size. We can't know that
        // size until the stream is built, so the ring buffer is sized off a
        // default instead.
        let buffer_size_form = match buffer_size {
            Some(frames) => cpal::BufferSize::Fixed(frames as u32),
            None => cpal::BufferSize::Default,
        };
        let frames = buffer_size.unwrap_or(DEFAULT_BUFFER_FRAMES).max(1) as usize;


        let stream_config = StreamConfig {
            channels,
            sample_rate: sample_rate as u32,
            buffer_size: buffer_size_form,
        };

        // Exactly one buffer size: this ring holds only the samples about to go
        // to CPAL, never more. Everything further ahead stays in the mixer's own
        // buffer, where later commands can still be summed into it.
        let capacity = frames * channels.max(1) as usize;
        let (audio_producer, consumer) = HeapRb::<S>::new(capacity).split();

        // add_samples -> commands -> mixer -> audio ring buffer -> callback.
        let (commands, pending) = HeapRb::<MixCommand<S>>::new(COMMAND_QUEUE_LEN).split();
        let mixer = MixerHandle::spawn(Mixer::new(pending, audio_producer));

        OutputClass {
            channels,
            device: device
                .unwrap_or_else(|| default_device().expect("no default output device available")),
            out_type,
            buffer_size,
            sample_rate,
            stream: None,
            stream_config,
            commands,
            consumer: Some(consumer),
            mixer,
            sample_type: PhantomData,
        }
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }
    pub fn device(&self) -> Device {
        self.device.clone()
    }
    pub fn name(&self) -> String {
        device_name(&self.device)
    }
    /// The configured buffer size in frames, or `None` if the device picks it.
    pub fn buffer_size(&self) -> Option<i32> {
        self.buffer_size
    }
    pub fn sample_rate(&self) -> i32 {
        self.sample_rate as i32
    }
    /// The value-level tag for `S`, for when a `SampleFormat` is needed at runtime.
    pub fn sample_format(&self) -> SampleFormat {
        S::format()
    }

    pub fn sample_type(&self) -> SampleFormat {
        S::format()
    }
    pub fn out_type(&self) -> OutputType {
        self.out_type
    }
    /// The body of the output callback.
    ///
    /// The ring buffer already holds `S`, so this pops one callback's worth of
    /// interleaved samples straight into the device's buffer — no conversion, no
    /// scratch buffer, no allocation on the audio thread. `add_samples` queues
    /// frames interleaved (L,R,L,R,…) and the ring buffer is FIFO, so they go
    /// out in exactly the order they came in.
    ///
    /// This is also the only place the ring buffer can be emptied — the mixer
    /// holds the producer half and cannot take anything back out — so a
    /// [`stop`](Self::stop) is finished here, not where it is asked for.
    fn data_callback(
        data: &mut [S],
        buffer: &mut HeapCons<S>,
        clear: &ClearSignal,
        cleared: &mut usize,
    ) {
        let epoch = clear.epoch();
        if epoch != *cleared {
            // A stop is in flight: drop what was already committed and play
            // silence instead of it. This keeps happening until the mixer has
            // answered the same stop, because until then it may still be
            // flushing pre-stop samples into the ring behind us.
            buffer.clear();
            data.fill(S::SILENCE);
            if clear.acked() == epoch {
                *cleared = epoch;
            }
            return;
        }

        let read = buffer.pop_slice(data);
        if read < data.len() {
            // Underrun: pad with silence rather than replaying stale audio.
            data[read..].fill(S::SILENCE);
        }
    }

    /// Builds the cpal output stream for this output's device, pulling mixed
    /// samples out of its ring buffer and writing them as `S`. The resulting
    /// `Stream` is stored on `self` (streams must be kept alive to keep playing)
    /// and also returned.
    ///
    /// Consumes the consumer half of the ring buffer, so it can only be called
    /// once per `OutputClass`.
    pub fn build_stream(&mut self) -> Result<&Stream, Error> {
        let mut buffer = self.consumer.take().ok_or_else(|| {
            Error::with_message(
                ErrorKind::UnsupportedOperation,
                "stream already built for this output",
            )
        })?;

        // Shared with the mixer thread so `stop` can reach the samples that are
        // already past it, sitting in the ring buffer.
        let clear = self.mixer.clear_signal();
        let mut cleared = clear.epoch();

        let stream = self.device.build_output_stream(
            self.stream_config,
            move |data: &mut [S], _: &cpal::OutputCallbackInfo| {
                Self::data_callback(data, &mut buffer, &clear, &mut cleared)
            },
            Self::err_fn,
            None,
        )?;

        self.stream = Some(stream);
        Ok(self.stream.as_ref().unwrap())
    }

    /// Schedules interleaved samples for playback at absolute sample `index`,
    /// summing them with anything already scheduled there. They stay in this
    /// output's sample type the whole way — `&[i16]` into an `OutputClass<i16>`
    /// is queued, mixed, and played as `i16`, never converted.
    ///
    /// This only hands a [`MixCommand`] to the mixer — no audio touches the
    /// stream's ring buffer here. Returns the index just past the batch, ready
    /// to pass straight back in for the next one.
    ///
    /// Fails if the mixer's command queue is full, which means it is not
    /// draining fast enough; the whole batch is refused rather than half of it.
    pub fn add_samples(&mut self, samples: &[S], index: usize) -> Result<usize, Error> {
        
        
        let command = MixCommand {
            index,
            samples: samples.to_vec(),
        };

        self.commands.try_push(command).map_err(|_| {
            Error::with_message(ErrorKind::ResourceExhausted, "mixer command queue is full")
        })?;

        Ok(index + samples.len())
    }
    /// Schedules interleaved samples for playback at a `time`.
    /// Time is in secounds from now, so it will delay the start of the audio 
    pub fn add_samples_time(&mut self, samples: &[S], time: usize) -> Result<usize, Error> {
        
        let index = time * self.sample_rate
        
        let command = MixCommand {
            index,
            samples: samples.to_vec(),
        };

        self.commands.try_push(command).map_err(|_| {
            Error::with_message(ErrorKind::ResourceExhausted, "mixer command queue is full")
        })?;

        Ok(index + samples.len())
    }

    /// Deletes all samples that have been scheduled but not yet played, and resets the
    /// mixer to the current time. This is useful if you want to stop all audio
    /// immediately and start fresh.
    ///
    /// Both stages are cleared: the mixer's queued commands and accumulation
    /// buffer, and the ring buffer the audio callback is already reading from —
    /// otherwise up to one buffer of committed audio would still play out.
    ///
    /// The stream keeps running and plays silence; the output stays usable and
    /// [`add_samples`](Self::add_samples) works again straight away. It is not
    /// instant: the mixer answers on its next pass and the callback on its
    /// next, so the tail is at most one buffer long.
    ///
    /// The play cursor does not move: it is an absolute position in the stream,
    /// not a count of what got played, so anything scheduled behind it is still
    /// late after a stop.
    pub fn stop(&mut self) {
        self.mixer.clear_samples();
    }

    /// Handle to the mixer thread feeding this output.
    pub fn mixer(&self) -> &MixerHandle {
        &self.mixer
    }
    /// Aligns the samples to the output's sample rate and channel count.
    ///
    /// Set `interleaved` to describe what you are handing in: `true` for frames
    /// (L,R,L,R,…), `false` for planar data, where each channel is one
    /// contiguous run. Planar input is woven into frames before anything else
    /// happens to it.
    ///
    /// The result is always interleaved at `self.channels` / `self.sample_rate`,
    /// ready to hand to [`add_samples`](Self::add_samples). Fails if `channels`
    /// is zero or `samples` does not hold a whole number of frames.
    pub fn align_samples(&self, samples: &[S], sample_rate: SampleRate, channels: u16, interleaved: bool) -> Result<Vec<S>, Error> {
        if channels == 0 {
            return Err(Error::with_message(
                ErrorKind::InvalidInput,
                "sample data must have at least one channel",
            ));
        }
        let src_channels = channels as usize;
        if samples.len() % src_channels != 0 {
            return Err(Error::with_message(
                ErrorKind::InvalidInput,
                "sample count is not a whole number of frames",
            ));
        }
        // The device always takes interleaved data, so interleaved input that
        // already matches the output needs nothing done to it.
        if sample_rate == self.sample_rate && channels == self.channels && interleaved {
            return Ok(samples.to_vec());
        }

        let dst_channels = self.channels.max(1) as usize;
        let frames = samples.len() / src_channels;

        // Weave first: everything after this point indexes by frame, so planar
        // input has to become frames before it can be remapped or resampled.
        // One channel is the same bytes either way, so it skips the copy.
        let mut buffer = if interleaved || src_channels == 1 {
            samples.to_vec()
        } else {
            Self::weave(samples, frames, src_channels)
        };

        if src_channels != dst_channels {
            buffer = Self::map_channels(&buffer, frames, src_channels, dst_channels);
        }

        if sample_rate != self.sample_rate {
            buffer = Self::resample(
                &buffer,
                frames,
                dst_channels,
                sample_rate as u32,
                self.sample_rate as u32,
            );
        }

        Ok(buffer)
    }

    /// Weaves planar samples — channel 0's `frames` samples, then channel 1's,
    /// and so on — into interleaved frames (L,R,L,R,…), the only layout the
    /// mixer and the device ever see.
    fn weave(samples: &[S], frames: usize, channels: usize) -> Vec<S> {
        let mut out = Vec::with_capacity(samples.len());
        for frame in 0..frames {
            for channel in 0..channels {
                out.push(samples[(channel * frames) + frame]);
            }
        }
        out
    }

    /// Re-lays `frames` interleaved frames of `src_channels` into `dst_channels`.
    ///
    /// Mono input fans out to every output channel and a mono output sums the
    /// input down; anything else keeps channel `n` as channel `n`, dropping
    /// the extras when narrowing and filling with silence when widening. That
    /// is deliberately dumb about channel *meaning* — a 5.1 stream narrowed to
    /// stereo keeps front-left/front-right and loses the rest rather than
    /// folding them in.
    fn map_channels(samples: &[S], frames: usize, src_channels: usize, dst_channels: usize) -> Vec<S> {
        let mut out = Vec::with_capacity(frames * dst_channels);
        for frame in 0..frames {
            let start = frame * src_channels;
            if src_channels == 1 {
                out.extend(std::iter::repeat(samples[start]).take(dst_channels));
            } else if dst_channels == 1 {
                // Summed with the format's own `mix`, so an `i16` source stays
                // `i16` the whole way down — no float round-trip. Folding N
                // channels into one is correspondingly louder and can overflow
                // the format, so attenuate before calling if the source is hot.
                let mut sum = S::SILENCE;
                for sample in &samples[start..start + src_channels] {
                    sum = sum.mix(*sample);
                }
                out.push(sum);
            } else {
                for channel in 0..dst_channels {
                    out.push(if channel < src_channels {
                        samples[start + channel]
                    } else {
                        S::SILENCE
                    });
                }
            }
        }
        out
    }

    /// Resamples interleaved `frames` from `from` Hz to `to` Hz, linearly
    /// interpolating between the two input frames each output frame falls
    /// between.
    ///
    /// Linear interpolation is cheap and has no pre-ring, but it is not
    /// band-limited: downsampling folds anything above the new Nyquist back
    /// into the audible range, so low-pass filter first if the source has real
    /// content up there.
    fn resample(samples: &[S], frames: usize, channels: usize, from: u32, to: u32) -> Vec<S> {
        if frames == 0 {
            return Vec::new();
        }
        let ratio = to as f64 / from as f64;
        let out_frames = ((frames as f64) * ratio).round().max(1.0) as usize;
        let last = frames - 1;

        let mut out = Vec::with_capacity(out_frames * channels);
        for frame in 0..out_frames {
            // Where this output frame lands on the input timeline.
            let position = frame as f64 / ratio;
            let left = (position.floor() as usize).min(last);
            let right = (left + 1).min(last);
            let t = (position - position.floor()) as f32;
            for channel in 0..channels {
                let a = samples[left * channels + channel].to_f32();
                let b = samples[right * channels + channel].to_f32();
                out.push(S::from_f32(a + (b - a) * t));
            }
        }
        out
    }

    fn err_fn(err: cpal::Error) {
        eprintln!("audio output stream error: {}", err);
    }

}
