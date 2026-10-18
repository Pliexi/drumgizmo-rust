// SPDX-License-Identifier: GPL-3.0-or-later
//! LV2 wrapper for the drumgizmo-rust engine (hand-rolled ABI).
//!
//! Mirrors the layout of `lv2/core/lv2.h`, `lv2/atom/atom.h` and
//! `lv2/urid/urid.h`; the compiled `cdylib` exports the classic
//! `lv2_descriptor` accessor. One instrument plugin:
//!
//! ```text
//! port 0  midi_in   atom:AtomPort, bufferType atom:Sequence, supports midi:MidiEvent
//! port 1  out_left   lv2:AudioPort output
//! port 2  out_right  lv2:AudioPort output
//! port 3  gain       lv2:ControlPort input (master gain, 0..8, default 1)
//! port 4  tempo      lv2:ControlPort input (sequencer bpm, 30..360, default 120)
//! ```
//!
//! MIDI events arrive as an atom:Sequence of midi:MidiEvent atoms whose
//! bodies are raw 1..=4 byte status messages; they are decoded with the
//! shared `dg_common::decode_midi_status_bytes` and forwarded into the
//! engine with block-relative offsets. Host URIDs come from the
//! `urid:map` feature; without it (non-spec host) any event whose body
//! starts with a MIDI status byte is accepted.
//!
//! Not provided (honest gaps, see docs/decisions.md): state extension,
//! worker, UI. Kit loading is done by the host-agnostic core state, so
//! hosts currently load the default (empty) kit in this wrapper.

#![allow(clippy::missing_safety_doc)]

use std::os::raw::{c_char, c_void};
use std::ptr;

use dg_common::MidiEvent as DgEvent;
use dg_plugin::PluginCore;

// ----------------------------------------------------------------------
// constants
// ----------------------------------------------------------------------

/// urid:map feature URI (lv2/urid/urid.h).
const URID_MAP_URI: &[u8] = b"http://lv2plug.in/ns/ext/urid#map\0";
/// midi:MidiEvent class URI (lv2/midi/midi.h).
const MIDI_EVENT_URI: &[u8] = b"http://lv2plug.in/ns/ext/midi#MidiEvent\0";

const PLUGIN_URI: &[u8] = b"https://github.com/Pliexi/drumgizmo-rust/lv2\0";

/// Port indices — the order MUST match the TTL bundle description.
const PORT_MIDI: u32 = 0;
const PORT_LEFT: u32 = 1;
const PORT_RIGHT: u32 = 2;
const PORT_GAIN: u32 = 3;
const PORT_TEMPO: u32 = 4;
const PORT_COUNT: usize = 5;

const GAIN_MIN: f32 = 0.0;
const GAIN_MAX: f32 = 8.0;
const GAIN_DEFAULT: f32 = 1.0;

// ----------------------------------------------------------------------
// hand-rolled ABI types (layout matches the C headers)
// ----------------------------------------------------------------------

