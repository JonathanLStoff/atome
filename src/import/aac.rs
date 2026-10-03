//! AAC through the operating system's decoder — never a bundled one.
//!
//! AAC is patent-pooled. atome goes into paid software, so it decodes AAC only
//! with the decoder the operating system already ships and already licensed:
//!
//! | Platform | Decoder |
//! |---|---|
//! | macOS, iOS | AudioToolbox (`AudioConverter`) — LC, HE-AAC v1 and v2 |
//! | Windows | Media Foundation's AAC decoder MFT — LC, HE-AAC v1 and v2 |
//! | Android | MediaCodec (`audio/mp4a-latm`) |
//! | Linux, elsewhere | none: AAC is refused by name |
//!
//! Symphonia's own AAC decoder and libfdk-aac are not compiled in at all.
//! Symphonia still *demuxes* — MP4 and Matroska hand their AAC packets to
//! [`OsAacDecoder`], registered in its place — and bare ADTS files, whose
//! reader lived in Symphonia's AAC crate, are split here ([`AdtsStream`]).
//!
//! Every backend turns one AAC access unit into interleaved `f32`, at the rate
//! and channel count the *decoder* reports: HE-AAC decodes at twice the rate
//! its core declares, and only the decoder knows.

use std::path::Path;

use cpal::{Error, ErrorKind, SampleFormat};

use crate::output::SampleType;

use super::AudioStream;

/// What an `AudioSpecificConfig` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    /// 2 for LC, 5 for SBR (HE-AAC), 29 for PS (HE-AAC v2).
    pub object_type: u8,
    /// The core's sample rate.
    pub sample_rate: u32,
    pub channels: u16,
}

const RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

impl Config {
    /// Reads the fields every decoder needs from an `AudioSpecificConfig`.
    pub(crate) fn parse(asc: &[u8]) -> Option<Self> {
        let mut bits = asc.iter().flat_map(|byte| (0..8).rev().map(move |bit| (byte >> bit) & 1));
        let mut take = |count: u32| -> Option<u32> {
            (0..count).try_fold(0_u32, |value, _| Some((value << 1) | u32::from(bits.next()?)))
        };

        let mut object_type = take(5)?;
        if object_type == 31 {
            object_type = 32 + take(6)?;
        }

        let index = take(4)?;
        let sample_rate = if index == 15 {
            take(24)?
        } else {
            *RATES.get(index as usize)?
        };

        let channels = take(4)?;

        Some(Config {
            object_type: object_type as u8,
            sample_rate,
            channels: if channels == 7 { 8 } else { channels as u16 },
        })
    }

    /// An `AudioSpecificConfig` for an ADTS header's fields.
    pub(crate) fn from_adts(header: &[u8]) -> Option<(Self, Vec<u8>)> {
        if header.len() < 7 || header[0] != 0xFF || header[1] & 0xF0 != 0xF0 {
            return None;
        }

        let object_type = (header[2] >> 6) + 1;
        let index = (header[2] >> 2) & 0x0F;
        let channels = ((header[2] & 0x01) << 2) | (header[3] >> 6);

        let asc = vec![
            (object_type << 3) | (index >> 1),
            ((index & 1) << 7) | (channels << 3),
        ];

        Some((
            Config {
                object_type,
                sample_rate: *RATES.get(index as usize)?,
                channels: u16::from(channels),
            },
            asc,
        ))
    }
}

/// One platform's AAC decoder.
pub(crate) trait Backend: Send + Sync {
    /// One access unit in; whatever it decodes to appended to `out`,
    /// interleaved.
    fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), String>;
    /// Forgets everything, for a seek.
    fn reset(&mut self);
    /// The rate and channel count the decoder puts out.
    fn output(&self) -> (u32, u16);
}

/// The operating system's AAC decoder for `asc`, or why there is none.
pub(crate) fn open(asc: &[u8]) -> Result<Box<dyn Backend>, String> {
    let config = Config::parse(asc).ok_or("the AudioSpecificConfig does not parse")?;
    platform::open(asc, config)
}

#[cfg(target_vendor = "apple")]
mod platform {
    //! AudioToolbox's `AudioConverter`, from AAC to interleaved `f32`.

    use std::ffi::c_void;

    use super::{Backend, Config};

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Asbd {
        sample_rate: f64,
        format_id: u32,
        format_flags: u32,
        bytes_per_packet: u32,
        frames_per_packet: u32,
        bytes_per_frame: u32,
        channels_per_frame: u32,
        bits_per_channel: u32,
        reserved: u32,
    }

    #[repr(C)]
    struct FormatInfo {
        asbd: Asbd,
        cookie: *const c_void,
        cookie_size: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct FormatListItem {
        asbd: Asbd,
        layout_tag: u32,
    }

