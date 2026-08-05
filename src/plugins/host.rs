//! What every hosted plugin format needs and none of them differ on.
//!
//! VST3 and AU are reached through the same `truce-rack-core` traits, so the
//! work of getting atome's audio in and out of one is the same either way:
//! pick a bus layout, activate, and per block turn an interleaved buffer of
//! whatever sample type the engine runs into the planar `f32` an
//! [`AudioBuffer`] wants — and back again.
//!
//! Only compiled when a hosting feature is on. [Internal
//! plugins](super::internal) do not come through here.

use truce_rack_core::{
    buffer::{AudioBuffer, BusRange},
    bus::BusLayout,
    error::{Error as CoreError, Result as CoreResult},
    events::EventList,
    info::ParameterFlags,
    plugin::{Plugin as CorePlugin, PluginCore, ProcessContext, ProcessStatus},
};

use super::params::{ParamError, ParamSpec, Params, Value};
use crate::output::SampleType;

/// A loaded plugin, activated and with the scratch space its blocks need.
///
/// The scratch is the point of this type. `AudioBuffer` is planar and takes
/// separate input and output channel arrays, while atome's chains are one
/// interleaved buffer processed in place — so a block needs somewhere to be
/// deinterleaved into and somewhere to be written back from. Allocating that
/// per block would put the allocator on the audio thread, so it is allocated
/// once here, at activation, and reused.
pub(crate) struct Hosted<P: PluginCore> {
    plugin: P,
    /// One buffer per channel, each `max_block` long.
    inputs: Vec<Vec<f32>>,
    /// Ditto, written by the plugin.
    outputs: Vec<Vec<f32>>,
    /// Flat channel ranges describing the single main bus, handed to
    /// `AudioBuffer::new` each block. Built once because they never change
    /// while the activation stands.
    bus: [BusRange; 1],
    /// Events the plugin emits. Drained (well, cleared) each block: atome has
    /// nowhere to route MIDI thru or parameter touches yet.
    outgoing: EventList,
    /// Always empty — atome does not feed plugins MIDI yet. Kept as a field so
    /// no allocation happens on the block path when that changes.
    incoming: EventList,
    sample_rate: f64,
    max_block: usize,
    channels: usize,
}