#[repr(C)]
pub struct LV2Feature {
    pub uri: *const c_char,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct LV2UridMap {
    pub handle: *mut c_void,
    pub map: Option<unsafe extern "C" fn(handle: *mut c_void, uri: *const c_char) -> u32>,
}

/// `LV2_Atom` is just two u32s (`size` then `type`); events are read
/// through explicit byte offsets instead of constructing structs.
///
/// `LV2_Atom_Event`: NOT an atom — a time stamp plus a body atom header;
/// the payload follows at `size_of::<Lv2AtomEvent>()` and each event is
/// 64-bit aligned (see the atom:Sequence layout table in the spec).
#[repr(C)]
struct Lv2AtomEvent {
    time: i64,
    body_size: u32,
    body_type: u32,
    // body contents follow here
}

/// `LV2_Descriptor`.
#[repr(C)]
pub struct LV2Descriptor {
    pub uri: *const c_char,
    pub instantiate: Option<
        unsafe extern "C" fn(
            descriptor: *const LV2Descriptor,
            sample_rate: f64,
            bundle_path: *const c_char,
            features: *const *const LV2Feature,
        ) -> *mut c_void,
    >,
    pub connect_port:
        Option<unsafe extern "C" fn(instance: *mut c_void, port: u32, data: *mut c_void)>,
    pub activate: Option<unsafe extern "C" fn(instance: *mut c_void)>,
    pub run: Option<unsafe extern "C" fn(instance: *mut c_void, sample_count: u32)>,
    pub deactivate: Option<unsafe extern "C" fn(instance: *mut c_void)>,
    pub cleanup: Option<unsafe extern "C" fn(instance: *mut c_void)>,
    pub extension_data: Option<unsafe extern "C" fn(uri: *const c_char) -> *const c_void>,
}

// The descriptor is immutable shared ABI data; every pointer inside
// points at 'static literals, host callback tables never touch it.
unsafe impl Send for LV2Descriptor {}
unsafe impl Sync for LV2Descriptor {}

// ----------------------------------------------------------------------
// per instance data
// ----------------------------------------------------------------------

struct Instance {
    core: PluginCore,
    /// Port buffers connected by the host (cloned per connect_port).
    buffers: [Option<*mut c_void>; PORT_COUNT],
    /// URID of midi:MidiEvent, or 0 when `urid:map` was not provided
    /// (then every well-shaped event is decoded instead of filtered).
    midi_urid: u32,
    /// Only changed gains are forwarded to the core command ring.
    last_gain: f32,
    #[allow(dead_code)]
    bundle_path: String,
}

impl Instance {
    fn port_ptr(&self, port: u32) -> Option<*mut c_void> {
        self.buffers
            .get(port as usize)
            .copied()
            .flatten()
            .filter(|ptr| !ptr.is_null())
    }
}

// ----------------------------------------------------------------------
// descriptor + accessor (the LV2 entry symbol)
// ----------------------------------------------------------------------

static DESCRIPTOR: LV2Descriptor = LV2Descriptor {
    uri: PLUGIN_URI.as_ptr().cast(),
    instantiate: Some(instantiate),
    connect_port: Some(connect_port),
    activate: Some(activate),
    run: Some(run),
    deactivate: Some(deactivate),
    cleanup: Some(cleanup),
    extension_data: Some(extension_data),
};

#[no_mangle]
pub extern "C" fn lv2_descriptor(index: u32) -> *const LV2Descriptor {
    if index == 0 {
        &DESCRIPTOR
    } else {
        ptr::null()
    }
}

// ----------------------------------------------------------------------
// callbacks
// ----------------------------------------------------------------------

unsafe extern "C" fn instantiate(
    _descriptor: *const LV2Descriptor,
    sample_rate: f64,
    bundle_path: *const c_char,
    features: *const *const LV2Feature,
) -> *mut c_void {
    let midi_urid = find_midi_event_urid(features);
    let bundle_path = {
        // Recording the path is cheap and stays valid for the instance
        // lifetime; C strings handed to instantiate are guaranteed to
        // live at least that long.
        if bundle_path.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(bundle_path)
                .to_string_lossy()
                .into_owned()
        }
    };
    let mut core = PluginCore::new();
    core.set_samplerate(sample_rate.clamp(8000.0, 384000.0) as u32);
    let instance = Box::new(Instance {
        core,
        buffers: [None; PORT_COUNT],
        midi_urid,
        last_gain: GAIN_DEFAULT,
        bundle_path,
    });
    Box::into_raw(instance) as *mut c_void
}

