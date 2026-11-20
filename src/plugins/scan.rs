//! Finding the hosted plugins installed on this machine, for a host's browser.
//!
//! Each [`Found`] carries what a "by company" or "by type" list needs: a
//! vendor and a [`kind`](Found::kind). VST3 bundles are read from their
//! `Contents/Resources/moduleinfo.json` when they ship one (VST3 SDK 3.7.5 and
//! later), which names the vendor and sub-categories without loading any
//! code; older bundles are loaded to be listed and get their kind from their
//! name. Audio Units come from the system registry and are typed by name too.
//!
//! ```no_run
//! use atome::plugins::scan;
//!
//! let (found, failed) = scan::vst3(&scan::default_vst3_folders());
//! for plugin in &found {
//!     println!("{} / {} / {}", plugin.vendor, plugin.kind, plugin.name);
//! }
//! ```

use std::path::PathBuf;

use super::{Plugin, PluginFormat};

/// One plugin a scan found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    /// Empty when the plugin does not say.
    pub vendor: String,
    pub format: PluginFormat,
    /// What finds it again: a VST3's class id, an AU's `type:sub:mfr`.
    pub id: String,
    /// The bundle. Empty for an AU, which lives in a registry.
    pub path: PathBuf,
    /// One of [`KINDS`].
    pub kind: &'static str,
    /// An instrument or MIDI effect: not something an audio chain runs.
    pub instrument: bool,
}

/// A bundle a scan could not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failed {
    pub path: PathBuf,
    pub reason: String,
}

/// The kinds a plugin is typed as, in browser order.
pub const KINDS: [&str; 14] = [
    "EQ",
    "Dynamics",
    "Filter",
    "Reverb",
    "Delay",
    "Modulation",
    "Distortion",
    "Pitch",
    "Spatial",
    "Restoration",
    "Mastering",
    "Analyzer",
    "Tools",
    "Other",
];

impl Found {
    /// A descriptor for this plugin, ready for [`Plugin::load`].
    pub fn plugin(&self, buffer_size: usize, sample_rate: usize, channels: usize) -> Plugin {
        let path = match self.format {
            PluginFormat::Au | PluginFormat::Au3 => PathBuf::from(&self.id),
            _ => self.path.clone(),
        };
        Plugin::new(
            self.name.clone(),
            path,
            buffer_size,
            sample_rate,
            channels,
            String::new(),
            self.format,
        )
    }
}

/// The usual VST3 folders for this OS.
#[cfg(feature = "vst3")]
pub fn default_vst3_folders() -> Vec<PathBuf> {
    super::vst3_host::default_vst3_paths()
}

/// Every VST3 in `folders`, each searched one level deep as hosts do.
#[cfg(feature = "vst3")]
pub fn vst3(folders: &[PathBuf]) -> (Vec<Found>, Vec<Failed>) {
    let (mut found, mut failed) = (Vec::new(), Vec::new());
    for folder in folders {
        let Ok(entries) = std::fs::read_dir(folder) else { continue };
        for bundle in entries.flatten().map(|entry| entry.path()) {
            if !bundle.to_string_lossy().ends_with(".vst3") {
                continue;
            }
            if let Some(listed) = module_info(&bundle) {
                found.extend(listed);
                continue;
            }
            match super::vst3_host::scan_bundle(&bundle) {
                Ok(listed) if !listed.is_empty() => {
                    found.extend(listed.into_iter().map(|info| Found {
                        kind: kind_from_name(&info.name),
                        name: info.name,
                        vendor: info.vendor,
                        format: PluginFormat::Vst3,
                        id: info.unique_id,
                        path: bundle.clone(),
                        instrument: false,
                    }))
                }
                Ok(_) => failed.push(Failed {
                    path: bundle,
                    reason: "no audio plugin in the bundle, or it would not load".into(),
                }),
                Err(error) => failed.push(Failed { path: bundle, reason: error.to_string() }),
            }
        }
    }
    (found, failed)
}

