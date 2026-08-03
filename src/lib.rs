//! A real-time audio engine over cpal.
//!
//! [`AudioEngine`] owns every stream and decides what reaches which device. It
//! is given a list of inputs and a list of outputs, both described as
//! [`AtomeDevice`]s, and builds an [`InputClass`] or [`OutputClass`] for each.
//!
//! # Routing
//!
//! An input with no routing feeds every output. An input that names outputs
//! feeds only those, so a talkback microphone can reach the monitors without
//! also reaching the main mix.
//!
//! # Plugins
//!
//! A [`Plugin`] attaches at one of three levels, and where it attaches is what
//! decides how much audio it hears:
//!
//! | Attached to | Hears |
//! |---|---|
//! | An input's `AtomeDevice` | that input alone, before routing |
//! | An output's `AtomeDevice` | what that device plays, after mixing |
//! | [`AudioEngine::new`] directly | everything |
//!
//! ```no_run
//! use atome::{device::AtomeDevice, AudioEngine};
//! use atome::output::{OutputType, SampleRate};
//!
//! let mic = AtomeDevice::default_input(OutputType::CoreAudio)
//!     .expect("no input device");
//! let speakers = AtomeDevice::default_output(OutputType::CoreAudio)
//!     .expect("no output device");
//!
//! let engine = AudioEngine::<f32>::new(
//!     vec![mic],
//!     vec![speakers],
//!     SampleRate::Hz48k,
//!     vec![2],
//!     Some(512),
//!     vec![],
//! )?;
//! # Ok::<(), cpal::Error>(())
//! ```

use cpal::{Error, ErrorKind};

pub mod device;
pub mod import;
pub mod input;
pub mod output;
pub mod plugins;

pub use device::{AtomeDevice, Direction};
pub use input::InputClass;
pub use output::{OutputClass, SampleRate, SampleType};
pub use plugins::Plugin;

/// An input, its stream, and the outputs it feeds.
pub struct EngineInput<S: SampleType> {
    device: AtomeDevice,
    input: InputClass<S>,
    /// Indices into [`AudioEngine::outputs`], resolved once at construction so
    /// the audio path never has to match a name against a list.
    routes: Vec<usize>,
}

impl<S: SampleType> EngineInput<S> {
    pub fn device(&self) -> &AtomeDevice {
        &self.device
    }

    pub fn input(&self) -> &InputClass<S> {
        &self.input
    }

    pub fn input_mut(&mut self) -> &mut InputClass<S> {
        &mut self.input
    }

    /// Which outputs this input feeds, by index.
    pub fn routes(&self) -> &[usize] {
        &self.routes
    }
}

/// An output and its stream.
pub struct EngineOutput<S: SampleType> {
    device: AtomeDevice,
    output: OutputClass<S>,
}

impl<S: SampleType> EngineOutput<S> {
    pub fn device(&self) -> &AtomeDevice {
        &self.device
    }

    pub fn output(&self) -> &OutputClass<S> {
        &self.output
    }

    pub fn output_mut(&mut self) -> &mut OutputClass<S> {
        &mut self.output
    }
}

/// The main audio engine: every stream, and the routing between them.
pub struct AudioEngine<S: SampleType> {
    inputs: Vec<EngineInput<S>>,
    outputs: Vec<EngineOutput<S>>,
    sample_rate: SampleRate,
    buffer_size: Option<i32>,
    /// Applied to everything, whichever device it came from or goes to.
    plugins: Vec<Plugin>,
}