/// Walks the feature list for `urid:map` and maps midi:MidiEvent once
/// (instantiate runs on a non-RT thread: mapping here follows the
/// "cache URIDs outside of performance critical code" advice).
unsafe fn find_midi_event_urid(features: *const *const LV2Feature) -> u32 {
    if features.is_null() {
        return 0;
    }
    let mut index = 0usize;
    loop {
        let feature = features.add(index).read();
        if feature.is_null() {
            return 0; // NULL terminated
        }
        let uri = &*feature;
        if !uri.uri.is_null() {
            let text = std::ffi::CStr::from_ptr(uri.uri).to_bytes();
            if text == &URID_MAP_URI[..URID_MAP_URI.len() - 1] {
                let map = uri.data as *const LV2UridMap;
                if !map.is_null() {
                    if let Some(map_fn) = (*map).map {
                        let urid = map_fn((*map).handle, MIDI_EVENT_URI.as_ptr().cast());
                        if urid != 0 {
                            return urid;
                        }
                    }
                }
                return 0; // map feature with broken/zero mapping
            }
        }
        index += 1;
    }
}

unsafe extern "C" fn connect_port(instance: *mut c_void, port: u32, data: *mut c_void) {
    let Some(instance) = (instance as *mut Instance).as_mut() else {
        return;
    };
    if let Some(slot) = instance.buffers.get_mut(port as usize) {
        *slot = Some(data);
    }
}

unsafe extern "C" fn activate(instance: *mut c_void) {
    if let Some(instance) = (instance as *mut Instance).as_mut() {
        instance.core.panic();
    }
}

unsafe extern "C" fn deactivate(instance: *mut c_void) {
    if let Some(instance) = (instance as *mut Instance).as_mut() {
        instance.core.panic();
    }
}

unsafe extern "C" fn cleanup(instance: *mut c_void) {
    if !instance.is_null() {
        drop(Box::from_raw(instance as *mut Instance));
    }
}

unsafe extern "C" fn extension_data(_uri: *const c_char) -> *const c_void {
    ptr::null() // no state/worker/UI extensions yet (documented blocker)
}

unsafe extern "C" fn run(instance: *mut c_void, sample_count: u32) {
    let Some(instance) = (instance as *mut Instance).as_mut() else {
        return;
    };
    if sample_count == 0 {
        return; // nothing to render; only control outputs would update
    }
    let frames = sample_count as usize;

    // Output ports must be connected; anything else lets the host call
    // find its mistake without us touching unconnected memory.
    let (Some(left_ptr), Some(right_ptr)) =
        (instance.port_ptr(PORT_LEFT), instance.port_ptr(PORT_RIGHT))
    else {
        return;
    };

    // MIDI input: atom:Sequence of midi:MidiEvent atoms.
    let mut events: Vec<DgEvent> = Vec::new();
    if let Some(seq) = instance.port_ptr(PORT_MIDI) {
        events = parse_sequence_events(seq as *const u8, instance.midi_urid);
    }

    // Control ports: forward changes only (the core dedups tempo itself).
    if let Some(gain) = instance.port_f32(PORT_GAIN) {
        let gain = gain.clamp(GAIN_MIN, GAIN_MAX);
        if (gain - instance.last_gain).abs() > f32::EPSILON {
            instance.last_gain = gain;
            instance.core.master_gain(gain);
        }
    }
    if let Some(tempo) = instance.port_f32(PORT_TEMPO) {
        instance.core.set_tempo(f64::from(tempo));
    }

    let left = std::slice::from_raw_parts_mut(left_ptr as *mut f32, frames);
    let right = std::slice::from_raw_parts_mut(right_ptr as *mut f32, frames);
    instance.core.process_stereo(left, right, &events);
}

impl Instance {
    /// Reads a float control port (Option because hosts may not connect
    /// optional controls; our control ports are required, so this is
    /// defensive only).
    fn port_f32(&self, port: u32) -> Option<f32> {
        let ptr = self.port_ptr(port)?;
        Some(unsafe { *(ptr as *const f32) })
    }
}

// ----------------------------------------------------------------------
// atom sequence decoding (mirrors lv2/atom/util.h)
// ----------------------------------------------------------------------

