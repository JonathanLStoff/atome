//! The VST3 host: `truce-rack-vst3` 1.1.5, carried in-tree (as `au_host` is).
//!
//! Copied from <https://github.com/truce-audio/truce-rack> (crate
//! `truce-rack-vst3`, MIT OR Apache-2.0, by the truce-rack authors) with three
//! changes, each marked `atome:` where it is:
//!
//! - **Drop order.** `Vst3Plugin` declared its loaded module before its COM
//!   pointers, so the library was unloaded first and releasing the plugin
//!   called into it: every dropped plugin crashed. The module is now last.
//! - **`InitDll` / `ExitDll`.** Windows never called the VST3 module entry and
//!   exit, so plugins that set up there (Native Instruments' Raum, Driver, Solid
//!   Bus Comp) crashed on first use. Both are called now.
//! - **[`scan_bundle`].** `scan_path` reads a folder of bundles; a bundle's own
//!   path found nothing (a `.vst3` is a file on Windows, a folder elsewhere).
//!   `scan_bundle` lists the classes of one bundle.
//!
//! Kept in-tree for good: upstream is only followed for security fixes.
//!
//! VST3 host implementation for the truce-rack framework.
//!
//! Built on the community `vst3` Rust bindings — no Steinberg SDK
//! submodule, no cmake. A fresh checkout builds in seconds.
//!
//! # Lifecycle
//!
//! Each `.vst3` bundle exports `GetPluginFactory` (and on macOS
//! `bundleEntry` / `bundleExit`). Loading walks:
//!
//! 1. `dlopen` the binary inside the bundle.
//! 2. `bundleEntry()` on macOS (no-op elsewhere).
//! 3. `GetPluginFactory()` → `IPluginFactory`.
//! 4. `createInstance(cid, IComponent::IID)` → `IComponent`.
//! 5. `IPluginBase::initialize(host_context)` on the component.
//! 6. Cast to `IAudioProcessor`.
//! 7. `IComponent::getControllerClassId(&mut cid2)` — if separate,
//!    `factory.createInstance(cid2, IEditController::IID)` and
//!    initialize it. Otherwise cast the component itself.
//! 8. Connect the component and controller `IConnectionPoint`s
//!    (separate-controller case only).
//!
//! Activate calls `setBusArrangements`, `setupProcessing`,
//! `setActive(true)`, `setProcessing(true)`. Deactivate reverses.

use truce_rack_core::buffer::AudioBuffer;
use truce_rack_core::bus::BusLayout;
use truce_rack_core::error::{Error, Result};
use truce_rack_core::events::EventList;
use truce_rack_core::info::{ParameterInfo, PluginCategory, PluginInfo, PresetInfo};
use truce_rack_core::plugin::{Plugin, PluginCore, ProcessContext, ProcessStatus};
use truce_rack_core::scanner::PluginScanner;
use truce_rack_core::transport::TransportInfo;
use truce_rack_core::wrapper::run_audio_block_with;

use std::path::{Path, PathBuf};
use std::ptr;

use vst3::Steinberg::Vst::{
    AudioBusBuffers, AudioBusBuffers__type0, Event, Event_::EventTypes_, Event__type0,
    IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentTrait, IConnectionPoint,
    IComponentHandler, IComponentHandlerTrait, IConnectionPointTrait, IEditController,
    IAttributeList, IAttributeListTrait, IEditControllerTrait, IEventList, IEventListTrait,
    IHostApplication, IHostApplicationTrait, IMessage, IMessageTrait,
    IParamValueQueue, IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait, ParamID,
    ParamValue, String128, NoteOffEvent, NoteOnEvent, ParameterInfo as Vst3ParameterInfo,
    ParameterInfo_::ParameterFlags_, PolyPressureEvent, ProcessContext as Vst3ProcessContext,
    ProcessContext_::StatesAndFlags_, ProcessData, ProcessModes_, ProcessSetup,
    SymbolicSampleSizes_, ViewType,
};
use vst3::Steinberg::{
    IBStream, IBStreamTrait, IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewTrait, IPluginBaseTrait, IPluginFactory,
    IPluginFactoryTrait, PClassInfo, PClassInfo_, TUID, ViewRect, kPlatformTypeHWND,
    kPlatformTypeNSView, kPlatformTypeX11EmbedWindowID, kNoInterface, kNotImplemented, kResultFalse, kResultOk,
    kResultTrue,
};
use vst3::{Class, ComPtr, ComRef, ComWrapper, Interface};

/// Format identifier used on returned [`PluginInfo`].
pub const FORMAT: &str = "vst3";

/// Bundle directory suffix every VST3 plugin uses.
pub const VST3_EXTENSION: &str = ".vst3";

/// Symbol name `GetPluginFactory` plugins export. Same on every OS.
const GET_FACTORY_SYMBOL: &[u8] = b"GetPluginFactory\0";

/// macOS-only bundle entry point.
#[cfg(target_os = "macos")]
const BUNDLE_ENTRY_SYMBOL: &[u8] = b"bundleEntry\0";

/// macOS-only bundle exit point.
#[cfg(target_os = "macos")]
const BUNDLE_EXIT_SYMBOL: &[u8] = b"bundleExit\0";

/// Stereo speaker arrangement = `kSpeakerL | kSpeakerR`. Defined
/// here to avoid a `kSpeaker*` import dance.
const STEREO_ARRANGEMENT: u64 = 0x03;

/// VST3 scanner.
#[derive(Debug, Default)]
pub struct Vst3Scanner;

impl Vst3Scanner {
    /// Construct a default scanner.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl PluginScanner for Vst3Scanner {
    type Plugin = Vst3Plugin;

    fn scan(&self) -> Result<Vec<PluginInfo>> {
        let mut out = Vec::new();
        for dir in default_vst3_paths() {
            if dir.exists() {
                scan_dir_into(&dir, &mut out);
            }
        }
        Ok(out)
    }

    fn scan_path(&self, path: &Path) -> Result<Vec<PluginInfo>> {
        let mut out = Vec::new();
        if path.exists() {
            scan_dir_into(path, &mut out);
        }
        Ok(out)
    }

    fn load(&self, info: &PluginInfo) -> Result<Self::Plugin> {
        Vst3Plugin::load_from(info)
    }
}

/// Standard VST3 install locations for the current OS.
#[must_use]
pub fn default_vst3_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let mut user = PathBuf::from(home);
        #[cfg(target_os = "macos")]
        user.push("Library/Audio/Plug-Ins/VST3");
        #[cfg(target_os = "linux")]
        user.push(".vst3");
        #[cfg(target_os = "windows")]
        user.push("AppData/Local/Programs/Common/VST3");
        out.push(user);
    }
    #[cfg(target_os = "macos")]
    out.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
    #[cfg(target_os = "linux")]
    out.push(PathBuf::from("/usr/lib/vst3"));
    #[cfg(target_os = "windows")]
    {
        if let Some(pf) = std::env::var_os("CommonProgramFiles") {
            let mut p = PathBuf::from(pf);
            p.push("VST3");
            out.push(p);
        }
    }
    out
}

/// atome: the plugin classes of the one bundle at `bundle` (a `.vst3` file or folder).
///
/// # Errors
///
/// The bundle will not load, or has no plugin factory.
pub fn scan_bundle(bundle: &Path) -> Result<Vec<PluginInfo>> {
    let mut out = Vec::new();
    scan_bundle_into(bundle, &mut out)?;
    Ok(out)
}

fn scan_dir_into(dir: &Path, out: &mut Vec<PluginInfo>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.ends_with(VST3_EXTENSION) {
            continue;
        }
        if let Err(err) = scan_bundle_into(&path, out) {
            eprintln!("[truce-rack-vst3] skipping {}: {err}", path.display());
        }
    }
}