    #[repr(C)]
    struct AudioBuffer {
        channels: u32,
        size: u32,
        data: *mut c_void,
    }

    #[repr(C)]
    struct AudioBufferList {
        count: u32,
        buffers: [AudioBuffer; 1],
    }

    #[repr(C)]
    #[derive(Default)]
    struct PacketDescription {
        start: i64,
        variable_frames: u32,
        size: u32,
    }

    type InputProc = unsafe extern "C" fn(
        converter: *mut c_void,
        packets: *mut u32,
        data: *mut AudioBufferList,
        descriptions: *mut *mut PacketDescription,
        user: *mut c_void,
    ) -> i32;

    const FORMAT_AAC: u32 = u32::from_be_bytes(*b"aac ");
    const FORMAT_LPCM: u32 = u32::from_be_bytes(*b"lpcm");
    const FLOAT_PACKED: u32 = 1 | 8;
    const PROPERTY_FORMAT_LIST: u32 = u32::from_be_bytes(*b"flst");
    const PROPERTY_COOKIE: u32 = u32::from_be_bytes(*b"dmgc");
    /// What the input callback returns once its one packet is spent: not an
    /// error, just "nothing more for now".
    const NO_MORE: i32 = 0x6D6F_7265;

    #[link(name = "AudioToolbox", kind = "framework")]
    extern "C" {
        fn AudioFormatGetPropertyInfo(id: u32, specifier_size: u32, specifier: *const c_void, size: *mut u32) -> i32;
        fn AudioFormatGetProperty(
            id: u32,
            specifier_size: u32,
            specifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
        fn AudioConverterNew(source: *const Asbd, destination: *const Asbd, converter: *mut *mut c_void) -> i32;
        fn AudioConverterSetProperty(converter: *mut c_void, id: u32, size: u32, data: *const c_void) -> i32;
        fn AudioConverterFillComplexBuffer(
            converter: *mut c_void,
            input: InputProc,
            user: *mut c_void,
            frames: *mut u32,
            output: *mut AudioBufferList,
            descriptions: *mut PacketDescription,
        ) -> i32;
        fn AudioConverterReset(converter: *mut c_void) -> i32;
        fn AudioConverterDispose(converter: *mut c_void) -> i32;
    }

    /// An MPEG-4 elementary stream descriptor around the
    /// `AudioSpecificConfig`: the magic cookie AudioToolbox expects for AAC.
    fn esds(asc: &[u8]) -> Vec<u8> {
        fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
            let length = body.len() as u32;
            let mut out = vec![
                tag,
                0x80 | ((length >> 21) & 0x7F) as u8,
                0x80 | ((length >> 14) & 0x7F) as u8,
                0x80 | ((length >> 7) & 0x7F) as u8,
                (length & 0x7F) as u8,
            ];
            out.extend_from_slice(body);
            out
        }

        let specific = descriptor(0x05, asc);
        let mut config = vec![0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        config.extend(specific);
        let decoder_config = descriptor(0x04, &config);
        let sl_config = descriptor(0x06, &[0x02]);

        let mut stream = vec![0, 0, 0];
        stream.extend(decoder_config);
        stream.extend(sl_config);
        descriptor(0x03, &stream)
    }

    struct Converter {
        converter: *mut c_void,
        channels: u16,
        rate: u32,
        source_channels: u32,
    }

    // SAFETY: an AudioConverter may be used from any thread, one call at a
    // time, which `&mut self` guarantees.
    unsafe impl Send for Converter {}
    // SAFETY: shared references only read plain fields; the converter is
    // touched only through `&mut self`.
    unsafe impl Sync for Converter {}

    /// The one packet the input callback hands over.
    struct Pending<'a> {
        packet: &'a [u8],
        description: PacketDescription,
        channels: u32,
        given: bool,
    }

    unsafe extern "C" fn supply(
        _converter: *mut c_void,
        packets: *mut u32,
        data: *mut AudioBufferList,
        descriptions: *mut *mut PacketDescription,
        user: *mut c_void,
    ) -> i32 {
        // SAFETY: `user` is the `Pending` `decode` passed in, alive for the
        // whole fill call; the other pointers are AudioToolbox's own.
        unsafe {
            let pending = &mut *user.cast::<Pending<'_>>();

            if pending.given {
                *packets = 0;
                return NO_MORE;
            }

            pending.given = true;
            pending.description = PacketDescription {
                start: 0,
                variable_frames: 0,
                size: pending.packet.len() as u32,
            };

            (*data).count = 1;
            (*data).buffers[0] = AudioBuffer {
                channels: pending.channels,
                size: pending.packet.len() as u32,
                data: pending.packet.as_ptr().cast_mut().cast(),
            };
            *packets = 1;
            if !descriptions.is_null() {
                *descriptions = &mut pending.description;
            }
            0
        }
    }

