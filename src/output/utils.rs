//! Device discovery helpers for the default host.

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Error, ErrorKind, Host, HostId};


/// Every audio host compiled into this build that reports itself as available,
/// initialised and ready to enumerate devices.
///
/// A host is the platform API the audio goes through — CoreAudio on macOS,
/// WASAPI/ASIO/DirectSound on Windows — and each one sees its own set of
/// devices, so a device missing from [`list_devices`] may just be on another
/// host. Which hosts exist at all is decided at compile time by cargo features
/// (`asio`, for one), not at runtime.
///
/// # Panics
///
/// If a host says it is available but then fails to initialise. cpal already
/// filters the list by availability, so this is the audio subsystem breaking
/// between the two calls rather than an ordinary "not installed" — use
/// [`get_host_by_id`] if a failure there should be handled instead of fatal.
pub fn list_hosts() -> Vec<Host> {
    cpal::available_hosts()
        .into_iter()
        .map(|host_id| cpal::host_from_id(host_id).unwrap())
        .collect()
}

/// Initialises the host with this [`HostId`].
///
/// Fails if the host is not available on this system — not compiled in, not
/// installed (ASIO with no driver), or its service is not running.
pub fn get_host_by_id(host_id: HostId) -> Result<Host, Error> {
    cpal::host_from_id(host_id)
}

/// Finds the host whose name is exactly `name`, e.g. `"CoreAudio"`, `"ASIO"`,
/// `"WASAPI"`.
///
/// The match is case-sensitive and against [`HostId::name`], which is
/// capitalised — `"coreaudio"` will not match. Use [`get_host_in_name`] for
/// something more forgiving. Fails with [`ErrorKind::DeviceNotAvailable`] if no
/// available host has that name.
pub fn get_host_by_name(name: &str) -> Result<Host, Error> {
    cpal::available_hosts()
        .into_iter()
        .find(|host_id| host_id.name() == name)
        .map(|host_id| cpal::host_from_id(host_id).unwrap())
        .ok_or_else(|| {
            Error::with_message(
                ErrorKind::DeviceNotAvailable,
                format!("no host named {name:?}"),
            )
        })
}

/// Finds the first host whose name *contains* `name`, so `"Direct"` resolves
/// `"DirectSound"`.
///
/// Case-insensitive, so `"asio"` finds `"ASIO"`. "First" is cpal's own host
/// order rather than anything meaningful, so a substring matching several hosts
/// picks one arbitrarily — prefer [`get_host_by_name`] when the name is known
/// exactly.
/// Fails with [`ErrorKind::DeviceNotAvailable`] if nothing matches.
pub fn get_host_in_name(name: &str) -> Result<Host, Error> {
    cpal::available_hosts()
        .into_iter()
        .find(|host_id| host_id.name().to_lowercase().contains(&name.to_lowercase()))
        .map(|host_id| cpal::host_from_id(host_id).unwrap())
        .ok_or_else(|| {
            Error::with_message(
                ErrorKind::DeviceNotAvailable,
                format!("no host named {name:?}"),
            )
        })
}

/// Every output-capable [`Device`] on `host`, or on the default host if `host`
/// is `None`.
///
/// Input-only devices are left out. The list is a snapshot: devices can be
/// plugged in or removed at any time, so call this again rather than caching
/// the result for long.
pub fn list_devices(host: Option<Host>) -> Result<Vec<Device>, Error> {
    if let Some(host) = host {
        Ok(host.output_devices()?.collect())
    } else {
        Ok(cpal::default_host().output_devices()?.collect())
    }
}

/// The names of every output-capable device, in the same order as
/// [`list_devices`]. Handy for showing the user a list to pick from and then
/// handing the chosen string straight to [`find_device`].
pub fn list_device_names() -> Result<Vec<String>, Error> {
    Ok(list_devices(None)?.iter().map(Device::to_string).collect())
}

/// Finds the output device called `name`.
///
/// Matches exactly first, then falls back to a case-insensitive match, so a
/// name typed by hand still resolves. Fails with [`ErrorKind::DeviceNotAvailable`]
/// if nothing matches.
pub fn find_device(name: &str) -> Result<Device, Error> {
    let devices = list_devices(None)?;

    devices
        .iter()
        .find(|device| device.to_string() == name)
        .or_else(|| {
            devices
                .iter()
                .find(|device| device.to_string().eq_ignore_ascii_case(name))
        })
        .cloned()
        .ok_or_else(|| {
            Error::with_message(
                ErrorKind::DeviceNotAvailable,
                format!("no output device named {name:?}"),
            )
        })
}

/// The host's default output device, if it has one.
pub fn default_device() -> Option<Device> {
    cpal::default_host().default_output_device()
}

/// Return the name of the device provided.
///
/// The name has to be owned: `description()` hands back a `DeviceDescription`
/// by value, so a `&str` borrowed out of it would dangle the moment this
/// function returns. Falls back to the device's `Display` name if querying the
/// description fails (which happens when the device has been unplugged).
pub fn device_name(device: &Device) -> String {
    device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| device.to_string())
}