impl<S: SampleType> AudioEngine<S> {
    /// Builds an engine over the given devices.
    ///
    /// `output_channels` gives one channel count per output device, in the same
    /// order — `[2, 2, 5]` for two stereo pairs and a five-channel rig. It is a
    /// list rather than one number because devices on one engine genuinely
    /// differ, and pairing them off by position is checked rather than assumed:
    /// a list of the wrong length is an error, not a silent truncation.
    ///
    /// Inputs take their channel count from the hardware instead, since a
    /// capture device gives what it has.
    ///
    /// Nothing is started here. Streams are built but paused, exactly as cpal
    /// leaves them.
    ///
    /// # Errors
    ///
    /// - A device in `inputs` that is not an input, or in `outputs` that is not
    ///   an output
    /// - `output_channels` not the same length as `outputs`
    /// - An input routed to a name that matches no output
    pub fn new(
        inputs: Vec<AtomeDevice>,
        outputs: Vec<AtomeDevice>,
        sample_rate: SampleRate,
        output_channels: Vec<u16>,
        buffer_size: Option<i32>,
        plugins: Vec<Plugin>,
    ) -> Result<Self, Error> {
        if output_channels.len() != outputs.len() {
            return Err(Error::with_message(
                ErrorKind::InvalidInput,
                format!(
                    "{} output devices but {} channel counts",
                    outputs.len(),
                    output_channels.len()
                ),
            ));
        }

        for device in &outputs {
            if device.direction() != Direction::Output {
                return Err(Error::with_message(
                    ErrorKind::InvalidInput,
                    format!("{} is an input device, listed as an output", device.name()),
                ));
            }
        }

        for device in &inputs {
            if device.direction() != Direction::Input {
                return Err(Error::with_message(
                    ErrorKind::InvalidInput,
                    format!("{} is an output device, listed as an input", device.name()),
                ));
            }
        }

        // Outputs first: an input's routing names them, so they have to exist
        // before it can be resolved.
        let built_outputs: Vec<EngineOutput<S>> = outputs
            .into_iter()
            .zip(output_channels)
            .map(|(device, channels)| {
                let output = OutputClass::new(
                    Some(device.device().clone()),
                    device.host(),
                    channels,
                    sample_rate,
                    buffer_size,
                );

                EngineOutput { device, output }
            })
            .collect();

        let names: Vec<String> = built_outputs
            .iter()
            .map(|output| output.device.name())
            .collect();

        let built_inputs = inputs
            .into_iter()
            .map(|device| {
                let routes = resolve_routes(&device, &names)?;

                // The callback is a placeholder: carrying captured audio to the
                // routed outputs is section 2.3's remaining work, and needs a
                // lock-free hand-off rather than anything that can be done from
                // inside the audio callback.
                let mut input = InputClass::new(
                    Some(device.device().clone()),
                    device.host(),
                    sample_rate,
                    buffer_size,
                    |_captured: &[S]| {},
                );

                // The same routing the indices above describe, as devices, so
                // an `InputClass` driven on its own knows where it is going
                // without asking the engine.
                if device.routing().is_some() {
                    let devices = routes
                        .iter()
                        .map(|index| built_outputs[*index].device.device().clone())
                        .collect();
                    input.set_routing(Some(devices));
                }

                Ok(EngineInput {
                    device,
                    input,
                    routes,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(AudioEngine {
            inputs: built_inputs,
            outputs: built_outputs,
            sample_rate,
            buffer_size,
            plugins,
        })
    }

    pub fn inputs(&self) -> &[EngineInput<S>] {
        &self.inputs
    }

    pub fn inputs_mut(&mut self) -> &mut [EngineInput<S>] {
        &mut self.inputs
    }

    pub fn outputs(&self) -> &[EngineOutput<S>] {
        &self.outputs
    }

    pub fn outputs_mut(&mut self) -> &mut [EngineOutput<S>] {
        &mut self.outputs
    }

    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    pub fn buffer_size(&self) -> Option<i32> {
        self.buffer_size
    }

    /// The plugins applied to everything.
    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    /// Applies the plugins attached to input `index`, in place.
    ///
    /// This is the input's own chain and nothing else — the engine-wide chain
    /// runs later, in [`apply_engine_plugins`](Self::apply_engine_plugins), and
    /// the destination's chain later still. Called before routing, so an input
    /// heard by several outputs is processed once rather than once per
    /// destination.
    ///
    /// Does nothing if that input has no plugins.
    pub fn apply_input_plugins(&mut self, index: usize, buffer: &mut [S]) -> Result<(), Error> {
        let input = self
            .inputs
            .get_mut(index)
            .ok_or_else(|| unknown(index, "input"))?;

        let channels = input.input.channels();
        for plugin in input.device.plugins_mut() {
            plugin.apply(buffer, channels)?;
        }

        Ok(())
    }

    /// Applies the plugins attached to output `index`, in place.
    ///
    /// The last chain to run, and the narrowest: it hears what this device is
    /// about to play and nothing that goes anywhere else.
    pub fn apply_output_plugins(&mut self, index: usize, buffer: &mut [S]) -> Result<(), Error> {
        let output = self
            .outputs
            .get_mut(index)
            .ok_or_else(|| unknown(index, "output"))?;

        let channels = output.output.channels();
        for plugin in output.device.plugins_mut() {
            plugin.apply(buffer, channels)?;
        }

        Ok(())
    }

    /// Applies the engine-wide plugins, in place.
    ///
    /// These were handed to [`new`](Self::new) directly rather than attached to
    /// a device, so they hear everything — every input, on its way to every
    /// output. `channels` says how `buffer` is laid out, since the engine's
    /// devices do not agree on one count.
    pub fn apply_engine_plugins(
        &mut self,
        buffer: &mut [S],
        channels: u16,
    ) -> Result<(), Error> {
        for plugin in &mut self.plugins {
            plugin.apply(buffer, channels)?;
        }

        Ok(())
    }

    /// Runs every chain that applies to audio captured on input `index` and
    /// bound for output `to`, in the order they belong in.
    ///
    /// The order is the point of having three levels: the input's own
    /// processing happens where the audio is still one source, the engine's in
    /// the middle, and the destination's last, when it is what that device will
    /// actually play.
    pub fn apply_plugins(
        &mut self,
        index: usize,
        to: usize,
        buffer: &mut [S],
        channels: u16,
    ) -> Result<(), Error> {
        self.apply_input_plugins(index, buffer)?;
        self.apply_engine_plugins(buffer, channels)?;
        self.apply_output_plugins(to, buffer)
    }
}

impl<S: SampleType> std::fmt::Debug for EngineInput<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineInput")
            .field("device", &self.device.name())
            .field("channels", &self.input.channels())
            .field("plugins", &self.device.plugins().len())
            .field("routes", &self.routes)
            .finish()
    }
}

impl<S: SampleType> std::fmt::Debug for EngineOutput<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EngineOutput")
            .field("device", &self.device.name())
            .field("channels", &self.output.channels())
            .field("plugins", &self.device.plugins().len())
            .finish()
    }
}