    pub(super) fn open(asc: &[u8], config: Config) -> Result<Box<dyn Backend>, String> {
        let cookie = esds(asc);

        let mut source = Asbd {
            sample_rate: f64::from(config.sample_rate),
            format_id: FORMAT_AAC,
            frames_per_packet: 1024,
            channels_per_frame: u32::from(config.channels.max(1)),
            ..Asbd::default()
        };

        // The richest format the cookie describes — HE-AAC v2 over HE-AAC over
        // LC — so SBR and PS are decoded rather than dropped.
        let info = FormatInfo {
            asbd: source,
            cookie: cookie.as_ptr().cast(),
            cookie_size: cookie.len() as u32,
        };
        let info_size = std::mem::size_of::<FormatInfo>() as u32;
        let mut size = 0_u32;

        // SAFETY: a live specifier of the size given; `size` is an out-param.
        if unsafe { AudioFormatGetPropertyInfo(PROPERTY_FORMAT_LIST, info_size, (&raw const info).cast(), &mut size) } == 0
            && size as usize >= std::mem::size_of::<FormatListItem>()
        {
            let mut items = vec![FormatListItem::default(); size as usize / std::mem::size_of::<FormatListItem>()];
            // SAFETY: `items` holds `size` bytes.
            let status = unsafe {
                AudioFormatGetProperty(
                    PROPERTY_FORMAT_LIST,
                    info_size,
                    (&raw const info).cast(),
                    &mut size,
                    items.as_mut_ptr().cast(),
                )
            };
            if status == 0 {
                if let Some(first) = items.first() {
                    source = first.asbd;
                }
            }
        }

        let channels = source.channels_per_frame.max(1);
        let destination = Asbd {
            sample_rate: source.sample_rate,
            format_id: FORMAT_LPCM,
            format_flags: FLOAT_PACKED,
            bytes_per_packet: 4 * channels,
            frames_per_packet: 1,
            bytes_per_frame: 4 * channels,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        };

        let mut converter = std::ptr::null_mut();
        // SAFETY: two live descriptions; `converter` receives a new converter.
        let status = unsafe { AudioConverterNew(&source, &destination, &mut converter) };
        if status != 0 || converter.is_null() {
            return Err(format!("AudioToolbox would not open an AAC decoder (OSStatus {status})"));
        }

        // SAFETY: a live converter and the cookie's bytes.
        unsafe { AudioConverterSetProperty(converter, PROPERTY_COOKIE, cookie.len() as u32, cookie.as_ptr().cast()) };

        Ok(Box::new(Converter {
            converter,
            channels: channels as u16,
            rate: source.sample_rate as u32,
            source_channels: source.channels_per_frame,
        }))
    }

    impl Backend for Converter {
        fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), String> {
            let mut pending = Pending {
                packet,
                description: PacketDescription::default(),
                channels: self.source_channels,
                given: false,
            };

            let capacity = 4096_usize;
            let mut buffer = vec![0.0_f32; capacity * usize::from(self.channels)];

            loop {
                let mut list = AudioBufferList {
                    count: 1,
                    buffers: [AudioBuffer {
                        channels: u32::from(self.channels),
                        size: (buffer.len() * 4) as u32,
                        data: buffer.as_mut_ptr().cast(),
                    }],
                };
                let mut frames = capacity as u32;

                // SAFETY: a live converter; `pending` outlives the call, and
                // the output list describes `buffer` exactly.
                let status = unsafe {
                    AudioConverterFillComplexBuffer(
                        self.converter,
                        supply,
                        (&raw mut pending).cast(),
                        &mut frames,
                        &mut list,
                        std::ptr::null_mut(),
                    )
                };

                if status != 0 && status != NO_MORE {
                    return Err(format!("AudioToolbox could not decode a packet (OSStatus {status})"));
                }

                out.extend_from_slice(&buffer[..frames as usize * usize::from(self.channels)]);

                if status == NO_MORE || frames == 0 {
                    return Ok(());
                }
            }
        }

        fn reset(&mut self) {
            // SAFETY: a live converter.
            unsafe { AudioConverterReset(self.converter) };
        }

        fn output(&self) -> (u32, u16) {
            (self.rate, self.channels)
        }
    }

    impl Drop for Converter {
        fn drop(&mut self) {
            // SAFETY: owned outright.
            unsafe { AudioConverterDispose(self.converter) };
        }
    }
}

#[cfg(windows)]
mod platform {
    //! Media Foundation's AAC decoder MFT, from AAC to interleaved `f32`.