impl<P> Hosted<P>
where
    P: CorePlugin<f32> + PluginCore,
{
    /// Activates `plugin` for `channels` channels and prepares its scratch.
    ///
    /// # Errors
    ///
    /// Fails if the plugin declares no bus layout that carries `channels`
    /// channels in and the same number out, or if the plugin's own `activate`
    /// fails.
    pub(crate) fn new(
        mut plugin: P,
        channels: usize,
        sample_rate: f64,
        max_block: usize,
    ) -> CoreResult<Self> {
        // A zero-length block would make every per-block slice empty and the
        // plugin's `max_block_size` a lie. One frame is the smallest honest
        // answer.
        let max_block = max_block.max(1);

        let layout = layout_for(plugin.supported_layouts(), channels).ok_or_else(|| {
            CoreError::Format {
                format: "atome",
                code: 0,
                message: format!(
                    "{} declares no {channels}-channel in/out layout",
                    plugin.info().name
                ),
            }
        })?;

        plugin.activate(layout, sample_rate, max_block)?;

        Ok(Self {
            plugin,
            inputs: vec![vec![0.0; max_block]; channels],
            outputs: vec![vec![0.0; max_block]; channels],
            bus: [BusRange::new(0, channels)],
            outgoing: EventList::new(),
            incoming: EventList::new(),
            sample_rate,
            max_block,
            channels,
        })
    }

    // -------------------------------------------------------------------------
    //  Parameters
    // -------------------------------------------------------------------------

    /// The plugin's parameters, described in its own units.
    ///
    /// Skips the ones the plugin asked to hide and the read-only meters — the
    /// first are not the host's business and the second cannot be set, so
    /// listing either as settable would be a lie.
    pub(crate) fn param_schema(&self) -> Vec<ParamSpec> {
        (0..self.plugin.parameter_count())
            .filter_map(|index| self.plugin.parameter_info(index).ok())
            .filter(|info| !info.flags.intersects(ParameterFlags::HIDDEN | ParameterFlags::READ_ONLY))
            .map(|info| {
                // A one-step parameter is a switch however the format spells
                // it, and describing it as a toggle is what lets `true` be
                // written where the plugin wants a 1.
                let toggle = info.step_count == 1
                    || info.flags.contains(ParameterFlags::BYPASS);

                if toggle {
                    ParamSpec::toggle(&info.name, info.default != info.min, &info.unit)
                } else {
                    ParamSpec::number(
                        &info.name,
                        &info.unit,
                        info.min,
                        info.max,
                        info.default,
                        "",
                    )
                }
            })
            .collect()
    }

    /// Every parameter's current value, by name.
    pub(crate) fn params(&self) -> Params {
        (0..self.plugin.parameter_count())
            .filter_map(|index| {
                let info = self.plugin.parameter_info(index).ok()?;
                if info
                    .flags
                    .intersects(ParameterFlags::HIDDEN | ParameterFlags::READ_ONLY)
                {
                    return None;
                }
                let value = self.plugin.parameter_value(index).ok()?;
                Some((info.name, Value::Number(value)))
            })
            .collect()
    }

    /// Sets parameters by the names the plugin reports.
    ///
    /// Names rather than indices, because an index is not stable across plugin
    /// versions and is not something anyone would write in a configuration
    /// file. The lookup is a linear scan over the parameter list, which is
    /// fine: this is not on the audio path, and the alternative is a cache
    /// that has to be invalidated when the plugin reloads.
    ///
    /// Resolved in full before anything is written, so a set that names one
    /// parameter the plugin does not have changes none of them — the same
    /// all-or-nothing guarantee the built-ins give.
    ///
    /// What that cannot cover is the plugin refusing a write it already
    /// accepted the look of. Nothing here can undo a `set_parameter` that
    /// succeeded before a later one failed, so that case is reported and the
    /// earlier writes stand.
    ///
    /// # Errors
    ///
    /// [`ParamError::Unknown`] for a name this plugin does not have, listing
    /// the ones it does; [`ParamError::Range`] for a value outside what the
    /// parameter declares.
    pub(crate) fn set_params(&mut self, params: &Params) -> Result<(), ParamError> {
        if params.is_empty() {
            return Ok(());
        }

        let schema = self.param_schema();

        // Resolve and check everything first.
        let mut writes = Vec::with_capacity(params.len());
        for (key, value) in params.iter() {
            let Some((index, spec)) = self.find(key, &schema) else {
                return Err(ParamError::Unknown {
                    key: key.to_string(),
                    known: schema.into_iter().map(|spec| spec.name).collect(),
                });
            };
            writes.push((key.to_string(), index, spec.check(value)?));
        }

        for (key, index, number) in writes {
            self.plugin.set_parameter(index, number).map_err(|error| {
                ParamError::Unsupported {
                    message: format!("could not set '{key}': {error}"),
                }
            })?;
        }

        Ok(())
    }

    /// The index and spec of the parameter called `key`.
    ///
    /// Exact match first, then case-insensitively — plugin parameter names are
    /// display strings written for a UI ("Dry/Wet", "Attack Time"), and asking
    /// a caller to reproduce a vendor's capitalisation exactly is a poor trade
    /// against the small chance of two names differing only in case.
    fn find(&self, key: &str, schema: &[ParamSpec]) -> Option<(usize, ParamSpec)> {
        let position = schema
            .iter()
            .position(|spec| spec.name == key)
            .or_else(|| {
                schema
                    .iter()
                    .position(|spec| spec.name.eq_ignore_ascii_case(key))
            })?;

        // `param_schema` filters, so its indices are not the plugin's. Map
        // back through the name.
        let name = &schema[position].name;
        let index = (0..self.plugin.parameter_count()).find(|index| {
            self.plugin
                .parameter_info(*index)
                .map(|info| &info.name == name)
                .unwrap_or(false)
        })?;

        Some((index, schema[position].clone()))
    }

    /// Runs one interleaved block through the plugin, in place.
    ///
    /// Blocks longer than the plugin was activated for are split: the plugin
    /// was promised `max_block` and gets no more than that, which is cheaper
    /// and more correct than re-activating mid-stream.
    ///
    /// # Errors
    ///
    /// Fails if `buffer` is not a whole number of frames, if the block's
    /// channel count is not the one the plugin was activated for, or if the
    /// plugin reports [`ProcessStatus::Error`].
    pub(crate) fn process<S: SampleType>(
        &mut self,
        buffer: &mut [S],
        channels: u16,
    ) -> CoreResult<()> {
        let channels = channels as usize;

        if channels != self.channels {
            return Err(CoreError::Format {
                format: "atome",
                code: 0,
                message: format!(
                    "activated for {} channels, asked to process {channels}",
                    self.channels
                ),
            });
        }

        if channels == 0 || buffer.len() % channels != 0 {
            return Err(CoreError::Format {
                format: "atome",
                code: 0,
                message: format!(
                    "{} samples is not a whole number of {channels}-channel frames",
                    buffer.len()
                ),
            });
        }

        for chunk in buffer.chunks_mut(self.max_block * channels) {
            self.process_block(chunk, channels)?;
        }

        Ok(())
    }

    /// One block of at most `max_block` frames.
    fn process_block<S: SampleType>(&mut self, buffer: &mut [S], channels: usize) -> CoreResult<()> {
        let frames = buffer.len() / channels;
        if frames == 0 {
            return Ok(());
        }

        // Interleaved in, planar out. Read sequentially through the source and
        // scatter, rather than the other way round: the source is the buffer
        // that is actually hot.
        for (frame, samples) in buffer.chunks(channels).enumerate() {
            for (channel, sample) in samples.iter().enumerate() {
                self.inputs[channel][frame] = sample.to_f32();
            }
        }

        // The plugin is entitled to write only some of the output — silence is
        // the safe thing for it to leave behind, not the previous block.
        for channel in &mut self.outputs {
            channel[..frames].fill(0.0);
        }

        self.outgoing.clear();

        {
            let input_views: Vec<&[f32]> =
                self.inputs.iter().map(|c| &c[..frames]).collect();
            let mut output_views: Vec<&mut [f32]> =
                self.outputs.iter_mut().map(|c| &mut c[..frames]).collect();

            let mut audio = AudioBuffer::new(
                &input_views,
                &mut output_views,
                frames,
                &self.bus,
                &self.bus,
            );

            let mut context = ProcessContext {
                sample_rate: self.sample_rate,
                max_block_size: self.max_block,
                transport: None,
                output_events: &mut self.outgoing,
            };

            match self
                .plugin
                .process(&mut audio, &self.incoming, &mut context)?
            {
                ProcessStatus::Error => {
                    return Err(CoreError::Format {
                        format: "atome",
                        code: 0,
                        message: "plugin reported an error processing the block".into(),
                    })
                }
                // Continue, Sleep, and Tail all describe what comes *next*.
                // The block just processed is good either way, and atome calls
                // every plugin on every block regardless, so there is nothing
                // to act on yet.
                _ => {}
            }
        }

        // Planar back to interleaved, over the caller's buffer.
        for (frame, samples) in buffer.chunks_mut(channels).enumerate() {
            for (channel, sample) in samples.iter_mut().enumerate() {
                *sample = S::from_f32(self.outputs[channel][frame]);
            }
        }

        Ok(())
    }
}

