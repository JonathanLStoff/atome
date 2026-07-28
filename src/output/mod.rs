use cpal::traits::DeviceTrait;
use cpal::{Data, Device, Error, ErrorKind, SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};


pub mod types;
pub mod utils;

pub use types::{OutputType, SampleRate, SampleType};
pub use utils::{default_device, find_device, list_device_names, list_devices, device_name};

/// Frames assumed when the caller asks for the device's default buffer size.
/// Only used to size the ring buffer — the stream itself still asks the device
/// for its own preferred size.
const DEFAULT_BUFFER_FRAMES: i32 = 1024;

/// # OutputClass
///
/// Used to hold a stream to a single output

pub struct OutputClass {
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
    // The runtime tag for the sample type: [i8, i16, i24, i32, i64, u8, u16, u24, u32, u64, F32, F64, DsdU8, DsdU16, DsdU32].
    // `SampleType` is a trait, so it can only bound a generic parameter, never
    // be a field; `SampleFormat` is its value-level counterpart, and
    // `S::format()` maps one to the other.
    sample_type: SampleFormat,
    // stream for cpal
    stream: Option<Stream>,
    // Config the stream is (or will be) built with
    stream_config: StreamConfig,
    // Producer half of the ring buffer: `add_samples` writes here
    producer: HeapProd<f32>,
    // Consumer half: taken by `build_stream` and moved into the audio callback
    consumer: Option<HeapCons<f32>>,
}

impl OutputClass {
    pub fn new(device: Option<Device>, out_type: OutputType, channels: u16, sample_rate:SampleRate, sample_type: SampleFormat, buffer_size: Option<i32>) -> Self {
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

        // Room for a few callback periods so a late producer does not underrun.
        let capacity = frames * channels.max(1) as usize * 4;
        let (producer, consumer) = HeapRb::<f32>::new(capacity).split();

        OutputClass {
            channels,
            device: device
                .unwrap_or_else(|| default_device().expect("no default output device available")),
            out_type,
            buffer_size,
            sample_rate,
            sample_type,
            stream: None,
            stream_config,
            producer,
            consumer: Some(consumer),
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
    pub fn sample_format(&self) -> SampleFormat {
        self.sample_type
    }

    pub fn sample_type(&self) -> SampleFormat {
        self.sample_type
    }
    pub fn out_type(&self) -> OutputType {
        self.out_type
    }
    /// The body of the output callback, generic over the device's sample type.
    ///
    /// Pops one callback's worth of interleaved samples out of the ring buffer
    /// and hands them to the host as `S`. `add_samples` already writes frames
    /// interleaved (L,R,L,R,…), so popping a contiguous run preserves that
    /// layout — the samples go out in exactly the order they came in.
    ///
    /// `scratch` is owned by the caller and reused across calls so the callback
    /// never allocates on the audio thread.
    fn data_callback<S: SampleType>(
        data: &mut Data,
        buffer: &mut HeapCons<f32>,
        scratch: &mut Vec<f32>,
    ) {
        let data = data
            .as_slice_mut::<S>()
            .expect("host supplied a buffer of a different sample type");

        // The ring buffer holds f32, so pop into scratch and convert on the way
        // out. `resize` only ever allocates on the first few callbacks.
        if scratch.len() < data.len() {
            scratch.resize(data.len(), 0.0);
        }
        let frames = &mut scratch[..data.len()];

        let read = buffer.pop_slice(frames);
        if read < frames.len() {
            // Underrun: pad with silence rather than replaying stale audio.
            frames[read..].fill(0.0);
        }

        for (dst, sample) in data.iter_mut().zip(frames.iter()) {
            *dst = S::from_f32(sample.clamp(-1.0, 1.0));
        }
    }

    /// Builds the cpal output stream for this output's device, pulling mixed
    /// samples out of its ring buffer and writing them in whatever sample format
    /// the device requires. The resulting `Stream` is stored on `self` (streams
    /// must be kept alive to keep playing) and also returned.
    ///
    /// Consumes the consumer half of the ring buffer, so it can only be called
    /// once per `OutputClass`.
    pub fn build_stream(&mut self) -> Result<&Stream, Error> {
        // Checked before taking the consumer, so a rejected format leaves the
        // output still buildable.
        if matches!(
            self.sample_type,
            SampleFormat::DsdU8 | SampleFormat::DsdU16 | SampleFormat::DsdU32
        ) {
            // DSD is a 1-bit bitstream, not PCM — nothing to convert f32 into.
            return Err(Error::with_message(
                ErrorKind::UnsupportedConfig,
                "DSD output is not supported",
            ));
        }

        let mut buffer = self.consumer.take().ok_or_else(|| {
            Error::with_message(
                ErrorKind::UnsupportedOperation,
                "stream already built for this output",
            )
        })?;

        let sample_type = self.sample_type;
        let mut scratch = Vec::<f32>::new();

        // The raw API hands the callback an untyped `Data`, so the sample type
        // is only known at runtime: dispatch once per callback to the right
        // instantiation of `data_callback`.
        let stream = self.device.build_output_stream_raw(
            self.stream_config,
            sample_type,
            move |data: &mut Data, _: &cpal::OutputCallbackInfo| match sample_type {
                SampleFormat::I8 => Self::data_callback::<i8>(data, &mut buffer, &mut scratch),
                SampleFormat::I16 => Self::data_callback::<i16>(data, &mut buffer, &mut scratch),
                SampleFormat::I24 => {
                    Self::data_callback::<cpal::I24>(data, &mut buffer, &mut scratch)
                }
                SampleFormat::I32 => Self::data_callback::<i32>(data, &mut buffer, &mut scratch),
                SampleFormat::I64 => Self::data_callback::<i64>(data, &mut buffer, &mut scratch),
                SampleFormat::U8 => Self::data_callback::<u8>(data, &mut buffer, &mut scratch),
                SampleFormat::U16 => Self::data_callback::<u16>(data, &mut buffer, &mut scratch),
                SampleFormat::U24 => {
                    Self::data_callback::<cpal::U24>(data, &mut buffer, &mut scratch)
                }
                SampleFormat::U32 => Self::data_callback::<u32>(data, &mut buffer, &mut scratch),
                SampleFormat::U64 => Self::data_callback::<u64>(data, &mut buffer, &mut scratch),
                SampleFormat::F32 => Self::data_callback::<f32>(data, &mut buffer, &mut scratch),
                SampleFormat::F64 => Self::data_callback::<f64>(data, &mut buffer, &mut scratch),
                // Rejected in `build_stream`, so unreachable in practice.
                _ => {}
            },
            Self::err_fn,
            None,
        )?;

        self.stream = Some(stream);
        Ok(self.stream.as_ref().unwrap())
    }

    /// Queues interleaved samples for playback. `S` is any real sample type, so
    /// `&[f32]`, `&[i16]`, `&[u8]`, ... all work directly with no conversion by
    /// the caller. Returns how many samples were accepted — a short count means
    /// the ring buffer was full and the rest were dropped.
    /// `index` index to place the first sample in the ring buffer.
    pub fn add_samples<S: SampleType>(&mut self, samples: &[S], index: usize) -> usize {
        // First check that the selected sample type is the same as the one we're using
        assert_eq!(S::format(), self.sample_type);

        let mut index = index;
        for sample in samples {
            // A full ring buffer means the callback hasn't drained yet; drop the
            // rest rather than panicking on the producer side.
            if self.producer.try_push(sample.to_f32()).is_err() {
                break;
            }
            index += 1;
        }
        index
    }

    fn err_fn(err: cpal::Error) {
        eprintln!("audio output stream error: {}", err);
    }
}
