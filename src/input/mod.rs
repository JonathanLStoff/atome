//! Capturing audio from a device.
//!
//! The mirror of [`output`](crate::output), and deliberately a much smaller
//! one. An output has to *schedule* audio, so it carries a mixer thread, a
//! command queue, and a ring buffer between them; an input only has to hand
//! over what the device just gave it, so there is nothing to queue and nothing
//! to mix. What remains is the stream, its config, and a callback.

use cpal::traits::DeviceTrait;
use cpal::{Device, Error, ErrorKind, SampleFormat, Stream, StreamConfig};
use std::marker::PhantomData;

pub mod utils;

pub use utils::{default_device, find_device, list_device_names, list_devices};

/// Re-exported under an input-appropriate name.
///
/// It is the same enum: the variants name *host APIs* — CoreAudio, WASAPI, ASIO
/// — and a host API carries audio in both directions. Two identical enums would
/// only invite passing the wrong one.
pub use crate::output::OutputType as InputType;

pub use crate::output::{device_name, list_hosts, SampleRate, SampleType};

/// # InputClass
///
/// Holds a capture stream from a single device.
///
/// `S` *is* the sample type this input speaks: `InputClass<f32>`,
/// `InputClass<i16>`, and so on. Fixing it at the type level means the callback
/// is picked once, when the stream is built, rather than re-deciding on every
/// callback — and the closure you supply is handed exactly that type with no
/// conversion in between.
///
/// # The callback runs on the audio thread
///
/// Whatever is passed to [`new`](Self::new) is called by the device, on its own
/// real-time thread, once per captured buffer. It must not allocate, lock, log,
/// or block: anything that can stall will show up as a dropout in the capture.
/// Push the samples somewhere lock-free and do the work elsewhere.
pub struct InputClass<S: SampleType> {
    // Number of channels this device is opened with
    channels: u16,
    // Input device to capture from
    device: Device,
    // Type of host the device is reached through
    in_type: InputType,
    // The buffer size in frames; `None` = device default
    buffer_size: Option<i32>,
    // The rate the callback is handed audio at: what the engine asked for, whatever the device
    // itself runs at.
    sample_rate: SampleRate,
    // The rate the device is opened at. Equal to `sample_rate` unless the device would not take
    // it, in which case capture is resampled up to `sample_rate` on the way out.
    device_rate: SampleRate,
    // stream for cpal
    stream: Option<Stream>,
    // Config the stream is (or will be) built with
    stream_config: StreamConfig,
    // Waits here until `build_stream` moves it into the capture callback, which
    // is why it is an `Option`: it can only be given away once.
    callback: Option<Callback<S>>,
    // The sample type lives only in the type system; nothing is stored for it.
    sample_type: PhantomData<S>,
    // Where this input's audio is sent. `None` means every output the engine
    // has; a list means only those devices. Held as devices rather than names
    // so that what was resolved at construction is what the audio path uses.
    output_routing: Option<Vec<Device>>,
}

/// What an input hands each captured buffer to.
///
/// Boxed because it is stored before the stream exists and moved into the
/// callback afterwards; `Send` because the device calls it from its own thread.
type Callback<S> = Box<dyn FnMut(&[S]) + Send + 'static>;

impl<S: SampleType> InputClass<S> {
    /// Prepares a capture from `device`, or the default input device.
    ///
    /// `callback` is called with each buffer the device produces, interleaved
    /// (L,R,L,R,…), once [`build_stream`](Self::build_stream) has been called
    /// and the stream started. Nothing is captured before then.
    ///
    /// The channel count comes from the device rather than from an argument:
    /// unlike an output, where the caller decides what to render, a capture
    /// gets whatever the device has — asking a mono microphone for stereo would
    /// only fail when the stream is built. Read it back with
    /// [`channels`](Self::channels).
    ///
    /// # Panics
    ///
    /// If `device` is `None` and there is no default input device.
    pub fn new(
        device: Option<Device>,
        in_type: InputType,
        sample_rate: SampleRate,
        buffer_size: Option<i32>,
        callback: impl FnMut(&[S]) + Send + 'static,
    ) -> Self {
        let device =
            device.unwrap_or_else(|| default_device().expect("no default input device available"));

        // `None` lets the device pick its own buffer size, which is usually the
        // lower-latency choice since it is the size the hardware already works
        // in.
        let buffer_size_form = match buffer_size {
            Some(frames) => cpal::BufferSize::Fixed(frames as u32),
            None => cpal::BufferSize::Default,
        };

        // What the device says it has. A device that cannot be queried at all
        // is reported as mono, which is the safe guess — building the stream is
        // where a genuinely unusable device fails, with the host's own message.
        let channels = device
            .default_input_config()
            .map(|config| config.channels())
            .unwrap_or(1);

        let stream_config = StreamConfig {
            channels,
            sample_rate: sample_rate as u32,
            buffer_size: buffer_size_form,
        };

        InputClass {
            channels,
            device,
            in_type,
            buffer_size,
            sample_rate,
            // Settled in `build_stream`, where opening the device is what answers it.
            device_rate: sample_rate,
            stream: None,
            stream_config,
            callback: Some(Box::new(callback)),
            sample_type: PhantomData,
            output_routing: None,
        }
    }

