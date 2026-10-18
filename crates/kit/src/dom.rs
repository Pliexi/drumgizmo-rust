// SPDX-License-Identifier: GPL-3.0-or-later
//! A port of `src/DGDOM.h` from original DrumGizmo 0.9.20.
//!
//! Types are named exactly like their C++ counterparts so the port can be
//! diffed against upstream. `main_state_t` is modelled as [`MainState`].

/// Mirrors `main_state_t` (`channel.h` in upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MainState {
    /// Attribute not present in the XML.
    #[default]
    Unset,
    /// Told explicitly to be the main channel.
    IsMain,
    /// Told explicitly not to be a main channel.
    IsNotMain,
}

impl MainState {
    /// True if this is `IsMain`.
    pub fn is_main(&self) -> bool {
        matches!(self, MainState::IsMain)
    }
}

/// Mirrors `SampleRefDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleRefDom {
    /// Not currently used by upstream either (see TODO in domloader.cc).
    pub probability: f64,
    pub name: String,
}

/// Mirrors `VelocityDOM` (v1.0 kits only).
#[derive(Debug, Clone, PartialEq)]
pub struct VelocityDom {
    pub upper: f64,
    pub lower: f64,
    pub samplerefs: Vec<SampleRefDom>,
}

/// Mirrors `AudioFileDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFileDom {
    /// One of the instrument's own channels.
    pub instrument_channel: String,
    /// File relative to the instrument XML.
    pub file: String,
    /// 1-based file channel index as found in XML (converted to 0-based when
    /// constructing the runtime `AudioFile`).
    pub filechannel: usize,
}

/// Mirrors `SampleDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleDom {
    pub name: String,
    /// >= v2.0 kits only; 0.0 for v1.0.
    pub power: f64,
    /// >= v2.0 kits only.
    pub normalized: bool,
    pub audiofiles: Vec<AudioFileDom>,
}

/// Mirrors `InstrumentChannelDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct InstrumentChannelDom {
    pub name: String,
    pub main: MainState,
}

/// Mirrors `InstrumentDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct InstrumentDom {
    pub name: String,
    pub version: String,
    pub description: String,
    pub samples: Vec<SampleDom>,
    pub instrument_channels: Vec<InstrumentChannelDom>,
    /// v1.0 only: velocity groups overriding sample selection.
    pub velocities: Vec<VelocityDom>,
}

/// Mirrors `ChannelDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelDom {
    pub name: String,
}

/// Mirrors `ChannelMapDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMapDom {
    /// Instrument (left side) channel name.
    pub r#in: String,
    /// Kit (right side) channel name.
    pub out: String,
    pub main: MainState,
}

/// Mirrors `ChokeDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChokeDom {
    /// Swallowed instrument name.
    pub instrument: String,
    /// Milliseconds of the choke ramp.
    pub choketime: f64,
}

/// Mirrors `InstrumentRefDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct InstrumentRefDom {
    pub name: String,
    /// Instrument XML file relative to the kit.
    pub file: String,
    /// Optional "patibility/choke/mute group" label.
    pub group: String,
    pub channel_map: Vec<ChannelMapDom>,
    pub chokes: Vec<ChokeDom>,
}

/// Mirrors `ClickMapDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClickMapDom {
    pub instrument: String,
    pub colour: String,
}

/// Mirrors `MetadataDOM`.
#[derive(Debug, Clone, PartialEq)]
#[derive(Default)]
pub struct MetadataDom {
    pub version: String,
    pub title: String,
    pub description: String,
    pub license: String,
    pub notes: String,
    pub author: String,
    pub email: String,
    pub website: String,
    pub logo: String,
    /// Image source relative to the kit.
    pub image: String,
    /// Image map id (used by the original GUI for clickable images).
    pub image_map: String,
    pub clickmaps: Vec<ClickMapDom>,
    /// File name of the default `midimap.xml`.
    pub default_midimap_file: String,
}


/// Mirrors `DrumkitDOM`.
#[derive(Debug, Clone, PartialEq)]
pub struct DrumkitDom {
    /// `<drumkit version=...>`, defaults to "1.0".
    pub version: String,
    /// `<drumkit samplerate=...>`, defaults to 44100.0.
    pub samplerate: f64,
    pub metadata: MetadataDom,
    pub instruments: Vec<InstrumentRefDom>,
    pub channels: Vec<ChannelDom>,
}

/// Errors raised by the kit parsers. Message text contains the offending
/// element/attribute and, where available, a line number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

/// Mirrors upstream `VersionStr` comparison semantics enough to distinguish
/// v1.0 from >= v2.0 instruments.
pub fn version_ge(a: &str, b: &str) -> bool {
    parse_version(a) >= parse_version(b)
}

pub fn parse_version(v: &str) -> (u32, u32, u32) {
    let nums: Vec<u32> = v
        .split('.')
        .map(|p| p.trim().parse::<u32>().unwrap_or(0))
        .collect();
    (
        *nums.first().unwrap_or(&0),
        *nums.get(1).unwrap_or(&0),
        *nums.get(2).unwrap_or(&0),
    )
}

/// True for instrument versions that carry per-sample power ("2.0" and later).
pub fn has_power(version: &str) -> bool {
    version_ge(version, "2.0")
}

/// True for versions using velocity group tables ("1", "1.0", "1.0.0").
pub fn uses_velocities(version: &str) -> bool {
    let (major, minor, patch) = parse_version(version);
    major == 1 && minor == 0 && patch == 0 && !version.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(version_ge("2.0", "2.0"));
        assert!(version_ge("2.0", "1.99"));
        assert!(version_ge("2.1.0", "2.0"));
        assert!(!version_ge("1.0", "2.0"));
        assert!(uses_velocities("1.0"));
        assert!(uses_velocities("1.0.0"));
        assert!(!uses_velocities("2.0"));
        assert!(!uses_velocities(""));
        assert_eq!(parse_version("2.0"), (2, 0, 0));
    }
}