fn scan_bundle_into(bundle: &Path, out: &mut Vec<PluginInfo>) -> Result<()> {
    let module = unsafe { LoadedModule::open(bundle) }?;
    let factory = module.factory()?;
    let count = unsafe { factory.countClasses() };
    let mut info = empty_pclass_info();
    for idx in 0..count {
        if unsafe { factory.getClassInfo(idx, &raw mut info) } != kResultOk {
            continue;
        }
        let category = char8_array_to_string(&info.category);
        if category != "Audio Module Class" {
            continue;
        }
        let name = char8_array_to_string(&info.name);
        out.push(PluginInfo {
            name,
            vendor: String::new(),
            version: 0,
            category: PluginCategory::Effect,
            path: bundle.to_path_buf(),
            unique_id: tuid_to_hex(&info.cid),
            format: FORMAT,
            has_editor: false,
            accepts_midi: false,
        });
    }
    Ok(())
}

fn empty_pclass_info() -> PClassInfo {
    PClassInfo {
        cid: [0; 16],
        cardinality: 0,
        category: [0; PClassInfo_::kCategorySize as usize],
        name: [0; PClassInfo_::kNameSize as usize],
    }
}

#[allow(clippy::cast_sign_loss)]
fn char8_array_to_string(array: &[i8]) -> String {
    // VST3 char8 is signed on Apple, unsigned elsewhere; the cast
    // preserves bit pattern.
    let bytes: Vec<u8> = array
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[allow(clippy::cast_sign_loss)]
fn tuid_to_hex(cid: &[i8; 16]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(32);
    for &b in cid {
        let _ = write!(s, "{:02x}", b as u8);
    }
    s
}

#[allow(clippy::cast_possible_wrap)]
fn hex_to_tuid(hex: &str) -> Option<TUID> {
    if hex.len() != 32 {
        return None;
    }
    let mut out: TUID = [0; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()? as i8;
    }
    Some(out)
}

#[cfg(target_os = "macos")]
mod mac {
    //! `CFBundle`-backed loader for VST3 bundles on macOS.
    //!
    //! Plugins like Surge XT Effects call into `CFPlugin` during
    //! `bundleEntry` (specifically `CFPlugInRegisterFactories`,
    //! which lives behind the `AddInstanceForFactory` log line).
    //! Those APIs only work when the dylib was loaded through a
    //! registered `CFBundle` — raw `dlopen` leaves the bundle
    //! unknown to CoreFoundation and the plugin dereferences
    //! garbage. Going through `CFBundleLoadExecutable` gives the
    //! plugin the context it expects.

    use std::path::{Path, PathBuf};

    use core_foundation::base::TCFType;
    use core_foundation::bundle::CFBundle;
    use core_foundation::string::CFString;
    use core_foundation::url::{CFURL, kCFURLPOSIXPathStyle};

    use truce_rack_core::error::{Error, Result};

    pub(super) struct MacBundle {
        bundle: CFBundle,
        path: PathBuf,
    }

    impl MacBundle {
        pub(super) fn open(bundle_path: &Path) -> Result<Self> {
            let path_str = bundle_path.to_str().ok_or_else(|| Error::LoadFailed {
                path: bundle_path.to_path_buf(),
                reason: "bundle path is not valid UTF-8".into(),
            })?;
            let cf_path = CFString::new(path_str);
            // CFURLCreateWithFileSystemPath with `isDirectory = true`
            // is what every reference VST3 host uses on macOS — a
            // bundle URL has to be flagged as a directory or
            // CFBundleCreate silently picks the wrong layout.
            let url = unsafe {
                use core_foundation_sys::base::kCFAllocatorDefault;
                use core_foundation_sys::url::CFURLCreateWithFileSystemPath;
                let raw = CFURLCreateWithFileSystemPath(
                    kCFAllocatorDefault,
                    cf_path.as_concrete_TypeRef(),
                    kCFURLPOSIXPathStyle,
                    1,
                );
                if raw.is_null() {
                    return Err(Error::LoadFailed {
                        path: bundle_path.to_path_buf(),
                        reason: "CFURLCreateWithFileSystemPath returned NULL".into(),
                    });
                }
                CFURL::wrap_under_create_rule(raw)
            };

            let bundle = CFBundle::new(url).ok_or_else(|| Error::LoadFailed {
                path: bundle_path.to_path_buf(),
                reason: "CFBundleCreate returned NULL".into(),
            })?;

            // CFBundleLoadExecutable must run before any function
            // pointer lookup. Returns true on success.
            let loaded = unsafe {
                use core_foundation_sys::bundle::CFBundleLoadExecutable;
                CFBundleLoadExecutable(bundle.as_concrete_TypeRef()) != 0
            };
            if !loaded {
                return Err(Error::LoadFailed {
                    path: bundle_path.to_path_buf(),
                    reason: "CFBundleLoadExecutable failed".into(),
                });
            }

            Ok(Self {
                bundle,
                path: bundle_path.to_path_buf(),
            })
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        /// The raw `CFBundleRef`, type-erased to `*mut c_void` so
        /// the call site doesn't need to depend on
        /// `core-foundation-sys`. Hand this to `bundleEntry` —
        /// it's the host's identity to the plugin.
        pub(super) fn raw(&self) -> *mut std::ffi::c_void {
            self.bundle.as_concrete_TypeRef().cast::<std::ffi::c_void>()
        }

        /// Look up an exported symbol. `name` may end in a trailing
        /// NUL (we strip it before handing the text to `CFString`).
        pub(super) unsafe fn function_ptr(&self, name: &[u8]) -> Option<*mut std::ffi::c_void> {
            let name = match name.split_last() {
                Some((0, rest)) => rest,
                _ => name,
            };
            let name_str = std::str::from_utf8(name).ok()?;
            let cf_name = CFString::new(name_str);
            let ptr = unsafe {
                use core_foundation_sys::bundle::CFBundleGetFunctionPointerForName;
                CFBundleGetFunctionPointerForName(
                    self.bundle.as_concrete_TypeRef(),
                    cf_name.as_concrete_TypeRef(),
                )
            };
            if ptr.is_null() {
                None
            } else {
                Some(ptr.cast_mut().cast::<std::ffi::c_void>())
            }
        }
    }

    // SAFETY: CFBundle is reference-counted by CoreFoundation; we
    // hold one owned reference and CoreFoundation itself is
    // thread-safe for read access. The Drop impl releases the
    // CFBundle but deliberately never calls
    // `CFBundleUnloadExecutable` — VST3 plugins leave Objective-C
    // class registrations and runloop callbacks pointing into the
    // dylib, and unloading invalidates them. Same "don't dlclose"
    // discipline truce-loader follows on the plugin side.
    unsafe impl Send for MacBundle {}
}

/// Per-platform VST3 bundle layout. Linux uses
/// `Contents/<arch>-linux/<stem>.so`; Windows uses
/// `Contents/<arch>-win/<stem>.vst3`. macOS goes through CFBundle
/// instead (see [`mac::MacBundle`]) so its binary lookup lives
/// there.
#[cfg(not(target_os = "macos"))]
fn bundle_binary_path(bundle: &Path) -> PathBuf {
    let stem = bundle
        .file_stem()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_default();
    #[cfg(target_os = "linux")]
    {
        if bundle.is_dir() {
            let arch_dir = format!("{}-linux", std::env::consts::ARCH);
            let mut binary = stem.clone();
            binary.push(".so");
            return bundle.join("Contents").join(arch_dir).join(binary);
        }
    }
    #[cfg(target_os = "windows")]
    {
        if bundle.is_dir() {
            let arch_dir = format!("{}-win", std::env::consts::ARCH);
            let mut binary = stem.clone();
            binary.push(".vst3");
            return bundle.join("Contents").join(arch_dir).join(binary);
        }
    }
    let _ = stem;
    bundle.to_path_buf()
}

/// RAII wrapper around the loaded module. On macOS this is a real
/// `CFBundle` so the plugin's `bundleEntry` sees the CFPlugin /
/// CFBundleGetIdentifier context it expects (raw dlopen crashes
/// some bundles — Surge XT Effects calls into CFPlugin's
/// `AddInstanceForFactory` during init). On Linux / Windows the
/// underlying file is a plain dynamic library; `libloading` is
/// enough.
#[cfg(not(target_os = "macos"))]
struct LoadedModule {
    library: libloading::Library,
    /// atome: `InitDll` ran and returned true, so `ExitDll` runs on drop (Windows).
    entered: bool,
}

#[cfg(not(target_os = "macos"))]
impl Drop for LoadedModule {
    fn drop(&mut self) {
        #[cfg(target_os = "windows")]
        if self.entered {
            if let Ok(exit) = unsafe { self.library.get::<unsafe extern "system" fn() -> bool>(b"ExitDll\0") } {
                unsafe {
                    exit();
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
struct LoadedModule {
    bundle: mac::MacBundle,
    entered: bool,
}

#[cfg(not(target_os = "macos"))]
impl LoadedModule {
    unsafe fn open(bundle: &Path) -> Result<Self> {
        let binary = bundle_binary_path(bundle);
        let library =
            unsafe { libloading::Library::new(&binary) }.map_err(|e| Error::LoadFailed {
                path: bundle.to_path_buf(),
                reason: format!("dlopen: {e}"),
            })?;
        // atome: the VST3 SDK's Windows module entry, called once after loading, before the factory.
        #[cfg(target_os = "windows")]
        let entered = match unsafe { library.get::<unsafe extern "system" fn() -> bool>(b"InitDll\0") } {
            Ok(init) => unsafe { init() },
            Err(_) => false,
        };
        #[cfg(not(target_os = "windows"))]
        let entered = false;
        Ok(Self { library, entered })
    }

    fn factory(&self) -> Result<ComPtr<IPluginFactory>> {
        let get_factory: libloading::Symbol<'_, unsafe extern "C" fn() -> *mut IPluginFactory> =
            unsafe { self.library.get(GET_FACTORY_SYMBOL) }.map_err(|e| Error::LoadFailed {
                path: PathBuf::new(),
                reason: format!("missing GetPluginFactory: {e}"),
            })?;
        let ptr = unsafe { get_factory() };
        let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(ptr) }.ok_or_else(|| {
            Error::LoadFailed {
                path: PathBuf::new(),
                reason: "GetPluginFactory returned NULL".into(),
            }
        })?;
        Ok(factory)
    }
}

#[cfg(target_os = "macos")]
impl LoadedModule {
    unsafe fn open(bundle: &Path) -> Result<Self> {
        let mac_bundle = mac::MacBundle::open(bundle)?;

        // VST3 macOS spec: bundleEntry takes the CFBundleRef the
        // host loaded the plugin from. Surge XT Effects (and any
        // bundle that touches CFPlugin/AU registration in init)
        // dereferences that argument; passing nothing crashes
        // inside CFRetain on a register that happened to be
        // non-zero. Reference impl: Steinberg's `module_mac.mm`.
        let entered = unsafe {
            match mac_bundle.function_ptr(BUNDLE_ENTRY_SYMBOL) {
                Some(ptr) => {
                    let entry: unsafe extern "C" fn(*mut std::ffi::c_void) -> bool =
                        std::mem::transmute(ptr);
                    entry(mac_bundle.raw())
                }
                None => false,
            }
        };

        Ok(Self {
            bundle: mac_bundle,
            entered,
        })
    }

    fn factory(&self) -> Result<ComPtr<IPluginFactory>> {
        let raw = unsafe {
            self.bundle
                .function_ptr(GET_FACTORY_SYMBOL)
                .ok_or_else(|| Error::LoadFailed {
                    path: self.bundle.path().to_path_buf(),
                    reason: "missing GetPluginFactory".into(),
                })?
        };
        let get_factory: unsafe extern "C" fn() -> *mut IPluginFactory =
            unsafe { std::mem::transmute(raw) };
        let ptr = unsafe { get_factory() };
        let factory = unsafe { ComPtr::<IPluginFactory>::from_raw(ptr) }.ok_or_else(|| {
            Error::LoadFailed {
                path: self.bundle.path().to_path_buf(),
                reason: "GetPluginFactory returned NULL".into(),
            }
        })?;
        Ok(factory)
    }
}

#[cfg(target_os = "macos")]
impl Drop for LoadedModule {
    fn drop(&mut self) {
        // bundleExit is the symmetric partner to bundleEntry and
        // takes no arguments per the VST3 macOS spec — only
        // bundleEntry sees the CFBundleRef. Most plugins are
        // no-ops; some unregister CFPlugin factories here.
        // atome: no let-chain (edition 2021).
        if self.entered {
            if let Some(ptr) = unsafe { self.bundle.function_ptr(BUNDLE_EXIT_SYMBOL) } {
                let exit: unsafe extern "C" fn() -> bool = unsafe { std::mem::transmute(ptr) };
                unsafe {
                    exit();
                }
            }
        }
    }
}

/// One loaded VST3 plugin instance.
///
/// Holds three COM smart pointers — component, audio processor,
/// edit controller — plus the dlopen handle that keeps the
/// underlying dylib mapped. When `Drop`s, the COM pointers
/// release their objects which triggers `terminate()` and
/// component disposal.
pub struct Vst3Plugin {
    info: PluginInfo,
    layouts: Vec<BusLayout>,
    active_layout: Option<BusLayout>,

    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: ComPtr<IEditController>,
    /// `true` when controller and component are *different* COM
    /// objects (separate-controller architecture) and we've wired
    /// their connection points.
    separate_controller: bool,
    component_cp: Option<ComPtr<IConnectionPoint>>,
    controller_cp: Option<ComPtr<IConnectionPoint>>,

    param_count: usize,
    processing: bool,

    /// Cached `IPlugView` for the plugin's editor. Created on
    /// `open()`, released on `close()` / Drop.
    view: Option<ComPtr<IPlugView>>,
    editor_open: bool,

    /// atome: the host side the plugin talks to (its context, the component handler its window
    /// reports edits through, and the frame its view sits in).
    host: ComWrapper<Host>,

    // atome: hold the module open for the lifetime of the instance, and drop it last: fields drop
    // in order, and the COM pointers above must be released while the library is still loaded.
    _module: LoadedModule,
}

impl Vst3Plugin {
    fn load_from(info: &PluginInfo) -> Result<Self> {
        let module = unsafe { LoadedModule::open(&info.path) }?;
        let factory = module.factory()?;

        let cid = hex_to_tuid(&info.unique_id).ok_or_else(|| Error::LoadFailed {
            path: info.path.clone(),
            reason: format!("could not parse VST3 unique_id {:?}", info.unique_id),
        })?;

        // Create IComponent.
        let component_ptr =
            unsafe { create_instance::<IComponent>(&factory, &cid) }.ok_or_else(|| {
                Error::LoadFailed {
                    path: info.path.clone(),
                    reason: "factory.createInstance(IComponent) returned NULL".into(),
                }
            })?;
        let component = component_ptr;

        // Initialize the component with the host's context (atome: some plugins, Native
        // Instruments' among them, crash on a NULL one).
        let host = ComWrapper::new(Host::default());
        let context = host.as_com_ref::<IHostApplication>().map_or(ptr::null_mut(), |host| host.as_ptr().cast());
        if unsafe { component.initialize(context) } != kResultOk {
            return Err(Error::LoadFailed {
                path: info.path.clone(),
                reason: "IComponent::initialize returned non-OK".into(),
            });
        }

        // Cast to IAudioProcessor.
        let processor = component
            .as_com_ref()
            .cast::<IAudioProcessor>()
            .ok_or_else(|| Error::LoadFailed {
                path: info.path.clone(),
                reason: "component does not implement IAudioProcessor".into(),
            })?;

        // Look up the separate-controller class id; if present we
        // create a separate controller and connect to it. Otherwise
        // the component is itself the controller.
        let mut controller_cid: TUID = [0; 16];
        let cls_id_status = unsafe { component.getControllerClassId(&raw mut controller_cid) };
        let (controller, separate_controller) = if cls_id_status == kResultTrue {
            let ctrl_ptr = unsafe { create_instance::<IEditController>(&factory, &controller_cid) }
                .ok_or_else(|| Error::LoadFailed {
                    path: info.path.clone(),
                    reason: "factory.createInstance(IEditController) returned NULL".into(),
                })?;
            if unsafe { ctrl_ptr.initialize(context) } != kResultOk {
                return Err(Error::LoadFailed {
                    path: info.path.clone(),
                    reason: "IEditController::initialize returned non-OK".into(),
                });
            }
            (ctrl_ptr, true)
        } else {
            // Single-component plugin — controller and component
            // share an object. Cast through queryInterface so the
            // refcount is correct.
            let ctrl = component
                .as_com_ref()
                .cast::<IEditController>()
                .ok_or_else(|| Error::LoadFailed {
                    path: info.path.clone(),
                    reason:
                        "component is not its own controller and didn't report a controller cid"
                            .into(),
                })?;
            (ctrl, false)
        };

        // Optional: connect the two connection points so the
        // plugin's component and controller can talk to each
        // other (param/audio synchronisation). Best-effort —
        // some plugins skip these even when the controller is
        // separate.
        let (component_cp, controller_cp) = if separate_controller {
            let cp_a = component.as_com_ref().cast::<IConnectionPoint>();
            let cp_b = controller.as_com_ref().cast::<IConnectionPoint>();
            if let (Some(a), Some(b)) = (&cp_a, &cp_b) {
                unsafe {
                    a.connect(b.as_com_ref().as_ptr().cast());
                    b.connect(a.as_com_ref().as_ptr().cast());
                }
            }
            (cp_a, cp_b)
        } else {
            (None, None)
        };

        // Edits made in the plugin's own window come back through the component handler.
        if let Some(handler) = host.as_com_ref::<IComponentHandler>() {
            unsafe { controller.setComponentHandler(handler.as_ptr()) };
        }

        let param_count_raw = unsafe { controller.getParameterCount() }.max(0);
        #[allow(clippy::cast_sign_loss)]
        let param_count = param_count_raw as usize;

        let mut info = info.clone();
        // The editor exists if `createView("editor")` returns a
        // non-null view. We probe by creating + releasing it once
        // at load time. (Some plugins are slow to create the view;
        // a heavier-weight host would defer this to first open.)
        info.has_editor = unsafe { create_editor_view(&controller) }.is_some();

        Ok(Self {
            info,
            layouts: vec![BusLayout::stereo()],
            active_layout: None,
            _module: module,
            component,
            processor,
            controller,
            separate_controller,
            component_cp,
            controller_cp,
            param_count,
            processing: false,
            view: None,
            editor_open: false,
            host,
        })
    }
}

/// Call `IEditController::createView("editor")` and wrap the raw
/// pointer. Returns `None` when the plugin has no editor.
unsafe fn create_editor_view(controller: &ComPtr<IEditController>) -> Option<ComPtr<IPlugView>> {
    let raw = unsafe { controller.createView(ViewType::kEditor) };
    if raw.is_null() {
        return None;
    }
    unsafe { ComPtr::<IPlugView>::from_raw(raw) }
}

fn platform_type_for_handle(
    handle: truce_rack_core::editor::WindowHandle,
) -> (*const i8, *mut std::ffi::c_void) {
    use truce_rack_core::editor::WindowHandle;
    match handle {
        WindowHandle::NSView(p) => (kPlatformTypeNSView, p),
        WindowHandle::HWND(p) => (kPlatformTypeHWND, p),
        WindowHandle::X11(id) => (kPlatformTypeX11EmbedWindowID, id as *mut std::ffi::c_void),
    }
}

/// Run `factory.createInstance` for `I` and wrap the result as a
/// `ComPtr<I>`. The factory call uses the interface's `IID`
/// directly so the plugin returns the correct `*mut c_void`.
unsafe fn create_instance<I>(factory: &ComPtr<IPluginFactory>, cid: &TUID) -> Option<ComPtr<I>>
where
    I: vst3::Interface,
{
    let mut obj: *mut std::ffi::c_void = ptr::null_mut();
    // `com_scrape_types::Guid` and `TUID` share the same
    // `[i8; 16]` layout — reinterpret to avoid an extra copy.
    let iid_bytes: &TUID =
        unsafe { &*(std::ptr::from_ref::<vst3::com_scrape_types::Guid>(&I::IID).cast::<TUID>()) };
    let cid_ptr = cid.as_ptr();
    let iid_ptr = iid_bytes.as_ptr();
    if unsafe { factory.createInstance(cid_ptr, iid_ptr, &raw mut obj) } != kResultOk
        || obj.is_null()
    {
        return None;
    }
    unsafe { ComPtr::<I>::from_raw(obj.cast()) }
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        if self.processing {
            unsafe { self.processor.setProcessing(0) };
        }
        if self.active_layout.is_some() {
            unsafe { self.component.setActive(0) };
        }
        if let (Some(a), Some(b)) = (&self.component_cp, &self.controller_cp) {
            unsafe {
                a.disconnect(b.as_com_ref().as_ptr().cast());
                b.disconnect(a.as_com_ref().as_ptr().cast());
            }
        }
        if self.separate_controller {
            unsafe { self.controller.terminate() };
        }
        unsafe { self.component.terminate() };
    }
}

impl PluginCore for Vst3Plugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
    fn active_layout(&self) -> Option<&BusLayout> {
        self.active_layout.as_ref()
    }
    fn supported_layouts(&self) -> &[BusLayout] {
        &self.layouts
    }

    fn parameter_count(&self) -> usize {
        self.param_count
    }

    fn parameter_info(&self, index: usize) -> Result<ParameterInfo> {
        if index >= self.param_count {
            return Err(Error::InvalidParameter(index));
        }
        let mut info = empty_parameter_info();
        let i32_index = i32::try_from(index).map_err(|_| Error::InvalidParameter(index))?;
        if unsafe { self.controller.getParameterInfo(i32_index, &raw mut info) } != kResultOk {
            return Err(Error::InvalidParameter(index));
        }
        Ok(vst3_param_info_to_rack(&info))
    }

    fn parameter_value(&self, index: usize) -> Result<f64> {
        if index >= self.param_count {
            return Err(Error::InvalidParameter(index));
        }
        let mut info = empty_parameter_info();
        let i32_index = i32::try_from(index).map_err(|_| Error::InvalidParameter(index))?;
        if unsafe { self.controller.getParameterInfo(i32_index, &raw mut info) } != kResultOk {
            return Err(Error::InvalidParameter(index));
        }
        Ok(unsafe { self.controller.getParamNormalized(info.id) })
    }

    fn parameter_value_string(&self, index: usize, _value: f64) -> Result<String> {
        // VST3 exposes IEditController::getParamStringByValue —
        // wiring it requires a TChar (UTF-16) buffer round-trip.
        // Tracked as a follow-on to avoid pulling in a wide-char
        // dep here.
        let _ = index;
        Err(Error::Other("vst3 parameter_value_string TODO".into()))
    }

    fn set_parameter(&mut self, index: usize, value: f64) -> Result<()> {
        if index >= self.param_count {
            return Err(Error::InvalidParameter(index));
        }
        let mut info = empty_parameter_info();
        let i32_index = i32::try_from(index).map_err(|_| Error::InvalidParameter(index))?;
        if unsafe { self.controller.getParameterInfo(i32_index, &raw mut info) } != kResultOk {
            return Err(Error::InvalidParameter(index));
        }
        let clamped = value.clamp(0.0, 1.0);
        if unsafe { self.controller.setParamNormalized(info.id, clamped) } != kResultOk {
            return Err(Error::Other(
                "IEditController::setParamNormalized failed".into(),
            ));
        }
        // atome: the processor hears it from the next block.
        self.host.edit(info.id, clamped);
        Ok(())
    }

    fn preset_count(&self) -> usize {
        // VST3 exposes presets via the `IUnitInfo` interface,
        // wired in a follow-on. Treat as zero for now.
        0
    }
    fn preset_info(&self, index: usize) -> Result<PresetInfo> {
        Err(Error::InvalidParameter(index))
    }
    fn load_preset(&mut self, _preset_number: i32) -> Result<()> {
        Err(Error::Other("vst3 preset loading not yet wired".into()))
    }

    fn save_state(&self) -> Result<Vec<u8>> {
        let stream = ComWrapper::new(MemoryStream::default());
        let stream_ptr = stream
            .to_com_ptr::<IBStream>()
            .ok_or_else(|| Error::Other("MemoryStream missing IBStream IID".into()))?;
        let status = unsafe { self.component.getState(stream_ptr.as_ptr()) };
        if status != kResultOk {
            return Err(Error::Other(format!(
                "IComponent::getState returned {status}"
            )));
        }
        // atome: the borrow ends before `stream` does (edition 2021 drops temporaries later).
        let data = stream.data.borrow().clone();
        Ok(data)
    }

    fn load_state(&mut self, bytes: &[u8]) -> Result<()> {
        let stream = ComWrapper::new(MemoryStream {
            data: std::cell::RefCell::new(bytes.to_vec()),
            position: std::cell::Cell::new(0),
        });
        let stream_ptr = stream
            .to_com_ptr::<IBStream>()
            .ok_or_else(|| Error::Other("MemoryStream missing IBStream IID".into()))?;
        let status = unsafe { self.component.setState(stream_ptr.as_ptr()) };
        if status != kResultOk {
            return Err(Error::Other(format!(
                "IComponent::setState returned {status}"
            )));
        }
        // atome: the controller (and so the plugin's window) shows the restored values.
        stream.position.set(0);
        let _ = unsafe { self.controller.setComponentState(stream_ptr.as_ptr()) };
        Ok(())
    }

    fn activate(
        &mut self,
        layout: BusLayout,
        sample_rate: f64,
        max_block_size: usize,
    ) -> Result<()> {
        let mut input_arr = STEREO_ARRANGEMENT;
        let mut output_arr = STEREO_ARRANGEMENT;
        let _ = unsafe {
            self.processor
                .setBusArrangements(&raw mut input_arr, 1, &raw mut output_arr, 1)
        };
        // atome: the main audio buses on, as the SDK asks of a host (some plugins crash
        // processing a bus that is off). Best-effort: a plugin with no input bus says no.
        unsafe {
            use vst3::Steinberg::Vst::{BusDirections_, MediaTypes_};
            self.component.activateBus(MediaTypes_::kAudio as i32, BusDirections_::kInput as i32, 0, 1);
            self.component.activateBus(MediaTypes_::kAudio as i32, BusDirections_::kOutput as i32, 0, 1);
        }

        let mut setup = ProcessSetup {
            #[allow(clippy::cast_possible_wrap)]
            processMode: ProcessModes_::kRealtime as i32,
            #[allow(clippy::cast_possible_wrap)]
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: i32::try_from(max_block_size).unwrap_or(i32::MAX),
            sampleRate: sample_rate,
        };
        if unsafe { self.processor.setupProcessing(&raw mut setup) } != kResultOk {
            return Err(Error::Other(
                "IAudioProcessor::setupProcessing failed".into(),
            ));
        }
        if unsafe { self.component.setActive(1) } != kResultOk {
            return Err(Error::Other("IComponent::setActive(true) failed".into()));
        }
        // atome: the SDK lets a plugin answer kNotImplemented here (iZotope's do); only a real
        // failure stops the load.
        let status = unsafe { self.processor.setProcessing(1) };
        if status != kResultOk && status != kNotImplemented {
            return Err(Error::Other(format!(
                "IAudioProcessor::setProcessing(true) returned {status}"
            )));
        }
        self.processing = true;
        self.active_layout = Some(layout);
        Ok(())
    }

    fn deactivate(&mut self) {
        if self.processing {
            unsafe { self.processor.setProcessing(0) };
            self.processing = false;
        }
        if self.active_layout.is_some() {
            unsafe { self.component.setActive(0) };
        }
        self.active_layout = None;
    }
    fn is_active(&self) -> bool {
        self.active_layout.is_some()
    }

    fn editor(&mut self) -> Option<&mut dyn truce_rack_core::editor::PluginEditor> {
        if !self.info.has_editor {
            return None;
        }
        Some(self)
    }
}

impl truce_rack_core::editor::PluginEditor for Vst3Plugin {
    fn open(
        &mut self,
        parent: truce_rack_core::editor::WindowHandle,
        _scale: f64,
    ) -> truce_rack_core::error::Result<()> {
        if self.editor_open {
            return Ok(());
        }
        let view = unsafe { create_editor_view(&self.controller) }
            .ok_or_else(|| Error::Other("IEditController::createView returned NULL".into()))?;
        let (type_str, parent_ptr) = platform_type_for_handle(parent);
        if unsafe { view.isPlatformTypeSupported(type_str) } != kResultOk {
            return Err(Error::Other(
                "IPlugView::isPlatformTypeSupported returned false".into(),
            ));
        }
        // atome: plugins resize through the frame, some while they attach.
        if let Some(frame) = self.host.as_com_ref::<IPlugFrame>() {
            unsafe { view.setFrame(frame.as_ptr()) };
        }
        if unsafe { view.attached(parent_ptr, type_str) } != kResultOk {
            unsafe { view.setFrame(ptr::null_mut()) };
            return Err(Error::Other("IPlugView::attached returned non-OK".into()));
        }        self.view = Some(view);
        self.editor_open = true;
        Ok(())
    }

    fn close(&mut self) {
        if let Some(view) = self.view.take() {
            unsafe { view.removed() };
            unsafe { view.setFrame(ptr::null_mut()) };
        }
        self.editor_open = false;
    }

    fn is_open(&self) -> bool {
        self.editor_open
    }

    fn size(&self) -> Option<(u32, u32)> {
        let view = self.view.as_ref()?;
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if unsafe { view.getSize(&raw mut rect) } != kResultOk {
            return None;
        }
        let w = u32::try_from(rect.right - rect.left).ok()?;
        let h = u32::try_from(rect.bottom - rect.top).ok()?;
        Some((w, h))
    }

    fn is_resizable(&self) -> bool {
        let Some(view) = self.view.as_ref() else {
            return false;
        };
        unsafe { view.canResize() == kResultOk }
    }

    fn set_size(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        let view = self.view.as_ref()?;
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: i32::try_from(width).ok()?,
            bottom: i32::try_from(height).ok()?,
        };
        // Plugins may snap to a constraint via checkSizeConstraint.
        let _ = unsafe { view.checkSizeConstraint(&raw mut rect) };
        if unsafe { view.onSize(&raw mut rect) } != kResultOk {
            return None;
        }
        let w = u32::try_from(rect.right - rect.left).ok()?;
        let h = u32::try_from(rect.bottom - rect.top).ok()?;
        Some((w, h))
    }

    fn show(&mut self) {
        // VST3 attaches once and stays visible — no show/hide
        // separate from attached/removed.
    }

    fn hide(&mut self) {
        // Same as show — VST3 has no distinct hide.
    }
}

fn empty_parameter_info() -> Vst3ParameterInfo {
    Vst3ParameterInfo {
        id: 0,
        title: [0; 128],
        shortTitle: [0; 128],
        units: [0; 128],
        stepCount: 0,
        defaultNormalizedValue: 0.0,
        unitId: 0,
        flags: 0,
    }
}

fn vst3_param_info_to_rack(info: &Vst3ParameterInfo) -> ParameterInfo {
    let name = string128_to_string(&info.title);
    let short_name = string128_to_string(&info.shortTitle);
    let unit = string128_to_string(&info.units);
    let mut flags = truce_rack_core::info::ParameterFlags::empty();
    if info.flags & ParameterFlags_::kIsBypass != 0 {
        flags |= truce_rack_core::info::ParameterFlags::BYPASS;
    }
    if info.flags & ParameterFlags_::kCanAutomate != 0 {
        flags |= truce_rack_core::info::ParameterFlags::AUTOMATABLE;
    }
    if info.flags & ParameterFlags_::kIsHidden != 0 {
        flags |= truce_rack_core::info::ParameterFlags::HIDDEN;
    }
    if info.flags & ParameterFlags_::kIsReadOnly != 0 {
        flags |= truce_rack_core::info::ParameterFlags::READ_ONLY;
    }
    if info.flags & ParameterFlags_::kIsList != 0 {
        flags |= truce_rack_core::info::ParameterFlags::ENUMERATED;
    }
    ParameterInfo {
        id: info.id,
        name,
        short_name,
        unit,
        min: 0.0,
        max: 1.0,
        default: info.defaultNormalizedValue,
        step_count: u32::try_from(info.stepCount).unwrap_or(0),
        flags,
    }
}

/// VST3 `String128` is `[char16; 128]` (UTF-16). Walk the slice
/// until the first NUL and decode to UTF-8.
fn string128_to_string(buf: &[u16; 128]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Translate a host [`TransportInfo`] snapshot into Steinberg's
/// `Vst::ProcessContext`. Only the fields the host reported get
/// their `k…Valid` flag set; everything else stays zero so the
/// plugin treats it as absent.
// `StatesAndFlags` is `c_int` or `c_uint` depending on platform, so
// the `as u32` casts are only redundant on some targets.
#[allow(clippy::unnecessary_cast)]
fn build_vst3_context(t: &TransportInfo, sample_rate: f64) -> Vst3ProcessContext {
    // SAFETY: ProcessContext is a repr(C) aggregate of integers,
    // floats, and small POD sub-structs (Chord, FrameRate). An
    // all-zero value is the valid "no flags set" state; we then
    // fill in the fields the host actually reported.
    let mut ctx: Vst3ProcessContext = unsafe { std::mem::zeroed() };
    ctx.sampleRate = sample_rate;

    let mut state: u32 = 0;
    if let Some(tempo) = t.tempo_bpm {
        ctx.tempo = tempo;
        state |= StatesAndFlags_::kTempoValid as u32;
    }
    if let Some((num, den)) = t.time_signature {
        ctx.timeSigNumerator = i32::try_from(num).unwrap_or(4);
        ctx.timeSigDenominator = i32::try_from(den).unwrap_or(4);
        state |= StatesAndFlags_::kTimeSigValid as u32;
    }
    if let Some(beats) = t.song_position_beats {
        // projectTimeMusic is in quarter notes (TQuarterNotes = f64).
        ctx.projectTimeMusic = beats;
        state |= StatesAndFlags_::kProjectTimeMusicValid as u32;
    }
    if let Some(samples) = t.song_position_samples {
        ctx.projectTimeSamples = samples;
    }
    if let Some(bar) = t.bar_start_beats {
        ctx.barPositionMusic = bar;
        state |= StatesAndFlags_::kBarPositionValid as u32;
    }
    if t.playing {
        state |= StatesAndFlags_::kPlaying as u32;
    }
    if t.recording {
        state |= StatesAndFlags_::kRecording as u32;
    }
    if t.loop_active {
        state |= StatesAndFlags_::kCycleActive as u32;
    }
    ctx.state = state;
    ctx
}

impl Plugin<f32> for Vst3Plugin {
    fn process(
        &mut self,
        buffer: &mut AudioBuffer<'_, f32>,
        events: &EventList,
        context: &mut ProcessContext<'_>,
    ) -> Result<ProcessStatus> {
        if !self.is_active() {
            return Err(Error::NotActivated);
        }
        let frames = buffer.num_frames();

        // Build an IEventList for the plugin's inputEvents slot.
        // We translate truce-rack MIDI events to VST3 Event structs.
        let mut translated = EventBuffer::default();
        for event in events {
            translated.push_rack(event);
        }
        let input_events_wrapper = ComWrapper::new(EventList3 {
            events: std::cell::RefCell::new(translated.events),
        });
        let input_events_ptr = input_events_wrapper
            .to_com_ptr::<IEventList>()
            .ok_or_else(|| Error::Other("EventList3 missing IEventList IID".into()))?;
        let output_events_wrapper = ComWrapper::new(EventList3::default());
        let output_events_ptr = output_events_wrapper
            .to_com_ptr::<IEventList>()
            .ok_or_else(|| Error::Other("EventList3 missing IEventList IID".into()))?;

        // atome: edits since the last block (the plugin's window, set_parameter), at its start.
        let changes = self.host.take_changes();
        let changes_ptr = changes.as_ref().and_then(|changes| changes.to_com_ptr::<IParameterChanges>());

        let main_inputs = buffer.main_inputs();
        let mut input_ptrs: Vec<*mut f32> =
            main_inputs.iter().map(|c| c.as_ptr().cast_mut()).collect();
        let mut input_bus = AudioBusBuffers {
            numChannels: i32::try_from(input_ptrs.len()).unwrap_or(0),
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: input_ptrs.as_mut_ptr(),
            },
        };

        let main_outputs = buffer.main_outputs();
        let mut output_ptrs: Vec<*mut f32> =
            main_outputs.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let mut output_bus = AudioBusBuffers {
            numChannels: i32::try_from(output_ptrs.len()).unwrap_or(0),
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: output_ptrs.as_mut_ptr(),
            },
        };

        // Build the transport context up front so its backing
        // struct outlives the process call.
        let mut process_context = context
            .transport
            .map(|t| build_vst3_context(&t, context.sample_rate));

        let mut data = ProcessData {
            #[allow(clippy::cast_possible_wrap)]
            processMode: ProcessModes_::kRealtime as i32,
            #[allow(clippy::cast_possible_wrap)]
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: i32::try_from(frames).unwrap_or(i32::MAX),
            numInputs: 1,
            numOutputs: 1,
            inputs: &raw mut input_bus,
            outputs: &raw mut output_bus,
            inputParameterChanges: changes_ptr.as_ref().map_or(ptr::null_mut(), ComPtr::as_ptr),
            outputParameterChanges: ptr::null_mut::<IParameterChanges>(),
            inputEvents: input_events_ptr.as_ptr(),
            outputEvents: output_events_ptr.as_ptr(),
            processContext: process_context
                .as_mut()
                .map_or(ptr::null_mut(), std::ptr::from_mut),
        };

        let processor_ptr = self.processor.as_ptr();
        let status = run_audio_block_with::<Vst3Plugin, i32>(FORMAT, -1, || unsafe {
            ((*(*processor_ptr).vtbl).process)(processor_ptr, &raw mut data)
        });
        if status == kResultOk {
            Ok(ProcessStatus::Continue)
        } else {
            Ok(ProcessStatus::Error)
        }
    }
}

// ---------------------------------------------------------------------------
// EventList3 — in-memory IEventList impl for MIDI in/out.
// ---------------------------------------------------------------------------

/// Translation buffer for truce-rack-core `EventList` → VST3 `Event[]`.
#[derive(Default)]
struct EventBuffer {
    events: Vec<Event>,
}

impl EventBuffer {
    fn push_rack(&mut self, event: &truce_rack_core::events::Event) {
        use truce_rack_core::events::{EventBody, MidiData};
        let offset = i32::try_from(event.sample_offset).unwrap_or(i32::MAX);
        let EventBody::Midi(body) = event.body else {
            return;
        };
        let header = |type_: u16| Event {
            busIndex: 0,
            sampleOffset: offset,
            ppqPosition: 0.0,
            flags: 0,
            r#type: type_,
            __field0: Event__type0 {
                noteOn: NoteOnEvent {
                    channel: 0,
                    pitch: 0,
                    tuning: 0.0,
                    velocity: 0.0,
                    length: 0,
                    noteId: -1,
                },
            },
        };
        match body {
            MidiData::NoteOn {
                channel,
                note,
                velocity,
            } => {
                let mut ev = header(u16::try_from(EventTypes_::kNoteOnEvent).unwrap_or(0));
                ev.__field0 = Event__type0 {
                    noteOn: NoteOnEvent {
                        channel: i16::from(channel),
                        pitch: i16::from(note),
                        tuning: 0.0,
                        velocity: f32::from(velocity) / 127.0,
                        length: 0,
                        noteId: -1,
                    },
                };
                self.events.push(ev);
            }
            MidiData::NoteOff {
                channel,
                note,
                velocity,
            } => {
                let mut ev = header(u16::try_from(EventTypes_::kNoteOffEvent).unwrap_or(0));
                ev.__field0 = Event__type0 {
                    noteOff: NoteOffEvent {
                        channel: i16::from(channel),
                        pitch: i16::from(note),
                        velocity: f32::from(velocity) / 127.0,
                        noteId: -1,
                        tuning: 0.0,
                    },
                };
                self.events.push(ev);
            }
            MidiData::PolyAftertouch {
                channel,
                note,
                pressure,
            } => {
                let mut ev = header(u16::try_from(EventTypes_::kPolyPressureEvent).unwrap_or(0));
                ev.__field0 = Event__type0 {
                    polyPressure: PolyPressureEvent {
                        channel: i16::from(channel),
                        pitch: i16::from(note),
                        pressure: f32::from(pressure) / 127.0,
                        noteId: -1,
                    },
                };
                self.events.push(ev);
            }
            // VST3 routes CC / ProgramChange / ChannelAftertouch /
            // PitchBend / Sysex through IParameterChanges or
            // IMidiMapping rather than IEventList. Wiring those is
            // a follow-on; for hosting test purposes notes are the
            // primary need.
            _ => {}
        }
    }
}

#[derive(Default)]
struct EventList3 {
    events: std::cell::RefCell<Vec<Event>>,
}

impl Class for EventList3 {
    type Interfaces = (IEventList,);
}

#[allow(clippy::cast_sign_loss)]
impl IEventListTrait for EventList3 {
    unsafe fn getEventCount(&self) -> i32 {
        i32::try_from(self.events.borrow().len()).unwrap_or(i32::MAX)
    }

    unsafe fn getEvent(&self, index: i32, out: *mut Event) -> i32 {
        if out.is_null() || index < 0 {
            return -1;
        }
        let events = self.events.borrow();
        let Some(event) = events.get(index as usize) else {
            return -1;
        };
        unsafe { *out = *event };
        kResultOk
    }

    unsafe fn addEvent(&self, event: *mut Event) -> i32 {
        if event.is_null() {
            return -1;
        }
        self.events.borrow_mut().push(unsafe { *event });
        kResultOk
    }
}

// ---------------------------------------------------------------------------
// MemoryStream — in-memory IBStream impl for state save/load.
// ---------------------------------------------------------------------------

/// Backing storage for the `IBStream` we hand to
/// `IComponent::setState` / `getState`. The plugin reads and
/// writes through `read`/`write`; the host inspects
/// `data` / `position` after the call.
#[derive(Default)]
struct MemoryStream {
    data: std::cell::RefCell<Vec<u8>>,
    position: std::cell::Cell<usize>,
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_read: *mut i32,
    ) -> i32 {
        if buffer.is_null() || num_bytes < 0 {
            return -1;
        }
        let pos = self.position.get();
        #[allow(clippy::cast_sign_loss)]
        let want = num_bytes as usize;
        let data = self.data.borrow();
        let available = data.len().saturating_sub(pos);
        let take = want.min(available);
        if take > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr().add(pos), buffer.cast::<u8>(), take);
            }
        }
        self.position.set(pos + take);
        if !num_bytes_read.is_null() {
            unsafe {
                *num_bytes_read = i32::try_from(take).unwrap_or(i32::MAX);
            }
        }
        kResultOk
    }

    unsafe fn write(
        &self,
        buffer: *mut std::ffi::c_void,
        num_bytes: i32,
        num_bytes_written: *mut i32,
    ) -> i32 {
        if buffer.is_null() || num_bytes < 0 {
            return -1;
        }
        let pos = self.position.get();
        #[allow(clippy::cast_sign_loss)]
        let want = num_bytes as usize;
        let mut data = self.data.borrow_mut();
        if data.len() < pos + want {
            data.resize(pos + want, 0);
        }
        unsafe {
            std::ptr::copy_nonoverlapping(buffer.cast::<u8>(), data.as_mut_ptr().add(pos), want);
        }
        self.position.set(pos + want);
        if !num_bytes_written.is_null() {
            unsafe {
                *num_bytes_written = i32::try_from(want).unwrap_or(i32::MAX);
            }
        }
        kResultOk
    }

    #[allow(clippy::cast_possible_wrap)]
    unsafe fn seek(&self, pos: i64, mode: i32, result: *mut i64) -> i32 {
        // VST3 SDK SeekMode: 0 = `SeekSet`, 1 = `SeekCur`, 2 = `SeekEnd`.
        let data_len = self.data.borrow().len() as i64;
        let current = self.position.get() as i64;
        let new_pos = match mode {
            0 => pos,
            1 => current + pos,
            2 => data_len + pos,
            _ => return -1,
        };
        if new_pos < 0 {
            return -1;
        }
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        self.position.set(new_pos as usize);
        if !result.is_null() {
            unsafe {
                *result = new_pos;
            }
        }
        kResultOk
    }

    #[allow(clippy::cast_possible_wrap)]
    unsafe fn tell(&self, pos: *mut i64) -> i32 {
        if pos.is_null() {
            return -1;
        }
        unsafe {
            *pos = self.position.get() as i64;
        }
        kResultOk
    }
}

