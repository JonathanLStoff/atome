//! Device discovery helpers for the default host.

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{Device, Error, ErrorKind};

/// Every output-capable [`Device`] on the default host.
///
/// The list is a snapshot: devices can be plugged in or removed at any time, so
/// call this again rather than caching the result for long.
pub fn list_devices() -> Result<Vec<Device>, Error> {
    Ok(cpal::default_host().output_devices()?.collect())
}

/// The names of every output-capable device, in the same order as
/// [`list_devices`]. Handy for showing the user a list to pick from and then
/// handing the chosen string straight to [`find_device`].
pub fn list_device_names() -> Result<Vec<String>, Error> {
    Ok(list_devices()?.iter().map(Device::to_string).collect())
}

/// Finds the output device called `name`.
///
/// Matches exactly first, then falls back to a case-insensitive match, so a
/// name typed by hand still resolves. Fails with [`ErrorKind::DeviceNotAvailable`]
/// if nothing matches.
pub fn find_device(name: &str) -> Result<Device, Error> {
    let devices = list_devices()?;

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