    use windows::core::GUID;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

    use super::{Backend, Config};

    struct Mft {
        mft: IMFTransform,
        rate: u32,
        channels: u16,
    }

    // SAFETY: the AAC decoder MFT is free-threaded; `&mut self` keeps it to
    // one caller at a time.
    unsafe impl Send for Mft {}
    // SAFETY: shared references only read plain fields.
    unsafe impl Sync for Mft {}

    fn failed(doing: &str, error: windows::core::Error) -> String {
        format!("Media Foundation, {doing}: {error}")
    }

    pub(super) fn open(asc: &[u8], config: Config) -> Result<Box<dyn Backend>, String> {
        // SAFETY: COM and Media Foundation start-up, each counted and harmless
        // to repeat; already initialised in another mode is fine.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(|e| failed("starting", e))?;
        }

        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_AAC,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_Float,
        };

        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0_u32;

        // SAFETY: both type infos live for the call; MF allocates `list`.
        unsafe {
            MFTEnumEx(
                MFT_CATEGORY_AUDIO_DECODER,
                MFT_ENUM_FLAG(MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0),
                Some(&input),
                Some(&output),
                &mut list,
                &mut count,
            )
            .map_err(|e| failed("finding the AAC decoder", e))?;
        }

        if list.is_null() || count == 0 {
            return Err("Windows has no AAC decoder here".to_string());
        }

        // SAFETY: MF wrote `count` entries at `list`; the array is freed once
        // they are owned here.
        let activates: Vec<Option<IMFActivate>> =
            (0..count as usize).map(|index| unsafe { list.add(index).read() }).collect();
        unsafe { CoTaskMemFree(Some(list as *const _)) };

        let activate = activates.into_iter().flatten().next().ok_or("Windows has no AAC decoder here")?;
        // SAFETY: a live activation object.
        let mft: IMFTransform = unsafe { activate.ActivateObject() }.map_err(|e| failed("opening the decoder", e))?;

        // HEAACWAVEINFO's tail — raw payload, profile unspecified — then the
        // AudioSpecificConfig: what MF_MT_USER_DATA holds for AAC.
        let mut user_data = vec![0_u8; 12];
        user_data[2] = 0xFE;
        user_data.extend_from_slice(asc);

        // SAFETY: a fresh media type handed to a live MFT.
        unsafe {
            let kind = MFCreateMediaType().map_err(|e| failed("a type", e))?;
            kind.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(|e| failed("a type", e))?;
            kind.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).map_err(|e| failed("a type", e))?;
            kind.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, config.sample_rate).map_err(|e| failed("the rate", e))?;
            kind.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(config.channels.max(1)))
                .map_err(|e| failed("the channels", e))?;
            kind.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(|e| failed("the payload", e))?;
            kind.SetBlob(&MF_MT_USER_DATA, &user_data).map_err(|e| failed("the config", e))?;
            mft.SetInputType(0, &kind, 0).map_err(|e| failed("the input type", e))?;
        }

        let (rate, channels) = float_output(&mft)?;

        // SAFETY: a live MFT with both types set.
        unsafe {
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }

        Ok(Box::new(Mft { mft, rate, channels }))
    }

    /// Sets the first float output type on offer, and says what it is.
    fn float_output(mft: &IMFTransform) -> Result<(u32, u16), String> {
        for index in 0.. {
            // SAFETY: a live MFT; the end of the list is an error, which ends
            // the search.
            let offered = unsafe { mft.GetOutputAvailableType(0, index) }.map_err(|e| failed("a float output", e))?;
            // SAFETY: a live media type.
            let subtype: GUID = unsafe { offered.GetGUID(&MF_MT_SUBTYPE) }.unwrap_or_default();
            if subtype == MFAudioFormat_Float {
                // SAFETY: as above.
                unsafe {
                    mft.SetOutputType(0, &offered, 0).map_err(|e| failed("the output type", e))?;
                    let rate = offered.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(48_000);
                    let channels = offered.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(2);
                    return Ok((rate, channels as u16));
                }
            }
        }
        Err("no float output".to_string())
    }

    impl Mft {
        fn pull(&self, out: &mut Vec<f32>) -> Result<bool, String> {
            // SAFETY: plain allocations, owned here; one output buffer for the
            // MFT's one stream.
            unsafe {
                let info = self.mft.GetOutputStreamInfo(0).map_err(|e| failed("stream info", e))?;
                let sample = MFCreateSample().map_err(|e| failed("a sample", e))?;
                let buffer = MFCreateMemoryBuffer(info.cbSize.max(1 << 16)).map_err(|e| failed("a buffer", e))?;
                sample.AddBuffer(&buffer).map_err(|e| failed("a buffer", e))?;

                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(Some(sample)),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = self.mft.ProcessOutput(0, &mut buffers, &mut status);
                let [buffer] = buffers;
                let sample = std::mem::ManuallyDrop::into_inner(buffer.pSample);
                drop(std::mem::ManuallyDrop::into_inner(buffer.pEvents));

                match result {
                    Ok(()) => {
                        if let Some(sample) = sample {
                            let contiguous = sample.ConvertToContiguousBuffer().map_err(|e| failed("output", e))?;
                            let mut data = std::ptr::null_mut();
                            let mut length = 0_u32;
                            contiguous.Lock(&mut data, None, Some(&mut length)).map_err(|e| failed("output", e))?;
                            let floats = std::slice::from_raw_parts(data.cast::<f32>(), length as usize / 4);
                            out.extend_from_slice(floats);
                            let _ = contiguous.Unlock();
                        }
                        Ok(true)
                    }
                    Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
                    Err(error) => Err(failed("decoding", error)),
                }
            }
        }
    }

    impl Backend for Mft {
        fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), String> {
            // SAFETY: a fresh buffer, locked only to copy in its own length,
            // handed to a live MFT.
            unsafe {
                let buffer = MFCreateMemoryBuffer(packet.len() as u32).map_err(|e| failed("a buffer", e))?;
                let mut data = std::ptr::null_mut();
                buffer.Lock(&mut data, None, None).map_err(|e| failed("a buffer", e))?;
                std::ptr::copy_nonoverlapping(packet.as_ptr(), data, packet.len());
                let _ = buffer.Unlock();
                buffer.SetCurrentLength(packet.len() as u32).map_err(|e| failed("a buffer", e))?;

                let sample = MFCreateSample().map_err(|e| failed("a sample", e))?;
                sample.AddBuffer(&buffer).map_err(|e| failed("a sample", e))?;
                self.mft.ProcessInput(0, &sample, 0).map_err(|e| failed("decoding", e))?;
            }

            while self.pull(out)? {}
            Ok(())
        }

        fn reset(&mut self) {
            // SAFETY: a live MFT.
            unsafe {
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            }
        }

        fn output(&self) -> (u32, u16) {
            (self.rate, self.channels)
        }
    }
}