// ---------------------------------------------------------------------------
// atome: the host side a plugin talks to.
// ---------------------------------------------------------------------------

/// The host's context, component handler and plugin frame, in one object. Parameter edits (from
/// the plugin's window, or `set_parameter`) wait here for the next processed block.
#[derive(Default)]
struct Host {
    edits: std::sync::Mutex<Vec<(ParamID, ParamValue)>>,
}

impl Class for Host {
    type Interfaces = (IHostApplication, IComponentHandler, IPlugFrame);
}

impl Host {
    fn edit(&self, id: ParamID, value: ParamValue) {
        if let Ok(mut edits) = self.edits.lock() {
            match edits.iter_mut().find(|(edited, _)| *edited == id) {
                Some(edit) => edit.1 = value,
                None => edits.push((id, value)),
            }
        }
    }

    /// The edits waiting, as the processor's input changes; `None` when there are none.
    fn take_changes(&self) -> Option<ComWrapper<ParamChanges>> {
        let edits = std::mem::take(&mut *self.edits.lock().ok()?);
        if edits.is_empty() {
            return None;
        }
        let queues = edits
            .into_iter()
            .filter_map(|(id, value)| ComWrapper::new(ParamQueue { id, value }).to_com_ptr::<IParamValueQueue>())
            .collect();
        Some(ComWrapper::new(ParamChanges { queues }))
    }
}

