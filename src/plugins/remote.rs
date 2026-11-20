//! Hosted plugins in a helper process of their own, so a plugin that crashes takes down only
//! its helper, not the host.
//!
//! The helper is any program that calls [`host_main`] (a host usually relaunches itself with an
//! argument it recognises, so there is nothing extra to ship). [`Remote::spawn`] starts one helper
//! per plugin and talks to it over the helper's standard input and output:
//!
//! - every request is `op: u8`, `len: u32` (little-endian), then `len` bytes;
//! - every reply is `status: u8` (0 ok, 1 error: the payload is the message), `len: u32`, payload.
//!
//! Audio goes block by block (`process`: interleaved `f32` in, the same out), so a host that
//! feeds ahead of playback (as a cue player does) hides the round trip. If the helper dies, every
//! call fails: the host plays on without the plugin and spawns a new helper when it wants one.
//!
//! The plugin's own window opens in the helper, attached to a parent window the host passes by
//! its native handle; the helper pumps the platform's messages so the window stays alive.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use super::{Plugin, PluginFormat, WindowHandle};

const LOAD: u8 = 1;
const PROCESS: u8 = 2;
const RESET: u8 = 3;
const STATE: u8 = 4;
const SET_STATE: u8 = 5;
const LATENCY: u8 = 6;
const OPEN_EDITOR: u8 = 7;
const CLOSE_EDITOR: u8 = 8;
const QUIT: u8 = 9;

fn format_code(format: PluginFormat) -> u8 {
    match format {
        PluginFormat::Internal => 0,
        PluginFormat::Au => 1,
        PluginFormat::Au3 => 2,
        PluginFormat::Vst => 3,
        PluginFormat::Vst3 => 4,
    }
}

fn format_of(code: u8) -> PluginFormat {
    match code {
        1 => PluginFormat::Au,
        2 => PluginFormat::Au3,
        3 => PluginFormat::Vst,
        4 => PluginFormat::Vst3,
        _ => PluginFormat::Internal,
    }
}

fn write_frame(out: &mut impl Write, op: u8, payload: &[u8]) -> std::io::Result<()> {
    out.write_all(&[op])?;
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(payload)?;
    out.flush()
}

fn read_frame(input: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    input.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    let mut payload = vec![0u8; len];
    input.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn take<'a>(input: &mut &'a [u8], n: usize) -> Result<&'a [u8], String> {
    if input.len() < n {
        return Err("a request was cut short".into());
    }
    let (head, rest) = input.split_at(n);
    *input = rest;
    Ok(head)
}