#[cfg(target_os = "android")]
mod platform {
    //! MediaCodec's `audio/mp4a-latm` decoder, from AAC to interleaved `f32`.

    use std::ffi::{c_char, c_void};

    use super::{Backend, Config};

    #[repr(C)]
    #[derive(Default)]
    struct BufferInfo {
        offset: i32,
        size: i32,
        presentation_time_us: i64,
        flags: u32,
    }

    const OUTPUT_FORMAT_CHANGED: isize = -2;

    #[link(name = "mediandk")]
    extern "C" {
        fn AMediaCodec_createDecoderByType(mime: *const c_char) -> *mut c_void;
        fn AMediaCodec_configure(codec: *mut c_void, format: *const c_void, surface: *mut c_void, crypto: *mut c_void, flags: u32) -> i32;
        fn AMediaCodec_start(codec: *mut c_void) -> i32;
        fn AMediaCodec_stop(codec: *mut c_void) -> i32;
        fn AMediaCodec_delete(codec: *mut c_void) -> i32;
        fn AMediaCodec_flush(codec: *mut c_void) -> i32;
        fn AMediaCodec_dequeueInputBuffer(codec: *mut c_void, timeout_us: i64) -> isize;
        fn AMediaCodec_getInputBuffer(codec: *mut c_void, index: usize, size: *mut usize) -> *mut u8;
        fn AMediaCodec_queueInputBuffer(codec: *mut c_void, index: usize, offset: isize, size: usize, time: u64, flags: u32) -> i32;
        fn AMediaCodec_dequeueOutputBuffer(codec: *mut c_void, info: *mut BufferInfo, timeout_us: i64) -> isize;
        fn AMediaCodec_getOutputBuffer(codec: *mut c_void, index: usize, size: *mut usize) -> *mut u8;
        fn AMediaCodec_releaseOutputBuffer(codec: *mut c_void, index: usize, render: bool) -> i32;
        fn AMediaCodec_getOutputFormat(codec: *mut c_void) -> *mut c_void;
        fn AMediaFormat_new() -> *mut c_void;
        fn AMediaFormat_delete(format: *mut c_void) -> i32;
        fn AMediaFormat_setInt32(format: *mut c_void, name: *const c_char, value: i32);
        fn AMediaFormat_setString(format: *mut c_void, name: *const c_char, value: *const c_char);
        fn AMediaFormat_setBuffer(format: *mut c_void, name: *const c_char, data: *const c_void, size: usize);
        fn AMediaFormat_getInt32(format: *mut c_void, name: *const c_char, out: *mut i32) -> bool;
    }