impl IHostApplicationTrait for Host {
    unsafe fn getName(&self, name: *mut String128) -> i32 {
        if name.is_null() {
            return kResultFalse;
        }
        let name = unsafe { &mut *name };
        name.fill(0);
        for (slot, unit) in name.iter_mut().zip("atome".encode_utf16()) {
            *slot = unit;
        }
        kResultOk
    }

    // The messages a plugin's processor and controller send each other through their connection
    // points are made by the host (atome: as the SDK's own host does; plugins whose controller is
    // separate need them).
    unsafe fn createInstance(&self, cid: *mut TUID, _iid: *mut TUID, obj: *mut *mut std::ffi::c_void) -> i32 {
        if obj.is_null() {
            return kResultFalse;
        }
        let cid: &[u8] = if cid.is_null() { &[] } else { unsafe { std::slice::from_raw_parts(cid.cast::<u8>(), 16) } };
        let made = if cid == IMessage::IID {
            ComWrapper::new(Message::default()).to_com_ptr::<IMessage>().map(|message| message.into_raw().cast())
        } else if cid == IAttributeList::IID {
            ComWrapper::new(Attributes::default()).to_com_ptr::<IAttributeList>().map(|list| list.into_raw().cast())
        } else {
            None
        };
        unsafe { *obj = made.unwrap_or(ptr::null_mut()) };
        if made.is_some() { kResultOk } else { kNoInterface }
    }
}

