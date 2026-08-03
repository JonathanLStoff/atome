//! A device plus everything atome needs to know about it.
//!
//! A [`cpal::Device`] says what hardware exists. An [`AtomeDevice`] says what
//! this engine should do with it: which host it came through, which plugins
//! process its audio, and — for an input — where that audio is sent.
//!
//! ```no_run
//! use atome::device::AtomeDevice;
//! use atome::output::OutputType;
//!
//! // A microphone whose audio goes only to the monitors, through one plugin.
//! let mic = AtomeDevice::default_input(OutputType::CoreAudio)
//!     .expect("no input device")
//!     .route_to(["Monitors"]);
//!
//! let monitors = AtomeDevice::default_output(OutputType::CoreAudio)
//!     .expect("no output device");
//! ```

use cpal::Device;

use crate::input;
use crate::output::{self, device_name, OutputType};
use crate::plugins::Plugin;

/// Whether a device is captured from or played to.
///
/// Held on the device rather than inferred from where it is passed, so handing
/// an output to [`AudioEngine`](crate::AudioEngine)'s input list is caught at
/// construction instead of failing later, when the wrong kind of cpal stream is
/// built.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Input,
    Output,
}

/// A device, the host it is reached through, and what the engine does with it.
#[derive(Clone)]
pub struct AtomeDevice {
    device: Device,
    direction: Direction,
    host: OutputType,
    plugins: Vec<Plugin>,
    routing: Option<Vec<String>>,
}

impl AtomeDevice {
    /// A capture device.
    pub fn input(device: Device, host: OutputType) -> Self {
        AtomeDevice::new(device, Direction::Input, host)
    }

    /// A playback device.
    pub fn output(device: Device, host: OutputType) -> Self {
        AtomeDevice::new(device, Direction::Output, host)
    }

    /// The system's default capture device, if it has one.
    pub fn default_input(host: OutputType) -> Option<Self> {
        input::default_device().map(|device| AtomeDevice::input(device, host))
    }

    /// The system's default playback device, if it has one.
    pub fn default_output(host: OutputType) -> Option<Self> {
        output::default_device().map(|device| AtomeDevice::output(device, host))
    }

    fn new(device: Device, direction: Direction, host: OutputType) -> Self {
        AtomeDevice {
            device,
            direction,
            host,
            plugins: Vec::new(),
            routing: None,
        }
    }

    /// Attaches a plugin to this device alone.
    ///
    /// On an input it processes that input's audio before routing; on an
    /// output, only what that device plays. Order is the order added.
    pub fn with_plugin(mut self, plugin: Plugin) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Attaches several plugins, in order.
    pub fn with_plugins(mut self, plugins: impl IntoIterator<Item = Plugin>) -> Self {
        self.plugins.extend(plugins);
        self
    }

    /// Sends this input's audio only to the named output devices.
    ///
    /// Without this an input feeds every output. Names are matched against
    /// [`name`](Self::name), and a name matching no output fails when the
    /// engine is built rather than going silently nowhere at runtime.
    ///
    /// Meaningless on an output, where it is ignored.
    pub fn route_to(mut self, outputs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.routing = Some(outputs.into_iter().map(Into::into).collect());
        self
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn host(&self) -> OutputType {
        self.host
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    pub fn plugins(&self) -> &[Plugin] {
        &self.plugins
    }

    pub fn plugins_mut(&mut self) -> &mut Vec<Plugin> {
        &mut self.plugins
    }

    /// Which outputs this input feeds, or `None` for all of them.
    pub fn routing(&self) -> Option<&[String]> {
        self.routing.as_deref()
    }

    /// The device's name, as the routing list matches it.
    pub fn name(&self) -> String {
        device_name(&self.device)
    }
}

impl std::fmt::Debug for AtomeDevice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `cpal::Device` has no `Debug`, and its name is the useful part anyway.
        formatter
            .debug_struct("AtomeDevice")
            .field("name", &self.name())
            .field("direction", &self.direction)
            .field("host", &self.host)
            .field("plugins", &self.plugins.len())
            .field("routing", &self.routing)
            .finish()
    }
}