/// What a bundle's `moduleinfo.json` lists, or `None` without one.
#[cfg(feature = "vst3")]
fn module_info(bundle: &std::path::Path) -> Option<Vec<Found>> {
    let text = std::fs::read_to_string(bundle.join("Contents/Resources/moduleinfo.json")).ok()?;
    let info: serde_json::Value = serde_json::from_str(&text).ok()?;
    let factory_vendor = info["Factory Info"]["Vendor"].as_str().unwrap_or_default();
    let classes = info["Classes"].as_array()?;
    Some(
        classes
            .iter()
            .filter(|class| class["Category"] == "Audio Module Class")
            .map(|class| {
                let name = class["Name"].as_str().unwrap_or_default().to_string();
                let subs: Vec<&str> = class["Sub Categories"]
                    .as_array()
                    .map(|subs| subs.iter().filter_map(|sub| sub.as_str()).collect())
                    .unwrap_or_default();
                Found {
                    kind: kind_from_vst3(&subs).unwrap_or_else(|| kind_from_name(&name)),
                    instrument: subs.contains(&"Instrument"),
                    vendor: class["Vendor"].as_str().unwrap_or(factory_vendor).to_string(),
                    name,
                    format: PluginFormat::Vst3,
                    id: class["CID"].as_str().unwrap_or_default().to_string(),
                    path: bundle.to_path_buf(),
                }
            })
            .collect(),
    )
}

/// Every Audio Unit (v2, and v3 extensions when `au3` is on) in the registry.
#[cfg(all(any(feature = "au", feature = "au3"), target_vendor = "apple"))]
pub fn au() -> Vec<Found> {
    use truce_rack_core::{info::PluginCategory, scanner::PluginScanner};

    let mut infos = Vec::new();
    #[cfg(feature = "au")]
    infos.extend(
        super::au_host::AuScanner::new()
            .scan()
            .unwrap_or_default()
            .into_iter()
            .map(|info| (info, PluginFormat::Au)),
    );
    #[cfg(feature = "au3")]
    infos.extend(
        super::au3_scan::Au3Scanner::new()
            .scan()
            .unwrap_or_default()
            .into_iter()
            .map(|info| (info, PluginFormat::Au3)),
    );
    infos
        .into_iter()
        .map(|(info, format)| Found {
            kind: match info.category {
                PluginCategory::Analyzer => "Analyzer",
                PluginCategory::Tool => "Tools",
                _ => kind_from_name(&info.name),
            },
            instrument: matches!(info.category, PluginCategory::Instrument | PluginCategory::NoteEffect),
            name: info.name,
            vendor: info.vendor,
            format,
            id: info.unique_id,
            path: PathBuf::new(),
        })
        .collect()
}

/// The kind VST3 sub-categories (`Fx|EQ`, split) name; the first match wins.
pub fn kind_from_vst3(subs: &[&str]) -> Option<&'static str> {
    subs.iter().find_map(|sub| {
        Some(match *sub {
            "EQ" => "EQ",
            "Dynamics" => "Dynamics",
            "Filter" => "Filter",
            "Reverb" => "Reverb",
            "Delay" => "Delay",
            "Modulation" => "Modulation",
            "Distortion" => "Distortion",
            "Pitch Shift" => "Pitch",
            "Spatial" | "Surround" => "Spatial",
            "Restoration" => "Restoration",
            "Mastering" => "Mastering",
            "Analyzer" => "Analyzer",
            "Tools" | "Network" => "Tools",
            _ => return None,
        })
    })
}

/// A kind guessed from a plugin's name; `"Other"` when nothing matches.
pub fn kind_from_name(name: &str) -> &'static str {
    const WORDS: [(&str, &[&str]); 12] = [
        ("EQ", &["eq", "equaliz"]),
        ("Dynamics", &["comp", "limit", "gate", "expand", "dynamic", "de-ess", "deess"]),
        ("Filter", &["filter", "pass"]),
        ("Reverb", &["reverb", "verb", "room", "hall", "plate"]),
        ("Delay", &["delay", "echo"]),
        ("Modulation", &["chorus", "flang", "phase", "trem", "vibr", "modul"]),
        ("Distortion", &["dist", "drive", "satur", "fuzz", "crush"]),
        ("Pitch", &["pitch", "tune", "shift"]),
        ("Spatial", &["pan", "width", "stereo", "spatial", "surround"]),
        ("Restoration", &["denois", "noise", "restor", "declick"]),
        ("Analyzer", &["meter", "analy", "scope", "spectr"]),
        ("Tools", &["gain", "util", "trim", "invert", "polarity"]),
    ];
    let name = name.to_lowercase();
    let words: Vec<&str> = name.split(|c: char| !c.is_alphanumeric() && c != '-').collect();
    WORDS
        .iter()
        .find(|(_, keys)| {
            keys.iter().any(|key| {
                // Short keys whole-word only, so "frequency" is not an EQ.
                if key.len() <= 2 {
                    words.contains(key)
                } else {
                    words.iter().any(|word| word.contains(key))
                }
            })
        })
        .map_or("Other", |(kind, _)| kind)
}