impl IComponentHandlerTrait for Host {
    unsafe fn beginEdit(&self, _id: ParamID) -> i32 {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> i32 {
        self.edit(id, value);
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> i32 {
        kResultOk
    }

    unsafe fn restartComponent(&self, _flags: i32) -> i32 {
        kResultOk
    }
}

impl IPlugFrameTrait for Host {
    // shortcut: the view takes the new size but the host's window does not follow it; make the
    // window follow (an editor callback to the host) when a plugin that resizes needs it.
    unsafe fn resizeView(&self, view: *mut IPlugView, size: *mut ViewRect) -> i32 {
        match unsafe { ComRef::from_raw(view) } {
            Some(view) if !size.is_null() => unsafe { view.onSize(size) },
            _ => kResultFalse,
        }
    }
}

/// One parameter's change for a block: a single point at its start.
struct ParamQueue {
    id: ParamID,
    value: ParamValue,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }

    unsafe fn getPointCount(&self) -> i32 {
        1
    }

    unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut ParamValue) -> i32 {
        if index != 0 || offset.is_null() || value.is_null() {
            return kResultFalse;
        }
        unsafe {
            *offset = 0;
            *value = self.value;
        }
        kResultOk
    }

    unsafe fn addPoint(&self, _offset: i32, _value: ParamValue, _index: *mut i32) -> i32 {
        kResultFalse
    }
}