    struct Codec {
        codec: *mut c_void,
        rate: u32,
        channels: u16,
        /// 2 for 16-bit integers, 4 for float — what the device actually gives.
        encoding: i32,
        time: u64,
    }

    // SAFETY: the synchronous NDK API, one call at a time via `&mut self`.
    unsafe impl Send for Codec {}
    // SAFETY: shared references only read plain fields.
    unsafe impl Sync for Codec {}

    pub(super) fn open(asc: &[u8], config: Config) -> Result<Box<dyn Backend>, String> {
        // SAFETY: NUL-terminated strings, a fresh format owned and deleted
        // here, and a codec owned by the returned value.
        unsafe {
            let codec = AMediaCodec_createDecoderByType(c"audio/mp4a-latm".as_ptr());
            if codec.is_null() {
                return Err("this device has no AAC decoder MediaCodec will open".to_string());
            }

            let format = AMediaFormat_new();
            AMediaFormat_setString(format, c"mime".as_ptr(), c"audio/mp4a-latm".as_ptr());
            AMediaFormat_setInt32(format, c"sample-rate".as_ptr(), config.sample_rate as i32);
            AMediaFormat_setInt32(format, c"channel-count".as_ptr(), i32::from(config.channels.max(1)));
            AMediaFormat_setInt32(format, c"is-adts".as_ptr(), 0);
            // Float where the device will (API 24); 16-bit otherwise, checked
            // on the first format change.
            AMediaFormat_setInt32(format, c"pcm-encoding".as_ptr(), 4);
            AMediaFormat_setBuffer(format, c"csd-0".as_ptr(), asc.as_ptr().cast(), asc.len());

            let status = AMediaCodec_configure(codec, format, std::ptr::null_mut(), std::ptr::null_mut(), 0);
            AMediaFormat_delete(format);

            if status != 0 || AMediaCodec_start(codec) != 0 {
                AMediaCodec_delete(codec);
                return Err(format!("MediaCodec would not start an AAC decoder (status {status})"));
            }

            Ok(Box::new(Codec {
                codec,
                rate: config.sample_rate,
                channels: config.channels.max(1),
                encoding: 2,
                time: 0,
            }))
        }
    }

    impl Codec {
        fn reread(&mut self) {
            // SAFETY: a started codec; the format is the caller's to delete.
            unsafe {
                let format = AMediaCodec_getOutputFormat(self.codec);
                if format.is_null() {
                    return;
                }
                let mut value = 0;
                if AMediaFormat_getInt32(format, c"sample-rate".as_ptr(), &mut value) {
                    self.rate = value as u32;
                }
                if AMediaFormat_getInt32(format, c"channel-count".as_ptr(), &mut value) {
                    self.channels = value as u16;
                }
                self.encoding = if AMediaFormat_getInt32(format, c"pcm-encoding".as_ptr(), &mut value) {
                    value
                } else {
                    2
                };
                AMediaFormat_delete(format);
            }
        }

        fn drain(&mut self, out: &mut Vec<f32>, timeout_us: i64) {
            let mut wait = timeout_us;
            loop {
                let mut info = BufferInfo::default();
                // SAFETY: a started codec and a live out-pointer.
                let index = unsafe { AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, wait) };
                wait = 0;

                if index == OUTPUT_FORMAT_CHANGED {
                    self.reread();
                    continue;
                }
                if index < 0 {
                    return;
                }

                // SAFETY: an index the codec handed out, released below.
                unsafe {
                    let mut size = 0;
                    let buffer = AMediaCodec_getOutputBuffer(self.codec, index as usize, &mut size);
                    if !buffer.is_null() {
                        let bytes = std::slice::from_raw_parts(buffer.add(info.offset.max(0) as usize), info.size.max(0) as usize);
                        if self.encoding == 4 {
                            out.extend(bytes.chunks_exact(4).map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]])));
                        } else {
                            out.extend(
                                bytes
                                    .chunks_exact(2)
                                    .map(|b| f32::from(i16::from_ne_bytes([b[0], b[1]])) / 32_768.0),
                            );
                        }
                    }
                    AMediaCodec_releaseOutputBuffer(self.codec, index as usize, false);
                }
            }
        }
    }

    impl Backend for Codec {
        fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) -> Result<(), String> {
            loop {
                // SAFETY: a started codec.
                let index = unsafe { AMediaCodec_dequeueInputBuffer(self.codec, 10_000) };
                if index < 0 {
                    self.drain(out, 10_000);
                    continue;
                }

                // SAFETY: an index the codec handed out, filled within its size.
                unsafe {
                    let mut size = 0;
                    let buffer = AMediaCodec_getInputBuffer(self.codec, index as usize, &mut size);
                    if buffer.is_null() || size < packet.len() {
                        return Err("a MediaCodec input buffer too small for an AAC packet".to_string());
                    }
                    std::ptr::copy_nonoverlapping(packet.as_ptr(), buffer, packet.len());
                    self.time += 1;
                    AMediaCodec_queueInputBuffer(self.codec, index as usize, 0, packet.len(), self.time, 0);
                }
                break;
            }

            // Output lags input by a packet or two; take what is ready, and
            // wait briefly for the rest so packets and PCM stay roughly paired.
            self.drain(out, 5_000);
            Ok(())
        }

        fn reset(&mut self) {
            // SAFETY: a started codec.
            unsafe { AMediaCodec_flush(self.codec) };
        }

        fn output(&self) -> (u32, u16) {
            (self.rate, self.channels)
        }
    }

    impl Drop for Codec {
        fn drop(&mut self) {
            // SAFETY: owned outright.
            unsafe {
                AMediaCodec_stop(self.codec);
                AMediaCodec_delete(self.codec);
            }
        }
    }
}