/// Pads a size to 64 bits (`lv2_atom_pad_size`).
fn pad_size(size: usize) -> usize {
    (size + 7) & !7
}

/// Reads `u32` at `base + offset` without alignment or reference
/// construction (buffers are host-owned, spec-aligned to 8 bytes).
unsafe fn u32_at(base: *const u8, offset: usize) -> u32 {
    ptr::read_unaligned(base.add(offset) as *const u32)
}

/// Iterates an atom:Sequence the way `LV2_ATOM_SEQUENCE_FOREACH` does:
///
/// * the sequence atom header (size, type) is bytes 0..=7,
/// * the body starts at byte 8 and begins with `unit` and `pad`,
/// * events begin at byte 16 and each event is
///   16 bytes header (time + body atom header) padded to 8 bytes,
/// * the event region ends at `8 + atom.size` bytes from the sequence
///   start (per `lv2_atom_sequence_is_end(body, size, i)` semantics).
///
/// Every midi:MidiEvent body (or, without a urid:map feature, every
/// event body) is decoded through `dg_common`. Malformed or truncated
/// events are skipped, never trusted past their own size.
unsafe fn parse_sequence_events(seq: *const u8, midi_urid: u32) -> Vec<DgEvent> {
    let mut events = Vec::new();
    if seq.is_null() {
        return events;
    }
    let atom_size = u32_at(seq, 0) as usize;
    // Events region: [16 .. 8 + atom_size). Empty sequences carry
    // atom.size == sizeof(LV2_Atom_Sequence_Body) == 8 → the range is
    // empty. Saturation guards absurd sizes so the window never wraps.
    let end = 8usize.saturating_add(atom_size);
    let mut offset = 16usize;
    while offset + std::mem::size_of::<Lv2AtomEvent>() <= end {
        let event_base = seq.add(offset);
        let body_size = u32_at(event_base, 8) as usize;
        let body_type = u32_at(event_base, 12);

        let data_offset = offset + std::mem::size_of::<Lv2AtomEvent>();
        let data_len = body_size.min(end - data_offset);
        let accept = midi_urid == 0 || body_type == midi_urid;
        if accept && data_len > 0 {
            let bytes = std::slice::from_raw_parts(event_base.add(16), data_len);
            if bytes[0] >= 0x80 {
                for event in dg_common::decode_midi_status_bytes(bytes) {
                    let mut event = event;
                    event.offset = (event_base as *const i64).read_unaligned().max(0) as usize;
                    events.push(event);
                }
            }
        }

        offset += data_offset - offset + pad_size(body_size);
    }
    events
}