fn take_u32(input: &mut &[u8]) -> Result<u32, String> {
    let bytes = take(input, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn take_str(input: &mut &[u8]) -> Result<String, String> {
    let len = take_u32(input)? as usize;
    Ok(String::from_utf8_lossy(take(input, len)?).into_owned())
}

// --- the host's side ---------------------------------------------------------------------------

/// One plugin running in a helper process.
pub struct Remote {
    child: Child,
    to: ChildStdin,
    from: ChildStdout,
    latency: usize,
    /// Set once a call fails: the helper is gone (or confused) and every later call fails at once.
    dead: bool,
    scratch: Vec<u8>,
}

impl Remote {
    /// Starts `program args…` as a helper (it must call [`host_main`]) and loads the plugin in it:
    /// the same description [`Plugin::new`] takes, plus its saved `state` if any.
    ///
    /// # Errors
    ///
    /// The helper would not start, or the plugin would not load in it.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        program: &Path,
        args: &[&str],
        name: &str,
        path: &Path,
        format: PluginFormat,
        sample_rate: u32,
        channels: u16,
        max_block: u32,
        state: Option<&[u8]>,
    ) -> Result<Self, String> {
        let mut command = Command::new(program);
        command.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window for the helper.
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|error| format!("could not start the plugin host: {error}"))?;
        let to = child.stdin.take().ok_or("the plugin host has no input")?;
        let from = child.stdout.take().ok_or("the plugin host has no output")?;
        let mut remote = Self { child, to, from, latency: 0, dead: false, scratch: Vec::new() };
        let mut payload = vec![format_code(format)];
        put_str(&mut payload, name);
        put_str(&mut payload, &path.to_string_lossy());
        payload.extend_from_slice(&sample_rate.to_le_bytes());
        payload.extend_from_slice(&u32::from(channels).to_le_bytes());
        payload.extend_from_slice(&max_block.to_le_bytes());
        let state = state.unwrap_or_default();
        payload.extend_from_slice(&(state.len() as u32).to_le_bytes());
        payload.extend_from_slice(state);
        let reply = remote.call(LOAD, &payload)?;
        if reply.len() >= 4 {
            remote.latency = u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]) as usize;
        }
        Ok(remote)
    }

    fn call(&mut self, op: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
        if self.dead {
            return Err("the plugin host has stopped".into());
        }
        let result = write_frame(&mut self.to, op, payload).and_then(|()| read_frame(&mut self.from));
        match result {
            Ok((0, reply)) => Ok(reply),
            Ok((_, message)) => Err(String::from_utf8_lossy(&message).into_owned()),
            Err(error) => {
                self.dead = true;
                Err(format!("the plugin host has stopped: {error}"))
            }
        }
    }

    /// Whether the helper has stopped (crashed, or was killed): the plugin is gone until respawned.
    pub fn is_dead(&mut self) -> bool {
        if !self.dead && matches!(self.child.try_wait(), Ok(Some(_))) {
            self.dead = true;
        }
        self.dead
    }

    /// Runs `buffer` (interleaved, `channels` wide) through the plugin, in place.
    ///
    /// # Errors
    ///
    /// The helper has stopped; `buffer` is left as it was.
    pub fn process(&mut self, buffer: &mut [f32], channels: u16) -> Result<(), String> {
        let mut payload = std::mem::take(&mut self.scratch);
        payload.clear();
        payload.extend_from_slice(&u32::from(channels).to_le_bytes());
        for sample in buffer.iter() {
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        let reply = self.call(PROCESS, &payload);
        self.scratch = payload;
        let reply = reply?;
        if reply.len() != buffer.len() * 4 {
            return Err("the plugin host sent back the wrong amount of audio".into());
        }
        for (sample, bytes) in buffer.iter_mut().zip(reply.chunks_exact(4)) {
            *sample = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        Ok(())
    }

    /// Clears the plugin's tails (see [`Plugin::reset`]).
    pub fn reset(&mut self) -> Result<(), String> {
        self.call(RESET, &[]).map(|_| ())
    }

    /// The plugin's whole state (see [`Plugin::state`]).
    pub fn state(&mut self) -> Result<Vec<u8>, String> {
        self.call(STATE, &[])
    }

    /// Restores bytes [`state`](Self::state) gave.
    pub fn set_state(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.call(SET_STATE, bytes).map(|_| ())
    }

    /// The plugin's latency in frames, as it reported it when it loaded.
    pub fn latency(&self) -> usize {
        self.latency
    }

    /// Opens the plugin's own window inside `parent` (a native window of the host's), at `scale`.
    /// Returns its size in pixels, if the plugin says.
    pub fn open_editor(&mut self, parent: WindowHandle, scale: f64) -> Result<Option<(u32, u32)>, String> {
        let (kind, raw): (u8, u64) = match parent {
            WindowHandle::HWND(hwnd) => (0, hwnd as u64),
            WindowHandle::NSView(view) => (1, view as u64),
            WindowHandle::X11(window) => (2, window),
        };
        let mut payload = vec![kind];
        payload.extend_from_slice(&raw.to_le_bytes());
        payload.extend_from_slice(&scale.to_le_bytes());
        let reply = self.call(OPEN_EDITOR, &payload)?;
        Ok((reply.len() >= 8).then(|| (u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]), u32::from_le_bytes([reply[4], reply[5], reply[6], reply[7]]))))
    }

    /// Closes the plugin's window.
    pub fn close_editor(&mut self) -> Result<(), String> {
        self.call(CLOSE_EDITOR, &[]).map(|_| ())
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        if !self.dead {
            let _ = write_frame(&mut self.to, QUIT, &[]);
        }
        // Not waited on for long: a helper that hangs is killed.
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- the helper's side -------------------------------------------------------------------------

/// The helper's whole life: reads requests from standard input, answers on standard output, until
/// told to quit or its input closes. Call it from `main` when the host started this program as a
/// helper; it returns the exit code.
pub fn host_main() -> i32 {
    // Requests are read on their own thread, so this one can pump the window system's messages
    // (a plugin's window needs them) while it waits.
    let (requests, incoming) = std::sync::mpsc::channel::<std::io::Result<(u8, Vec<u8>)>>();
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        loop {
            let frame = read_frame(&mut input);
            let stop = frame.is_err();
            if requests.send(frame).is_err() || stop {
                return;
            }
        }
    });
    let mut out = std::io::stdout().lock();
    let mut plugin: Option<Plugin> = None;
    let mut channels_loaded = 2u16;
    loop {
        pump_messages();
        let frame = match incoming.recv_timeout(std::time::Duration::from_millis(5)) {
            Ok(Ok(frame)) => frame,
            Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return 0,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        };
        let (op, payload) = frame;
        if op == QUIT {
            if let Some(editor) = plugin.as_mut().and_then(Plugin::editor) {
                editor.close();
            }
            return 0;
        }
        let reply = serve(op, &payload, &mut plugin, &mut channels_loaded);
        let written = match reply {
            Ok(bytes) => write_frame(&mut out, 0, &bytes),
            Err(message) => write_frame(&mut out, 1, message.as_bytes()),
        };
        if written.is_err() {
            return 1;
        }
    }
}