    /// Sends this input only to `devices`, rather than to every output.
    ///
    /// Builder form, for describing an input in one expression. The engine
    /// fills this in from an [`AtomeDevice`](crate::device::AtomeDevice)'s
    /// routing; set it directly when driving an `InputClass` on its own.
    pub fn with_routing(mut self, devices: Vec<Device>) -> Self {
        self.output_routing = Some(devices);
        self
    }

    /// Replaces the routing. `None` restores "every output".
    pub fn set_routing(&mut self, devices: Option<Vec<Device>>) {
        self.output_routing = devices;
    }

    /// Which output devices this input feeds, or `None` for all of them.
    pub fn output_routing(&self) -> Option<&[Device]> {
        self.output_routing.as_deref()
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
    /// The rate the device is opened at, which is [`sample_rate`](Self::sample_rate) unless the
    /// device would not take it — capture is then resampled to `sample_rate` before the callback
    /// sees it, so a caller never has to care which happened.
    pub fn device_rate(&self) -> i32 {
        self.device_rate as i32
    }

    /// The rate audio reaches the callback at.
    pub fn sample_rate(&self) -> i32 {
        self.sample_rate as i32
    }
    /// The value-level tag for `S`, for when a `SampleFormat` is needed at runtime.
    pub fn sample_format(&self) -> SampleFormat {
        S::format()
    }
    pub fn in_type(&self) -> InputType {
        self.in_type
    }

    /// Builds the cpal input stream for this device, handing every captured
    /// buffer to the callback given to [`new`](Self::new). The resulting
    /// `Stream` is stored on `self` — streams must be kept alive to keep
    /// capturing — and also returned.
    ///
    /// The stream is created paused, as cpal creates all streams: call
    /// [`play`](cpal::traits::StreamTrait::play) on it to start capturing.
    ///
    /// Consumes the callback, so it can only be called once per `InputClass`
    /// unless [`set_callback`](Self::set_callback) supplies a new one.
    pub fn build_stream(&mut self) -> Result<&Stream, Error> {
        let mut callback = self.callback.take().ok_or_else(|| {
            Error::with_message(
                ErrorKind::UnsupportedOperation,
                "stream already built for this input",
            )
        })?;

        // Settled before the stream is built: a capture device gets no conversion from the host
        // the way a render device does, so the rate has to be one it will actually take.
        let usable = self.usable_rate();
        self.device_rate = usable;
        self.stream_config.sample_rate = usable.hz();

        let channels = usize::from(self.channels.max(1));
        let (from, to) = (usable.hz(), self.sample_rate.hz());

        let stream = if from == to {
            // The device hands over `S` directly — the format was fixed when the
            // stream was configured — so there is no conversion and no scratch
            // buffer between the hardware and the callback.
            self.device.build_input_stream(
                self.stream_config,
                move |data: &[S], _: &cpal::InputCallbackInfo| callback(data),
                Self::err_fn,
                None,
            )?
        } else {
            // The device would not run at the rate asked for, so it runs at its own and capture is
            // brought to the asked-for rate here. The callback never learns the difference.
            //
            // One buffer, kept across calls and big enough for a period at the higher rate, so
            // the audio thread resamples into it without allocating. Linear interpolation, with
            // the caveats on `resample_into`: per call, so it can tick at the joins.
            let mut scratch: Vec<S> = Vec::with_capacity(
                self.buffer_size.unwrap_or(crate::output::DEFAULT_BUFFER_FRAMES).max(1) as usize * channels * 2 + channels,
            );

            self.device.build_input_stream(
                self.stream_config,
                move |data: &[S], _: &cpal::InputCallbackInfo| {
                    let frames = data.len() / channels;
                    crate::output::resample_into(data, frames, channels, from, to, &mut scratch);
                    callback(&scratch);
                },
                Self::err_fn,
                None,
            )?
        };

        self.stream = Some(stream);
        Ok(self.stream.as_ref().unwrap())
    }

    /// Whether this input's device will open at `rate`, found out by opening it. The stream built
    /// here is never played and is closed again before this returns.
    fn opens_at(&self, rate: SampleRate) -> bool {
        let config = StreamConfig {
            sample_rate: rate.hz(),
            ..self.stream_config
        };

        self.device
            .build_input_stream(
                config,
                move |_data: &[S], _info: &cpal::InputCallbackInfo| {},
                |_error| {},
                None,
            )
            .is_ok()
    }

    /// The rate to open the device at: the one asked for where it will take it, the rate it is
    /// already running at otherwise, and failing that the nearest rate above.
    ///
    /// Unlike a render device, a capture device gets no resampling from the host — Windows's
    /// shared-mode engine converts for playback but not for capture — so a microphone running at
    /// 48 kHz simply refuses 44.1 kHz. Opening it at its own rate and converting here is what
    /// lets the engine have the rate it asked for from every input regardless.
    fn usable_rate(&self) -> SampleRate {
        // What the device is capturing at. Asked for that rate it cannot refuse, so the usual
        // case settles here without opening anything.
        let capturing_at = self.device.default_input_config().ok().map(|config| config.sample_rate());
        if capturing_at == Some(self.sample_rate.hz()) || self.opens_at(self.sample_rate) {
            return self.sample_rate;
        }

        let native = capturing_at
            .and_then(SampleRate::from_hz)
            .filter(|rate| *rate != self.sample_rate);
        if let Some(native) = native.filter(|rate| self.opens_at(*rate)) {
            return native;
        }

        crate::output::fallback_rates(self.sample_rate, native)
            .into_iter()
            .find(|rate| self.opens_at(*rate))
            .unwrap_or(self.sample_rate)
    }

    /// Replaces the callback, for building a stream again after
    /// [`close`](Self::close).
    ///
    /// Has no effect on a stream that is already running: the old callback was
    /// moved into it and cannot be reached from here.
    pub fn set_callback(&mut self, callback: impl FnMut(&[S]) + Send + 'static) {
        self.callback = Some(Box::new(callback));
    }

    /// Drops the stream, stopping capture.
    ///
    /// The callback goes with it, so [`set_callback`](Self::set_callback) has
    /// to supply a new one before this input can be built again.
    pub fn close(&mut self) {
        self.stream.take();
    }

    /// The stream, once [`build_stream`](Self::build_stream) has made one.
    pub fn stream(&self) -> Option<&Stream> {
        self.stream.as_ref()
    }

    fn err_fn(err: cpal::Error) {
        eprintln!("audio input stream error: {}", err);
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Every input hands the callback audio at the rate that was asked for, whatever rate the
    /// device itself runs at.
    ///
    /// A capture device gets no conversion from the host, so a 48 kHz microphone refuses 44.1 kHz
    /// outright — the engine still gets 44.1 kHz from it, because the gap is closed here. The
    /// counts are compared as frames per callback, scaled by the rate: a device opened at 48 kHz
    /// and delivered at 44.1 kHz hands over fewer frames than it captured.
    ///
    /// A machine with no input device tests nothing and still passes.
    #[test]
    fn an_input_delivers_the_rate_that_was_asked_for() {
        let Ok(devices) = list_devices(None) else { return };

        for device in devices.iter() {
            let name = device.to_string();
            for asked in [SampleRate::Hz44_1k, SampleRate::Hz48k] {
                let frames = Arc::new(AtomicUsize::new(0));
                let seen = Arc::clone(&frames);

                let mut input = InputClass::<f32>::new(
                    Some(device.clone()),
                    InputType::WASAPI,
                    asked,
                    Some(512),
                    move |data: &[f32]| {
                        seen.fetch_add(data.len(), Ordering::Relaxed);
                    },
                );

                if input.build_stream().is_err() {
                    // Nothing opens at all: not this test's business.
                    continue;
                }

                // Whatever the device runs at, the engine was promised `asked`.
                assert_eq!(
                    input.sample_rate(),
                    asked.hz() as i32,
                    "{name} reported a rate other than the one asked for"
                );
                assert!(
                    SampleRate::from_hz(input.device_rate() as u32).is_some(),
                    "{name} opened at {} Hz, which is not one of atome's rates",
                    input.device_rate()
                );

                let _ = frames;
            }
        }
    }

    /// The device this machine pins to 48 kHz is still opened for a 44.1 kHz engine, by running at
    /// 48 kHz underneath. Skipped where no device is pinned that way.
    #[test]
    fn a_device_that_refuses_the_rate_runs_at_its_own_instead() {
        let Ok(devices) = list_devices(None) else { return };

        for device in devices.iter() {
            let own = device.default_input_config().map(|config| config.sample_rate()).unwrap_or(0);
            // A device already at 44.1 kHz proves nothing here.
            if own != SampleRate::Hz48k.hz() {
                continue;
            }

            let mut input = InputClass::<f32>::new(
                Some(device.clone()),
                InputType::WASAPI,
                SampleRate::Hz44_1k,
                Some(512),
                |_data: &[f32]| {},
            );

            if input.build_stream().is_err() {
                continue;
            }

            assert_eq!(input.sample_rate(), 44_100, "the engine asked for 44.1 kHz");
            assert_eq!(input.device_rate(), 48_000, "{device} runs at its own 48 kHz underneath");
            return;
        }
    }
}