#[cfg(not(any(target_vendor = "apple", windows, target_os = "android")))]
mod platform {
    use super::{Backend, Config};

    pub(super) fn open(_asc: &[u8], _config: Config) -> Result<Box<dyn Backend>, String> {
        Err("this platform has no operating-system AAC decoder, and atome never bundles one \
             (AAC is patent-pooled): AAC is decoded on macOS, iOS, Windows, and Android only"
            .to_string())
    }
}

/// [`Backend`] as a Symphonia decoder, registered for AAC in place of the
/// bundled one: MP4 and Matroska are still demuxed by Symphonia, and their AAC
/// packets decoded by the operating system.
pub(crate) mod symphonia_decoder {
    use symphonia::core::audio::{
        Audio as _, AudioBuffer, AudioMut as _, AudioSpec, Channels, GenericAudioBufferRef, AsGenericAudioBufferRef as _,
    };
    use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
    use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult};
    use symphonia::core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
    use symphonia::core::codecs::CodecInfo;
    use symphonia::core::errors::{Error, Result};
    use symphonia::core::packet::PacketRef;

    use super::Backend;

    pub(crate) struct OsAacDecoder {
        backend: Box<dyn Backend>,
        params: AudioCodecParameters,
        buffer: AudioBuffer<f32>,
        pcm: Vec<f32>,
    }

    impl std::fmt::Debug for OsAacDecoder {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("OsAacDecoder").finish_non_exhaustive()
        }
    }

    const INFO: CodecInfo = CodecInfo {
        short_name: "aac-os",
        long_name: "AAC, through the operating system's decoder",
        profiles: &[],
    };

    fn spec(rate: u32, channels: u16) -> AudioSpec {
        AudioSpec::new(rate, Channels::Discrete(channels.max(1)))
    }

    impl RegisterableAudioDecoder for OsAacDecoder {
        fn try_registry_new(params: &AudioCodecParameters, _options: &AudioDecoderOptions) -> Result<Box<dyn AudioDecoder>> {
            let asc = params
                .extra_data
                .as_deref()
                .ok_or(Error::Unsupported("aac: no AudioSpecificConfig to start a decoder from"))?;

            let backend = super::open(asc).map_err(|reason| {
                log_unsupported(&reason);
                Error::Unsupported("aac: no operating-system decoder here — see atome's import::aac")
            })?;

            // The decoder is the authority on rate and channels: HE-AAC puts
            // out twice the rate its core declares.
            let (rate, channels) = backend.output();
            let mut params = params.clone();
            params.sample_rate = Some(rate);
            params.channels = Some(Channels::Discrete(channels));

            Ok(Box::new(OsAacDecoder {
                backend,
                params,
                buffer: AudioBuffer::new(spec(rate, channels), 0),
                pcm: Vec::new(),
            }))
        }

        fn supported_codecs() -> &'static [SupportedAudioCodec] {
            &[SupportedAudioCodec {
                id: CODEC_ID_AAC,
                info: INFO,
            }]
        }
    }

    /// Symphonia's error carries only a static string, so the platform's own
    /// reason goes to stderr in debug builds rather than vanishing.
    fn log_unsupported(reason: &str) {
        if cfg!(debug_assertions) {
            eprintln!("atome: {reason}");
        }
    }

    impl AudioDecoder for OsAacDecoder {
        fn reset(&mut self) {
            self.backend.reset();
            self.buffer.clear();
        }

        fn codec_info(&self) -> &CodecInfo {
            &INFO
        }

        fn codec_params(&self) -> &AudioCodecParameters {
            &self.params
        }

        fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
            self.pcm.clear();

            if let Err(reason) = self.backend.decode(packet.data, &mut self.pcm) {
                self.buffer.clear();
                log_unsupported(&reason);
                return Err(Error::DecodeError("aac: the operating system's decoder refused a packet"));
            }

            let (rate, channels) = self.backend.output();
            let frames = self.pcm.len() / usize::from(channels.max(1));

            if self.buffer.capacity() < frames || self.buffer.spec().rate() != rate {
                self.buffer = AudioBuffer::new(spec(rate, channels), frames.max(1));
            }

            self.buffer.clear();
            self.buffer.render_uninit(Some(frames));
            self.buffer.copy_from_slice_interleaved(&&self.pcm[..frames * usize::from(channels.max(1))]);
            self.buffer.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);

            Ok(self.buffer.as_generic_audio_buffer_ref())
        }

        fn finalize(&mut self) -> FinalizeResult {
            FinalizeResult::default()
        }

        fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
            self.buffer.as_generic_audio_buffer_ref()
        }
    }
}