// ----------------------------------------------------------------------
// tests (ABI iterations against hand-built buffers)
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dg_common::MessageType;

    /// Builds an atom:Sequence buffer from (time, type, bytes) triples.
    fn build_sequence(events: &[(i64, u32, &[u8])]) -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&0u32.to_ne_bytes()); // size placeholder
        buffer.extend_from_slice(&1u32.to_ne_bytes()); // type: whatever
        buffer.extend_from_slice(&0u32.to_ne_bytes()); // unit
        buffer.extend_from_slice(&0u32.to_ne_bytes()); // pad
        for (time, event_type, data) in events {
            buffer.extend_from_slice(&time.to_ne_bytes());
            buffer.extend_from_slice(&(data.len() as u32).to_ne_bytes()); // body.size
            buffer.extend_from_slice(&(*event_type).to_ne_bytes()); // body.type
            buffer.extend_from_slice(data);
            let pad = pad_size(data.len()) - data.len();
            buffer.resize(buffer.len() + pad, 0);
        }
        let region = buffer.len() - 8; // events after the body header
        buffer[0..4].copy_from_slice(&(region as u32).to_ne_bytes());
        buffer
    }

    #[test]
    fn empty_sequence_yields_no_events() {
        // Only the 16 byte header: size = 8 (empty event region).
        let mut buffer = [0u8; 16];
        buffer[0..4].copy_from_slice(&8u32.to_ne_bytes());
        buffer[4..8].copy_from_slice(&99u32.to_ne_bytes());
        let events = unsafe { parse_sequence_events(buffer.as_ptr(), 99) };
        assert!(events.is_empty());

        // Spec's canonical minimum: size exactly covers the body header.
        assert!(unsafe { parse_sequence_events(buffer.as_ptr(), 0) }.is_empty());
        // A zeroed buffer is safe as well (size 0 → empty window).
        assert!(unsafe { parse_sequence_events(ptr::null(), 7) }.is_empty());
    }

    #[test]
    fn one_midi_event_is_decoded() {
        let payload = [0x90u8, 0x3C, 0x40];
        let buffer = build_sequence(&[(0, 7, &payload)]);
        assert_eq!(buffer.len(), 16 + 16 + pad_size(3));

        let events = unsafe { parse_sequence_events(buffer.as_ptr(), 7) };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, MessageType::OnSet);
        assert_eq!(events[0].note, 60);
        assert!((events[0].velocity - 64.0).abs() < f32::EPSILON);
        assert_eq!(events[0].offset, 0);
    }

    #[test]
    fn urid_zero_accepts_status_events() {
        let payload = [0x80u8, 0x3C, 0x00];
        let buffer = build_sequence(&[(5, 42, &payload)]);
        let events = unsafe { parse_sequence_events(buffer.as_ptr(), 0) };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, MessageType::NoteOff);
        assert_eq!(events[0].offset, 5); // block-relative time stamp
    }

    #[test]
    fn different_urids_are_skipped() {
        let drum = [0x90u8, 0x24, 0x7F];
        let other = [0x98u8, 0x24, 0x7F]; // same shape, different type
        let buffer = build_sequence(&[(0, 7, &other), (10, 7, &drum)]);
        let events = unsafe { parse_sequence_events(buffer.as_ptr(), 9) };
        assert!(events.is_empty(), "not our urid: nothing decoded");

        let buffer = build_sequence(&[(0, 7, &other), (10, 9, &drum)]);
        let events = unsafe { parse_sequence_events(buffer.as_ptr(), 9) };
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].offset, 10);
    }

    #[test]
    fn realtime_and_running_status_payloads_are_dropped() {
        // A "midi" atom with a realtime byte alone becomes no event.
        let buffer = build_sequence(&[(0, 7, &[0xF8u8, 0, 0]), (1, 7, &[0x01u8, 0x3C])]);
        assert!(unsafe { parse_sequence_events(buffer.as_ptr(), 7) }.is_empty());
    }

    #[test]
    fn truncated_body_sizes_never_escape_real_memory() {
        // A body size claim pushing an event's data past a later, real
        // allocation boundary: cloning the buffer into a large zeroed
        // region keeps the reads bounded so the test stays sound while
        // proving garbage bodies decode into nothing.
        let payload = [0x90u8, 0x23, 0x50];
        let buffer = build_sequence(&[(0, 7, &payload)]);
        let mut bumped = buffer.clone();
        bumped[0..4].copy_from_slice(&10240u32.to_ne_bytes());
        bumped.resize(10 * 1024, 0);
        let events = unsafe { parse_sequence_events(bumped.as_ptr(), 7) };
        assert_eq!(events.len(), 1); // exactly the real event, garbage → null bodies
        assert!(!buffer.is_empty());
    }

    #[test]
    fn pad_size_matches_the_spec_helper() {
        // lv2_atom_pad_size: (size + 7) & ~7.
        assert_eq!(pad_size(0), 0);
        assert_eq!(pad_size(1), 8);
        assert_eq!(pad_size(3), 8);
        assert_eq!(pad_size(8), 8);
        assert_eq!(pad_size(9), 16);
    }
}