/// A block's parameter changes.
struct ParamChanges {
    queues: Vec<ComPtr<IParamValueQueue>>,
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> i32 {
        i32::try_from(self.queues.len()).unwrap_or(i32::MAX)
    }

    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        usize::try_from(index).ok().and_then(|index| self.queues.get(index)).map_or(ptr::null_mut(), ComPtr::as_ptr)
    }

    unsafe fn addParameterData(&self, _id: *const ParamID, _index: *mut i32) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
}

/// A message between a plugin's processor and controller (`IHostApplication::createInstance`).
struct Message {
    id: std::cell::RefCell<std::ffi::CString>,
    attributes: ComWrapper<Attributes>,
}

impl Default for Message {
    fn default() -> Self {
        Self { id: std::cell::RefCell::new(std::ffi::CString::default()), attributes: ComWrapper::new(Attributes::default()) }
    }
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> *const std::ffi::c_char {
        self.id.borrow().as_ptr()
    }

    unsafe fn setMessageID(&self, id: *const std::ffi::c_char) {
        if !id.is_null() {
            *self.id.borrow_mut() = unsafe { std::ffi::CStr::from_ptr(id) }.to_owned();
        }
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Borrowed, as the SDK's: the message keeps it.
        self.attributes.as_com_ref::<IAttributeList>().map_or(ptr::null_mut(), |list| list.as_ptr())
    }
}