impl<P: PluginCore> Drop for Hosted<P> {
    fn drop(&mut self) {
        // Symmetry with the `activate` in `new`. The plugin's own Drop releases
        // the instance; this releases what activation reserved, while the
        // instance is still alive to release it.
        self.plugin.deactivate();
    }
}

/// Picks a bus layout carrying `channels` channels in and the same out.
///
/// Symmetric in and out because atome's chains process one buffer in place:
/// a 1-in/2-out layout has nowhere to put its second output channel.
///
/// Only a layout the plugin declared will do — that is the set `activate` will
/// accept. The mono and stereo fallbacks are for plugins that declare nothing
/// at all, which is common among AUs: they describe their I/O as channel-count
/// pairs rather than as a layout list, so an empty list means "unstated", not
/// "none".
fn layout_for(supported: &[BusLayout], channels: usize) -> Option<BusLayout> {
    if supported.is_empty() {
        return match channels {
            1 => Some(BusLayout::mono()),
            2 => Some(BusLayout::stereo()),
            _ => None,
        };
    }

    supported
        .iter()
        .find(|layout| {
            main_channels(&layout.inputs) == Some(channels)
                && main_channels(&layout.outputs) == Some(channels)
        })
        .cloned()
}

/// Channel count of the main (first) bus, if there is one.
fn main_channels(buses: &[truce_rack_core::bus::Bus]) -> Option<usize> {
    buses.first().map(|bus| bus.channels.count() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use truce_rack_core::{
        bus::{Bus, ChannelConfig},
        info::{ParameterInfo, PluginCategory, PluginInfo, PresetInfo},
    };

    fn layout(inputs: ChannelConfig, outputs: ChannelConfig) -> BusLayout {
        BusLayout {
            inputs: std::iter::once(Bus::main("Input", inputs)).collect(),
            outputs: std::iter::once(Bus::main("Output", outputs)).collect(),
        }
    }

    // -------------------------------------------------------------------------
    //  A plugin standing in for a real one
    // -------------------------------------------------------------------------

    /// What the fake plugin should do with a block.
    #[derive(Clone, Copy)]
    enum Behaviour {
        /// Copy input to output, so the buffer should come back unchanged —
        /// which is only true if deinterleaving and re-interleaving agree.
        PassThrough,
        /// Write the channel index into every sample of that channel. Proves
        /// the planar channels line up with the interleaved ones, and in the
        /// right order.
        StampChannelIndex,
        /// Write nothing at all. The output should be silence, not whatever
        /// the last block left behind.
        WriteNothing,
        /// Fail the block.
        Fail,
    }

    /// A `truce-rack` plugin that exists only to be driven by [`Hosted`].
    struct Fake {
        info: PluginInfo,
        layouts: Vec<BusLayout>,
        behaviour: Behaviour,
        active: bool,
        /// Frames seen per `process` call, so block splitting can be checked.
        blocks: Arc<std::sync::Mutex<Vec<usize>>>,
        /// Counts `deactivate`, so the `Drop` impl can be checked.
        deactivations: Arc<AtomicUsize>,
    }

    impl Fake {
        fn new(behaviour: Behaviour, layouts: Vec<BusLayout>) -> Self {
            Self {
                info: PluginInfo {
                    name: "Fake".into(),
                    vendor: "atome tests".into(),
                    version: 1,
                    category: PluginCategory::Effect,
                    path: std::path::PathBuf::new(),
                    unique_id: "fake".into(),
                    format: "fake",
                    has_editor: false,
                    accepts_midi: false,
                },
                layouts,
                behaviour,
                active: false,
                blocks: Arc::new(std::sync::Mutex::new(Vec::new())),
                deactivations: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl PluginCore for Fake {
        fn info(&self) -> &PluginInfo {
            &self.info
        }
        fn active_layout(&self) -> Option<&BusLayout> {
            None
        }
        fn supported_layouts(&self) -> &[BusLayout] {
            &self.layouts
        }
        fn parameter_count(&self) -> usize {
            0
        }
        fn parameter_info(&self, index: usize) -> CoreResult<ParameterInfo> {
            Err(CoreError::InvalidParameter(index))
        }
        fn parameter_value(&self, index: usize) -> CoreResult<f64> {
            Err(CoreError::InvalidParameter(index))
        }
        fn parameter_value_string(&self, index: usize, _value: f64) -> CoreResult<String> {
            Err(CoreError::InvalidParameter(index))
        }
        fn set_parameter(&mut self, index: usize, _value: f64) -> CoreResult<()> {
            Err(CoreError::InvalidParameter(index))
        }
        fn preset_count(&self) -> usize {
            0
        }
        fn preset_info(&self, index: usize) -> CoreResult<PresetInfo> {
            Err(CoreError::InvalidParameter(index))
        }
        fn load_preset(&mut self, _preset_number: i32) -> CoreResult<()> {
            Ok(())
        }
        fn save_state(&self) -> CoreResult<Vec<u8>> {
            Ok(Vec::new())
        }
        fn load_state(&mut self, _bytes: &[u8]) -> CoreResult<()> {
            Ok(())
        }
        fn activate(
            &mut self,
            _layout: BusLayout,
            _sample_rate: f64,
            _max_block_size: usize,
        ) -> CoreResult<()> {
            self.active = true;
            Ok(())
        }
        fn deactivate(&mut self) {
            self.active = false;
            self.deactivations.fetch_add(1, Ordering::Relaxed);
        }
        fn is_active(&self) -> bool {
            self.active
        }
    }

    impl CorePlugin<f32> for Fake {
        fn process(
            &mut self,
            buffer: &mut AudioBuffer<'_, f32>,
            _events: &EventList,
            _context: &mut ProcessContext<'_>,
        ) -> CoreResult<ProcessStatus> {
            let frames = buffer.num_frames();
            self.blocks.lock().unwrap().push(frames);

            match self.behaviour {
                Behaviour::PassThrough => {
                    // `main_inputs` has to be read out before `main_outputs`
                    // takes its mutable borrow.
                    let inputs: Vec<Vec<f32>> =
                        buffer.main_inputs().iter().map(|c| c.to_vec()).collect();
                    for (output, input) in buffer.main_outputs().iter_mut().zip(inputs) {
                        output.copy_from_slice(&input);
                    }
                }
                Behaviour::StampChannelIndex => {
                    for (index, output) in buffer.main_outputs().iter_mut().enumerate() {
                        output.fill(index as f32);
                    }
                }
                Behaviour::WriteNothing => {}
                Behaviour::Fail => return Ok(ProcessStatus::Error),
            }

            Ok(ProcessStatus::Continue)
        }
    }

    fn hosted(behaviour: Behaviour, channels: usize, max_block: usize) -> Hosted<Fake> {
        Hosted::new(
            Fake::new(behaviour, vec![]),
            channels,
            48_000.0,
            max_block,
        )
        .expect("stereo and mono have fallback layouts")
    }

    // -------------------------------------------------------------------------
    //  The block path
    // -------------------------------------------------------------------------

    #[test]
    fn a_pass_through_plugin_leaves_the_buffer_alone() {
        let mut plugin = hosted(Behaviour::PassThrough, 2, 512);

        // Deliberately asymmetric between channels: a buffer that survives
        // this survives a deinterleave/re-interleave that swapped them.
        let before = [0.1_f32, -0.9, 0.2, -0.8, 0.3, -0.7];
        let mut buffer = before;

        plugin.process(&mut buffer, 2).expect("processed");

        assert_eq!(buffer, before);
    }

    #[test]
    fn planar_channels_land_back_in_the_right_interleaved_slots() {
        let mut plugin = hosted(Behaviour::StampChannelIndex, 2, 512);

        let mut buffer = [9.0_f32; 6];
        plugin.process(&mut buffer, 2).expect("processed");

        // Channel 0 stamped 0, channel 1 stamped 1, interleaved.
        assert_eq!(buffer, [0.0, 1.0, 0.0, 1.0, 0.0, 1.0]);
    }

    #[test]
    fn a_plugin_that_writes_nothing_yields_silence() {
        let mut plugin = hosted(Behaviour::WriteNothing, 2, 512);

        // Run a loud block first, so the scratch has something in it, then a
        // second one. The second must not hear the first.
        let mut buffer = [1.0_f32; 4];
        plugin.process(&mut buffer, 2).expect("first block");
        assert_eq!(buffer, [0.0; 4]);

        let mut buffer = [1.0_f32; 4];
        plugin.process(&mut buffer, 2).expect("second block");
        assert_eq!(buffer, [0.0; 4]);
    }

    #[test]
    fn a_long_buffer_is_split_into_blocks_the_plugin_agreed_to() {
        let mut plugin = hosted(Behaviour::PassThrough, 2, 4);
        let blocks = Arc::clone(&plugin.plugin.blocks);

        // 10 frames through a plugin activated for 4: 4, 4, then 2.
        let mut buffer = [0.5_f32; 20];
        plugin.process(&mut buffer, 2).expect("processed");

        assert_eq!(*blocks.lock().unwrap(), vec![4, 4, 2]);
        assert_eq!(buffer, [0.5; 20]);
    }

    #[test]
    fn converts_to_and_from_the_engines_own_sample_type() {
        let mut plugin = hosted(Behaviour::PassThrough, 2, 512);

        // i16 in, i16 out, f32 in the plugin. Full-scale values are the ones
        // a lossy conversion would round wrong.
        let before = [i16::MAX, i16::MIN, 0, 12_345];
        let mut buffer = before;

        plugin.process(&mut buffer, 2).expect("processed");

        assert_eq!(buffer, before);
    }

    #[test]
    fn an_empty_buffer_is_not_an_error() {
        let mut plugin = hosted(Behaviour::PassThrough, 2, 512);
        plugin.process(&mut [] as &mut [f32], 2).expect("nothing to do");
    }

    #[test]
    fn refuses_a_channel_count_it_was_not_activated_for() {
        let mut plugin = hosted(Behaviour::PassThrough, 2, 512);

        let mut buffer = [0.0_f32; 4];
        let error = plugin.process(&mut buffer, 1).expect_err("activated for 2");
        assert!(error.to_string().contains("activated for 2"));
    }

    #[test]
    fn refuses_a_partial_frame() {
        let mut plugin = hosted(Behaviour::PassThrough, 2, 512);

        let mut buffer = [0.0_f32; 5];
        assert!(plugin.process(&mut buffer, 2).is_err());
    }

    #[test]
    fn a_failing_plugin_fails_the_block() {
        let mut plugin = hosted(Behaviour::Fail, 2, 512);

        let mut buffer = [0.0_f32; 4];
        assert!(plugin.process(&mut buffer, 2).is_err());
    }

    #[test]
    fn activation_happens_on_construction_and_is_undone_on_drop() {
        let plugin = hosted(Behaviour::PassThrough, 2, 512);
        assert!(plugin.plugin.is_active());

        let deactivations = Arc::clone(&plugin.plugin.deactivations);
        drop(plugin);

        assert_eq!(deactivations.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_channel_count_with_no_layout_refuses_to_load() {
        // Seven channels, nothing declared, no fallback — `new` should refuse
        // rather than activate something the plugin never agreed to.
        let Err(error) = Hosted::new(Fake::new(Behaviour::PassThrough, vec![]), 7, 48_000.0, 512)
        else {
            panic!("a 7-channel activation should have been refused");
        };

        assert!(error.to_string().contains("7-channel"));
    }

    #[test]
    fn prefers_a_declared_layout() {
        let declared = vec![
            layout(ChannelConfig::Mono, ChannelConfig::Mono),
            layout(ChannelConfig::Surround5_1, ChannelConfig::Surround5_1),
        ];

        let picked = layout_for(&declared, 6).expect("5.1 is declared");
        assert_eq!(main_channels(&picked.outputs), Some(6));
    }

    #[test]
    fn falls_back_to_stereo_when_nothing_is_declared() {
        let picked = layout_for(&[], 2).expect("stereo fallback");
        assert_eq!(main_channels(&picked.inputs), Some(2));
    }

    #[test]
    fn refuses_a_count_it_cannot_build() {
        // Nothing declared and no fallback for seven channels — better to say
        // so than to activate a layout the plugin never agreed to.
        assert!(layout_for(&[], 7).is_none());
    }

    #[test]
    fn will_not_pick_an_asymmetric_layout() {
        let declared = vec![layout(ChannelConfig::Mono, ChannelConfig::Stereo)];
        // In-place processing has one buffer, so a 1-in/2-out layout has
        // nowhere to put the second output channel.
        assert!(layout_for(&declared, 1).is_none());
    }
}