/// Reports the wiring rather than the streams: which devices, how many channels
/// each, and what routes where. None of the cpal types underneath have `Debug`,
/// and none of them would say anything useful if they did.
impl<S: SampleType> std::fmt::Debug for AudioEngine<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AudioEngine")
            .field("sample_rate", &self.sample_rate)
            .field("buffer_size", &self.buffer_size)
            .field("inputs", &self.inputs)
            .field("outputs", &self.outputs)
            .field("plugins", &self.plugins.len())
            .finish()
    }
}

/// Turns an input's routing names into indices into the output list.
///
/// Resolved once, here, so that the audio path is an index lookup rather than a
/// string comparison, and so a name that matches nothing is caught while there
/// is still somewhere sensible to report it.
fn resolve_routes(device: &AtomeDevice, outputs: &[String]) -> Result<Vec<usize>, Error> {
    let Some(routing) = device.routing() else {
        // No routing named: this input feeds everything.
        return Ok((0..outputs.len()).collect());
    };

    routing
        .iter()
        .map(|wanted| {
            outputs
                .iter()
                .position(|name| name == wanted)
                .ok_or_else(|| {
                    Error::with_message(
                        ErrorKind::InvalidInput,
                        format!(
                            "{} is routed to {wanted:?}, which is not one of the outputs",
                            device.name()
                        ),
                    )
                })
        })
        .collect()
}

fn unknown(index: usize, what: &str) -> Error {
    Error::with_message(
        ErrorKind::InvalidInput,
        format!("no {what} at index {index}"),
    )
}
