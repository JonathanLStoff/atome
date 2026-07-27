//! A real‑time audio engine using atomic‑queue (MPMC) and ringbuf (SPSC).
//!
//! The engine runs a background mixer thread that collects audio chunks from
//! multiple producers, sums them, and writes the mixed result into a ring buffer.
//! The CPAL audio callback reads from that ring buffer and sends it to the sound card.

use atomic_queue::Queue;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    BufferSize, Stream, StreamConfig,
};
use ringbuf::{HeapRb, Rb};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub mod output;



// TODO: Gain control
// TODO: Plugin flow
// TODO: 

/// The main audio engine. Producers can push audio chunks via `add_samples`.
pub struct AudioEngine {
    /// The CPAL audio stream – must be kept alive.
    _stream: Option<Stream>,
    /// Shared MPMC queue for incoming audio chunks.
    queue: Arc<Queue<Vec<f32>>>,
    /// Final SPSC ring buffer read by the audio callback.
    output_buffer: Arc<HeapRb<f32>>,
    /// Number of frames per chunk (e.g., 512).
    buffer_size: usize,
    /// This is not used here, it is used as part of output object
    // channels: usize,
}

impl AudioEngine {
    /// Creates a new audio engine with the given **buffer size (frames)**.
    ///
    /// The engine uses the default output device and assumes 48 kHz, stereo, f32.
    /// Panics if audio device initialisation fails.
    pub fn new(buffer_size_frames: usize) -> Self {
        // 1. Setup CPAL device & config
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .expect("no output device available");
        let config = device.default_output_config().unwrap();
        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;

        // We'll set the stream's buffer size to match our requested size.
        let stream_config = StreamConfig {
            channels: channels as u16,
            sample_rate: cpal::SampleRate(sample_rate),
            buffer_size: BufferSize::Fixed(buffer_size_frames as u32),
        };

        // 2. Create the MPMC queue (unbounded capacity, but we can limit it)
        //    The queue holds entire chunks (Vec<f32>). We cap it to avoid unbounded growth.
        let queue_capacity = 1024; // max number of pending chunks
        let queue = Arc::new(Queue::<Vec<f32>>::with_capacity(queue_capacity));

        // 3. Create the final SPSC ring buffer. We give it enough room for ~2 seconds.
        let ring_capacity = (sample_rate * 2 * channels) as usize;
        let output_buffer = Arc::new(HeapRb::<f32>::new(ring_capacity));
        let output_buffer_clone = Arc::clone(&output_buffer);

        // 4. Spawn the mixer thread
        let queue_clone = Arc::clone(&queue);
        let buffer_size = buffer_size_frames;
        let channels_clone = channels;
        thread::Builder::new()
            .name("audio_mixer".to_string())
            .spawn(move || {
                // Allocate a working buffer once
                let mut mixed = vec![0.0f32; buffer_size * channels_clone];

                loop {
                    // Collect all pending chunks from the queue (non‑blocking).
                    // We pop until the queue is empty.
                    let mut chunks = Vec::new();
                    while let Some(chunk) = queue_clone.pop() {
                        chunks.push(chunk);
                    }

                    // Mix (sum) all chunks into `mixed`.
                    if chunks.is_empty() {
                        // No audio – write silence.
                        mixed.fill(0.0);
                    } else {
                        mixed.fill(0.0);
                        for chunk in chunks {
                            // Ensure the chunk has the expected length.
                            // If not, we pad or truncate (here we truncate).
                            let len = chunk.len().min(mixed.len());
                            for i in 0..len {
                                mixed[i] += chunk[i];
                            }
                        }
                    }

                    // Write the mixed block to the output ring buffer.
                    // If the buffer is full, we drop the block (avoid blocking).
                    let _ = output_buffer_clone.push_slice(&mixed);

                    // Sleep a tiny bit to avoid burning CPU.
                    // The sleep duration should be < buffer_size / sample_rate
                    // to keep the buffer filled. Here we use 1 ms.
                    thread::sleep(Duration::from_micros(1000));
                }
            })
            .expect("failed to spawn mixer thread");

        // 5. Build the CPAL stream with our callback.
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => {
                let out_buf = Arc::clone(&output_buffer);
                device
                    .build_output_stream(
                        &stream_config,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            // Read as many samples as possible from the ring buffer.
                            let read = out_buf.pop_slice(data);
                            // Fill the rest with silence.
                            if read < data.len() {
                                data[read..].fill(0.0);
                            }
                        },
                        |err| eprintln!("audio stream error: {}", err),
                        None,
                    )
                    .expect("failed to build output stream")
            }
            _ => panic!("only f32 sample format is supported"),
        };

        stream.play().expect("failed to play stream");

        AudioEngine {
            _stream: Some(stream),
            queue,
            output_buffer,
            buffer_size: buffer_size_frames,
            channels,
        }
    }

    /// Push a new audio chunk to be mixed and played.
    ///
    /// The chunk **must** contain exactly `buffer_size * channels` samples
    /// in interleaved format (L,R,L,R,…). If the queue is full, the chunk is
    /// dropped and `Err` is returned.
    pub fn add_samples(&self, samples: Vec<f32>) -> Result<(), &'static str> {
        // Validate length
        let expected = self.buffer_size * self.channels;
        if samples.len() != expected {
            return Err("chunk length must equal buffer_size * channels");
        }
        // Try to push; if the queue is full, the push returns `false`.
        if self.queue.push(samples) {
            Ok(())
        } else {
            Err("audio queue is full – chunk dropped")
        }
    }

    /// Returns the number of samples currently waiting in the output buffer.
    /// Useful for monitoring.
    pub fn output_buffer_len(&self) -> usize {
        self.output_buffer.len()
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        // Dropping the stream stops the audio callback.
        // The mixer thread will exit when the `queue` is dropped
        // (since we hold the last Arc).
        if let Some(stream) = self._stream.take() {
            drop(stream);
        }
        println!("AudioEngine stopped.");
    }
}