enum Attribute {
    Int(i64),
    Float(f64),
    Text(Vec<u16>),
    Binary(Vec<u8>),
}

/// The values a message carries, by name.
#[derive(Default)]
struct Attributes {
    values: std::cell::RefCell<std::collections::HashMap<Vec<u8>, Attribute>>,
}

impl Class for Attributes {
    type Interfaces = (IAttributeList,);
}

fn attribute_key(id: *const std::ffi::c_char) -> Option<Vec<u8>> {
    (!id.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(id) }.to_bytes().to_vec())
}

impl Attributes {
    fn set(&self, id: *const std::ffi::c_char, value: Attribute) -> i32 {
        let Some(key) = attribute_key(id) else { return kResultFalse };
        self.values.borrow_mut().insert(key, value);
        kResultOk
    }
}

impl IAttributeListTrait for Attributes {
    unsafe fn setInt(&self, id: *const std::ffi::c_char, value: i64) -> i32 {
        self.set(id, Attribute::Int(value))
    }

    unsafe fn getInt(&self, id: *const std::ffi::c_char, value: *mut i64) -> i32 {
        match attribute_key(id).and_then(|key| match self.values.borrow().get(&key) { Some(Attribute::Int(found)) => Some(*found), _ => None }) {
            Some(found) if !value.is_null() => {
                unsafe { *value = found };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: *const std::ffi::c_char, value: f64) -> i32 {
        self.set(id, Attribute::Float(value))
    }

    unsafe fn getFloat(&self, id: *const std::ffi::c_char, value: *mut f64) -> i32 {
        match attribute_key(id).and_then(|key| match self.values.borrow().get(&key) { Some(Attribute::Float(found)) => Some(*found), _ => None }) {
            Some(found) if !value.is_null() => {
                unsafe { *value = found };
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: *const std::ffi::c_char, string: *const u16) -> i32 {
        if string.is_null() {
            return kResultFalse;
        }
        let mut text = Vec::new();
        let mut at = string;
        while unsafe { *at } != 0 {
            text.push(unsafe { *at });
            at = unsafe { at.add(1) };
        }
        self.set(id, Attribute::Text(text))
    }

    unsafe fn getString(&self, id: *const std::ffi::c_char, string: *mut u16, size_in_bytes: u32) -> i32 {
        let values = self.values.borrow();
        let Some(Attribute::Text(text)) = attribute_key(id).and_then(|key| values.get(&key)) else { return kResultFalse };
        let room = (size_in_bytes as usize / 2).saturating_sub(1);
        if string.is_null() || size_in_bytes < 2 {
            return kResultFalse;
        }
        let count = text.len().min(room);
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), string, count);
            *string.add(count) = 0;
        }
        kResultOk
    }

    unsafe fn setBinary(&self, id: *const std::ffi::c_char, data: *const std::ffi::c_void, size_in_bytes: u32) -> i32 {
        let bytes = if data.is_null() { Vec::new() } else { unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size_in_bytes as usize) }.to_vec() };
        self.set(id, Attribute::Binary(bytes))
    }

    unsafe fn getBinary(&self, id: *const std::ffi::c_char, data: *mut *const std::ffi::c_void, size_in_bytes: *mut u32) -> i32 {
        let values = self.values.borrow();
        let Some(Attribute::Binary(bytes)) = attribute_key(id).and_then(|key| values.get(&key)) else { return kResultFalse };
        if data.is_null() || size_in_bytes.is_null() {
            return kResultFalse;
        }
        // Points into the list, valid while the value is not set again (as the SDK's).
        unsafe {
            *data = bytes.as_ptr().cast();
            *size_in_bytes = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        }
        kResultOk
    }
}