/// A bare ADTS file, split here — Symphonia's ADTS reader came with its AAC
/// decoder and is not compiled in — and decoded by the operating system.
pub(crate) struct AdtsStream {
    data: Vec<u8>,
    cursor: usize,
    backend: Box<dyn Backend>,
    pending: Vec<f32>,
    taken: usize,
    rate: u32,
    channels: u16,
}

impl AdtsStream {
    pub(crate) fn open(path: &Path) -> Result<Self, Error> {
        let data = std::fs::read(path).map_err(|error| {
            Error::with_message(ErrorKind::InvalidInput, format!("{}: {error}", path.display()))
        })?;

        let (_, asc) = Config::from_adts(&data).ok_or_else(|| {
            Error::with_message(ErrorKind::InvalidInput, format!("{}: not ADTS", path.display()))
        })?;

        let backend = open(&asc).map_err(|reason| Error::with_message(ErrorKind::UnsupportedOperation, reason))?;
        let (rate, channels) = backend.output();

        Ok(AdtsStream {
            data,
            cursor: 0,
            backend,
            pending: Vec::new(),
            taken: 0,
            rate,
            channels,
        })
    }

    /// The next frame's payload, header and CRC skipped.
    fn next_frame(&mut self) -> Option<(usize, usize)> {
        let header = self.data.get(self.cursor..self.cursor + 7)?;
        if header[0] != 0xFF || header[1] & 0xF0 != 0xF0 {
            return None;
        }

        let length = ((usize::from(header[3]) & 0x03) << 11) | (usize::from(header[4]) << 3) | (usize::from(header[5]) >> 5);
        let header_length = if header[1] & 0x01 == 0 { 9 } else { 7 };

        let start = self.cursor + header_length;
        let end = self.cursor + length;
        if length < header_length || end > self.data.len() {
            return None;
        }

        self.cursor = end;
        Some((start, end))
    }
}

impl<S: SampleType> AudioStream<S> for AdtsStream {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn source_format(&self) -> SampleFormat {
        SampleFormat::F32
    }

    fn read(&mut self, out: &mut [S]) -> Result<usize, Error> {
        while self.taken >= self.pending.len() {
            let Some((start, end)) = self.next_frame() else {
                return Ok(0);
            };

            self.pending.clear();
            self.taken = 0;
            let frame = self.data[start..end].to_vec();
            self.backend
                .decode(&frame, &mut self.pending)
                .map_err(|reason| Error::with_message(ErrorKind::Other, reason))?;
        }

        let channels = usize::from(self.channels.max(1));
        let available = self.pending.len() - self.taken;
        let count = available.min(out.len()) / channels * channels;

        for (slot, sample) in out.iter_mut().zip(&self.pending[self.taken..self.taken + count]) {
            *slot = S::from_f32(*sample);
        }
        self.taken += count;

        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_audio_specific_config_gives_up_its_fields() {
        // AAC LC, 44.1 kHz, stereo.
        assert_eq!(
            Config::parse(&[0x12, 0x10]),
            Some(Config {
                object_type: 2,
                sample_rate: 44_100,
                channels: 2
            })
        );
    }

    #[test]
    fn an_adts_header_becomes_an_audio_specific_config() {
        // LC, 48 kHz (index 3), stereo, no CRC.
        let header = [0xFF, 0xF1, 0x4C, 0x80, 0x01, 0x7F, 0xFC];
        let (config, asc) = Config::from_adts(&header).unwrap();

        assert_eq!((config.object_type, config.sample_rate, config.channels), (2, 48_000, 2));
        assert_eq!(Config::parse(&asc), Some(config));
    }
}
