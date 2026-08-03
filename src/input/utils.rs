//! Device discovery, for capture rather than playback.
//!
//! The mirror of [`output::utils`](crate::output::utils), asking each host for
//! its *input* devices. A machine's two lists overlap but are not the same:
//! speakers appear only in one, a microphone only in the other, and an
//! interface in both. Host enumeration is shared, since a host is a host — use
//! [`output::list_hosts`](crate::output::list_hosts) for that.

use cpal::traits::HostTrait;
use cpal::{Device, Error, ErrorKind, Host};

/// Every input-capable [`Device`] on `host`, or on the default host if `host`
/// is `None`.
///
/// Output-only devices are left out. The list is a snapshot: devices can be
/// plugged in or removed at any time, so call this again rather than caching
/// the result for long.
pub fn list_devices(host: Option<Host>) -> Result<Vec<Device>, Error> {
    if let Some(host) = host {
        Ok(host.input_devices()?.collect())
    } else {
        Ok(cpal::default_host().input_devices()?.collect())
    }
}

/// The names of every input-capable device, in the same order as
/// [`list_devices`].
pub fn list_device_names() -> Result<Vec<String>, Error> {
    Ok(list_devices(None)?.iter().map(Device::to_string).collect())
}

/// Finds the input device called `name`.
///
/// Matches exactly first, then falls back to a case-insensitive match, so a
/// name typed by hand still resolves. Fails with
/// [`ErrorKind::DeviceNotAvailable`] if nothing matches.
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
                format!("no input device named {name:?}"),
            )
        })
}

/// The host's default input device, if it has one.
pub fn default_device() -> Option<Device> {
    cpal::default_host().default_input_device()
}