fn serve(op: u8, payload: &[u8], plugin: &mut Option<Plugin>, channels_loaded: &mut u16) -> Result<Vec<u8>, String> {
    let mut input = payload;
    match op {
        LOAD => {
            let format = format_of(take(&mut input, 1)?[0]);
            let name = take_str(&mut input)?;
            let path = PathBuf::from(take_str(&mut input)?);
            let sample_rate = take_u32(&mut input)?;
            let channels = take_u32(&mut input)?;
            let max_block = take_u32(&mut input)?;
            let state_len = take_u32(&mut input)? as usize;
            let state = take(&mut input, state_len)?;
            let mut loaded = Plugin::new(name, path, max_block as usize, sample_rate as usize, channels as usize, String::new(), format);
            loaded.load().map_err(|error| error.to_string())?;
            if !state.is_empty() {
                loaded.set_state(state)?;
            }
            let latency = loaded.latency() as u32;
            *channels_loaded = channels as u16;
            *plugin = Some(loaded);
            Ok(latency.to_le_bytes().to_vec())
        }
        _ => {
            let plugin = plugin.as_mut().ok_or("no plugin is loaded")?;
            match op {
                PROCESS => {
                    let channels = take_u32(&mut input)? as u16;
                    let mut buffer: Vec<f32> = input.chunks_exact(4).map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])).collect();
                    plugin.apply(&mut buffer, channels).map_err(|error| error.to_string())?;
                    Ok(buffer.iter().flat_map(|sample| sample.to_le_bytes()).collect())
                }
                RESET => {
                    plugin.reset();
                    Ok(Vec::new())
                }
                STATE => plugin.state().ok_or_else(|| "the plugin has no state to save".into()),
                SET_STATE => plugin.set_state(input).map(|()| Vec::new()),
                LATENCY => Ok((plugin.latency() as u32).to_le_bytes().to_vec()),
                OPEN_EDITOR => {
                    let kind = take(&mut input, 1)?[0];
                    let raw = take(&mut input, 8)?;
                    let raw = u64::from_le_bytes(raw.try_into().map_err(|_| "a bad window handle")?);
                    let scale = f64::from_le_bytes(take(&mut input, 8)?.try_into().map_err(|_| "a bad scale")?);
                    let parent = match kind {
                        0 => WindowHandle::HWND(raw as *mut std::ffi::c_void),
                        1 => WindowHandle::NSView(raw as *mut std::ffi::c_void),
                        _ => WindowHandle::X11(raw),
                    };
                    let editor = plugin.editor().ok_or("the plugin has no window of its own")?;
                    editor.open(parent, scale).map_err(|error| error.to_string())?;
                    editor.show();
                    Ok(editor.size().map(|(width, height)| [width.to_le_bytes(), height.to_le_bytes()].concat()).unwrap_or_default())
                }
                CLOSE_EDITOR => {
                    if let Some(editor) = plugin.editor() {
                        editor.close();
                    }
                    Ok(Vec::new())
                }
                other => Err(format!("unknown request {other}")),
            }
        }
    }
}

/// Lets the window system deliver its messages to this thread's windows (a plugin's editor).
#[cfg(windows)]
fn pump_messages() {
    #[repr(C)]
    struct Msg {
        hwnd: isize,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        pt: [i32; 2],
        private: u32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn PeekMessageW(msg: *mut Msg, hwnd: isize, min: u32, max: u32, remove: u32) -> i32;
        fn TranslateMessage(msg: *const Msg) -> i32;
        fn DispatchMessageW(msg: *const Msg) -> isize;
    }
    let mut msg = Msg { hwnd: 0, message: 0, wparam: 0, lparam: 0, time: 0, pt: [0, 0], private: 0 };
    // SAFETY: plain Win32 calls on this thread's own message queue with a correctly laid out MSG.
    unsafe {
        while PeekMessageW(&mut msg, 0, 0, 0, 1) != 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

// shortcut: macOS and Linux plugin windows in a helper need their run loops pumped too (CFRunLoop,
// X11 events); audio works out of process there now, windows come later.
#[cfg(not(windows))]
fn pump_messages() {}
