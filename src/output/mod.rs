use cpal::traits::DeviceTrait;
use cpal::{Device, Error, ErrorKind, SampleFormat, Stream, StreamConfig};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

pub mod types;

pub use types::{OutputType, SampleRate, SampleType};

/// # OutputClass
/// 
/// Used to hold a stream to a single output

pub struct OutputClass {
    // Number of channels in this output
    channels: i32,
    // String name of device
    name: String,
    // Type of device
    out_type: OutputType,
    // The buffer size, one for all outputs
    buffer_size: i32,
    // The sample rate
    sample_rate: SampleRate,
    // The sample type options: [i8, i16, i24, i32, i64, u8, u16, u24, u32, u64, F32, F64, DsdU8, DsdU16, DsdU32]
    sample_format: SampleFormat,
    // stream for cpal
    stream: Option<Stream>,
    // Producer half of the ring buffer: `add_samples` writes here
    producer: HeapProd<f32>,
    // Consumer half: taken by `build_stream` and moved into the audio callback
    consumer: Option<HeapCons<f32>>,
}

impl OutputClass {
    pub fn new(name: String, out_type: OutputType, channels: i32, sample_rate:SampleRate, sample_format: SampleFormat, buffer_size: i32) -> Self {
        // Room for a few callback periods so a late producer does not underrun.
        let capacity = buffer_size.max(1) as usize * channels.max(1) as usize * 4;
        let (producer, consumer) = HeapRb::<f32>::new(capacity).split();

        OutputClass {
            channels,
            name,
            out_type,
            buffer_size,
            sample_rate,
            sample_format,
            stream: None,
            producer,
            consumer: Some(consumer),
        }
    }

    pub fn channels(&self) -> i32 {
        self.channels
    }
    pub fn name(&self) -> String {
        self.name.clone()
    }
    pub fn buffer_size(&self) -> i32 {
        self.buffer_size
    }
    pub fn sample_rate(&self) -> i32 {
        self.sample_rate as i32
    }
    pub fn sample_format(&self) -> SampleFormat {
        self.sample_format
    }
    pub fn out_type(&self) -> OutputType {
        self.out_type
    }

    /// Builds the cpal output stream for `device`, pulling mixed samples out of
    /// this output's ring buffer and writing them in whatever sample format the
    /// device requires. The resulting `Stream` is stored on `self` (streams must
    /// be kept alive to keep playing) and also returned.
    ///
    /// Consumes the consumer half of the ring buffer, so it can only be called
    /// once per `OutputClass`.
    pub fn build_stream(
        &mut self,
        device: &Device,
        sample_format: SampleFormat,
    ) -> Result<&Stream, Error> {
        let buffer = self.consumer.take().ok_or_else(|| {
            Error::with_message(
                ErrorKind::UnsupportedOperation,
                "stream already built for this output",
            )
        })?;

        let stream_config = StreamConfig {
            channels: self.channels as u16,
            sample_rate: device.default_output_config()?.sample_rate(),
            buffer_size: cpal::BufferSize::Fixed(self.buffer_size as u32),
        };

        let stream = match sample_format {
            SampleFormat::I8 => Self::typed_stream::<i8>(device, stream_config, buffer)?,
            SampleFormat::I16 => Self::typed_stream::<i16>(device, stream_config, buffer)?,
            SampleFormat::I24 => Self::typed_stream::<cpal::I24>(device, stream_config, buffer)?,
            SampleFormat::I32 => Self::typed_stream::<i32>(device, stream_config, buffer)?,
            SampleFormat::I64 => Self::typed_stream::<i64>(device, stream_config, buffer)?,
            SampleFormat::U8 => Self::typed_stream::<u8>(device, stream_config, buffer)?,
            SampleFormat::U16 => Self::typed_stream::<u16>(device, stream_config, buffer)?,
            SampleFormat::U24 => Self::typed_stream::<cpal::U24>(device, stream_config, buffer)?,
            SampleFormat::U32 => Self::typed_stream::<u32>(device, stream_config, buffer)?,
            SampleFormat::U64 => Self::typed_stream::<u64>(device, stream_config, buffer)?,
            SampleFormat::F64 => Self::typed_stream::<f64>(device, stream_config, buffer)?,
            // f32 is the engine's own format, so the callback can read the ring
            // buffer straight into the device buffer with no scratch copy.
            SampleFormat::F32 => {
                let mut buffer = buffer;
                device.build_output_stream(
                    stream_config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        let read = buffer.pop_slice(data);
                        if read < data.len() {
                            data[read..].fill(0.0);
                        }
                    },
                    Self::err_fn,
                    None,
                )?
            }
            // DSD is a 1-bit bitstream, not PCM — nothing to convert f32 into.
            SampleFormat::DsdU8 | SampleFormat::DsdU16 | SampleFormat::DsdU32 => {
                return Err(Error::with_message(
                    ErrorKind::UnsupportedConfig,
                    "DSD output is not supported",
                ))
            }
            _ => return Err(Error::new(ErrorKind::UnsupportedConfig)),
        };

        self.stream = Some(stream);
        Ok(self.stream.as_ref().unwrap())
    }

    /// One output callback, generic over the device's sample type: read `f32`
    /// from the ring buffer, convert into `S` on the way out.
    fn typed_stream<S: SampleType>(
        device: &Device,
        config: StreamConfig,
        mut buffer: HeapCons<f32>,
    ) -> Result<Stream, Error> {
        let mut scratch = Vec::<f32>::new();
        device.build_output_stream(
            config,
            move |data: &mut [S], _: &cpal::OutputCallbackInfo| {
                if scratch.len() < data.len() {
                    scratch.resize(data.len(), 0.0);
                }
                let scratch = &mut scratch[..data.len()];
                let read = buffer.pop_slice(scratch);
                if read < scratch.len() {
                    scratch[read..].fill(0.0);
                }
                for (dst, sample) in data.iter_mut().zip(scratch.iter()) {
                    *dst = S::from_f32(sample.clamp(-1.0, 1.0));
                }
            },
            Self::err_fn,
            None,
        )
    }

    /// Queues interleaved samples for playback. `S` is any real sample type, so
    /// `&[f32]`, `&[i16]`, `&[u8]`, ... all work directly with no conversion by
    /// the caller. Returns how many samples were accepted — a short count means
    /// the ring buffer was full and the rest were dropped.
    pub fn add_samples<S: SampleType>(&mut self, samples: &[S]) -> usize {
        samples
            .iter()
            .take_while(|sample| self.producer.try_push(sample.to_f32()).is_ok())
            .count()
    }

    fn err_fn(err: cpal::Error) {
        eprintln!("audio output stream error: {}", err);
    }
}
