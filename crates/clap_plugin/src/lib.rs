// SPDX-License-Identifier: GPL-3.0-or-later
//! CLAP wrapper for the drumgizmo-rust engine.
//!
//! Exposes the `clap_entry` symbol: a factory with one instrument stereo
//! plugin. The realtime engine of dg-plugin runs inside `process` on the
//! host audio thread; events (CLAP dialect notes, raw MIDI bytes and
//! parameter changes) are forwarded into the engine with block-relative
//! offsets; fixed block-accurate event handling only (no per-sample
//! interpolation).
//!
//! Parameters: 0 = master gain, 1 = tempo (used by the sequencer).
//! State extension saves/loads the full PluginState JSON blob.

#![allow(clippy::missing_safety_doc)]

use std::os::raw::{c_char, c_void};
use std::ptr;

use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    clap_event_header, clap_event_midi, clap_event_note, clap_event_param_value,
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_CHOKE, CLAP_EVENT_NOTE_OFF,
    CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_STEREO,
};
use clap_sys::ext::note_ports::{
    clap_note_port_info, clap_plugin_note_ports, CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP,
    CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI_MPE,
};
use clap_sys::ext::params::{
    clap_param_info, clap_plugin_params, CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE,
};
use clap_sys::ext::state::{clap_plugin_state, CLAP_EXT_STATE};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::id::{clap_id, CLAP_INVALID_ID};
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{
    clap_process, clap_process_status, CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR,
    CLAP_PROCESS_SLEEP,
};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;

use dg_common::{MessageType, MidiEvent as DgEvent};
use dg_plugin::{PluginCore, PluginState};

// ----------------------------------------------------------------------
// constants
// ----------------------------------------------------------------------

const PLUGIN_ID: &[u8] = b"com.pliexi.drumgizmo-rust\0";
const PLUGIN_NAME: &[u8] = b"drumgizmo-rust\0";
const PLUGIN_VENDOR: &[u8] = b"drumgizmo-rust\0";
const PLUGIN_URL: &[u8] = b"https://github.com/Pliexi/drumgizmo-rust\0";
const PLUGIN_VERSION: &[u8] = b"0.1.0\0";
const PLUGIN_DESCRIPTION: &[u8] =
    b"DrumGizmo re-implementation in Rust (kit sampler + sequencer)\0";

const PARAM_MASTER_GAIN: clap_id = 0;
const PARAM_TEMPO: clap_id = 1;

const MASTER_GAIN_MIN: f64 = 0.0;
const MASTER_GAIN_MAX: f64 = 8.0;
const MASTER_GAIN_DEFAULT: f64 = 1.0;
const TEMPO_MIN: f64 = 30.0;
const TEMPO_MAX: f64 = 360.0;
const TEMPO_DEFAULT: f64 = 120.0;

/// Maximum accepted state blob (host driven streams are unbounded).
const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;

/// Note ports are one drum input; audio ports one main stereo pair.
const NOTE_PORT_ID: clap_id = 1;
const AUDIO_PORT_ID: clap_id = 2;

// ----------------------------------------------------------------------
// small ABI helpers
// ----------------------------------------------------------------------

const fn cstr(bytes: &[u8]) -> *const c_char {
    bytes.as_ptr().cast()
}

/// `[*const c_char]` feature lists are not `Sync` on raw pointers; the
/// wrapper is honest because every entry points at a `static` byte
/// literal that outlives the process.
struct FeatureList([*const c_char; 3]);
unsafe impl Send for FeatureList {}
unsafe impl Sync for FeatureList {}

static FEATURES: FeatureList = FeatureList([
    b"instrument\0".as_ptr().cast(),
    b"stereo\0".as_ptr().cast(),
    ptr::null(),
]);

static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: cstr(PLUGIN_ID),
    name: cstr(PLUGIN_NAME),
    vendor: cstr(PLUGIN_VENDOR),
    url: cstr(PLUGIN_URL),
    manual_url: ptr::null(),
    support_url: cstr(PLUGIN_URL),
    version: cstr(PLUGIN_VERSION),
    description: cstr(PLUGIN_DESCRIPTION),
    features: FEATURES.0.as_ptr(),
};

/// Copies `text` (must include the NUL) as C characters, truncating at
/// one byte before the end of the buffer per the CLAP name conventions.
fn copy_cstr(dst: &mut [c_char], text: &[u8]) {
    let writable = dst.len() - 1; // keep the final byte for the NUL
    for (index, slot) in dst.iter_mut().enumerate().take(writable) {
        *slot = text.get(index).copied().unwrap_or(0) as c_char;
    }
    *dst.last_mut().expect("name buffers are never empty") = 0;
}

// ----------------------------------------------------------------------
// per instance data
// ----------------------------------------------------------------------

struct Instance {
    core: PluginCore,
    /// Host pointer (not owned).
    #[allow(dead_code)]
    host: *const clap_host,
}

unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

fn instance_of<'a>(plugin: *const clap_plugin) -> Option<&'a mut Instance> {
    unsafe {
        if plugin.is_null() || (*plugin).plugin_data.is_null() {
            return None;
        }
        Some(&mut *((*plugin).plugin_data as *mut Instance))
    }
}

// ----------------------------------------------------------------------
// clap_plugin callbacks
// ----------------------------------------------------------------------

static PLUGIN_VTABLE: clap_plugin = clap_plugin {
    desc: &DESCRIPTOR,
    plugin_data: ptr::null_mut(),
    init: Some(plugin_init),
    destroy: Some(plugin_destroy),
    activate: Some(plugin_activate),
    deactivate: Some(plugin_deactivate),
    start_processing: Some(plugin_start_processing),
    stop_processing: Some(plugin_stop_processing),
    reset: Some(plugin_reset),
    process: Some(plugin_process),
    get_extension: Some(plugin_get_extension),
    on_main_thread: Some(plugin_on_main_thread),
};

unsafe extern "C" fn plugin_init(plugin: *const clap_plugin) -> bool {
    // The instance was attached right at creation; init only validates.
    instance_of(plugin).is_some()
}

unsafe extern "C" fn plugin_destroy(plugin: *const clap_plugin) {
    if let Some(plugin) = (plugin as *mut clap_plugin).as_mut() {
        let data = plugin.plugin_data;
        plugin.plugin_data = ptr::null_mut();
        drop(Box::from_raw(data as *mut Instance));
    }
}

unsafe extern "C" fn plugin_activate(
    plugin: *const clap_plugin,
    sample_rate: f64,
    _min_frames_count: u32,
    _max_frames_count: u32,
) -> bool {
    let Some(instance) = instance_of(plugin) else {
        return false;
    };
    let rate = sample_rate.clamp(8000.0, 384000.0) as u32;
    instance.core.set_samplerate(rate);
    true
}

unsafe extern "C" fn plugin_deactivate(_plugin: *const clap_plugin) {}

unsafe extern "C" fn plugin_start_processing(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn plugin_stop_processing(_plugin: *const clap_plugin) {}

unsafe extern "C" fn plugin_reset(plugin: *const clap_plugin) {
    if let Some(instance) = instance_of(plugin) {
        instance.core.panic();
    }
}

unsafe extern "C" fn plugin_get_extension(
    _plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    let id = std::ffi::CStr::from_ptr(id);
    if id == CLAP_EXT_PARAMS {
        &PARAMS_EXT as *const clap_plugin_params as *const c_void
    } else if id == CLAP_EXT_STATE {
        &STATE_EXT as *const clap_plugin_state as *const c_void
    } else if id == CLAP_EXT_NOTE_PORTS {
        &NOTE_PORTS_EXT as *const clap_plugin_note_ports as *const c_void
    } else if id == CLAP_EXT_AUDIO_PORTS {
        &AUDIO_PORTS_EXT as *const clap_plugin_audio_ports as *const c_void
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn plugin_on_main_thread(_plugin: *const clap_plugin) {}

// ----------------------------------------------------------------------
// process
// ----------------------------------------------------------------------

unsafe extern "C" fn plugin_process(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> clap_process_status {
    let Some(instance) = instance_of(plugin) else {
        return CLAP_PROCESS_ERROR;
    };
    let process = &*process;
    let frames = process.frames_count as usize;

    // Hosts that connect a mono bus still expect audio; fewer than two
    // channels cannot be honoured by the stereo-only engine, so the
    // block is skipped (lossless: nothing was played into it either).
    let Some((left, right)) = output_stereo_pair(process, frames) else {
        return CLAP_PROCESS_SLEEP;
    };

    // Collect realtime events in block-relative form; parameter events
    // are applied to the core right here (see Instance docs).
    let mut events: Vec<DgEvent> = Vec::new();
    if !process.in_events.is_null() {
        let list = &*process.in_events;
        if let (Some(size), Some(get)) = (list.size, list.get) {
            let count = size(list);
            for index in 0..count {
                let header = get(list, index);
                if header.is_null() {
                    continue;
                }
                events_in_block(instance, header, &mut events);
            }
        }
    }

    instance.core.process_stereo(left, right, &events);
    CLAP_PROCESS_CONTINUE
}

/// Reads the first main output bus as a pair of mutable 32-bit channel
/// slices; per CLAP the stereo port packs two pointers in `data32`.
unsafe fn output_stereo_pair<'a>(
    process: &clap_process,
    frames: usize,
) -> Option<(&'a mut [f32], &'a mut [f32])> {
    if process.audio_outputs.is_null() || process.audio_outputs_count == 0 || frames == 0 {
        return None;
    }
    let buffers = std::slice::from_raw_parts(process.audio_outputs, 1);
    let buffer = &buffers[0];
    let data32 = buffer.data32;
    if data32.is_null() || buffer.channel_count < 2 {
        return None;
    }
    let left = std::slice::from_raw_parts_mut(*data32, frames);
    let right = std::slice::from_raw_parts_mut(*data32.add(1), frames);
    Some((left, right))
}

/// Translates one host event into engine events. Block relative offsets
/// are clamped into the block by the engine itself.
unsafe fn events_in_block(
    instance: &mut Instance,
    header: *const clap_event_header,
    out: &mut Vec<DgEvent>,
) {
    if (*header).space_id != CLAP_CORE_EVENT_SPACE_ID {
        return; // vendor spaces are passed through untouched
    }
    let offset = (*header).time as usize;
    match (*header).type_ {
        CLAP_EVENT_NOTE_ON => {
            let note = &*(header as *const clap_event_note);
            if !(0..=127).contains(&note.key) {
                return;
            }
            // CLAP velocity is 0..=1; the engine consumes the 1..=127
            // byte scale it gets from MIDI in.
            out.push(DgEvent {
                kind: MessageType::OnSet,
                note: note.key as u8,
                velocity: dg_plugin::velocity_to_byte(note.velocity.clamp(0.0, 1.0) as f32),
                offset,
            });
        }
        CLAP_EVENT_NOTE_OFF => {
            let note = &*(header as *const clap_event_note);
            if (0..=127).contains(&note.key) {
                out.push(DgEvent {
                    kind: MessageType::NoteOff,
                    note: note.key as u8,
                    velocity: 0.0,
                    offset,
                });
            }
        }
        CLAP_EVENT_NOTE_CHOKE => {
            let note = &*(header as *const clap_event_note);
            if (0..=127).contains(&note.key) {
                out.push(DgEvent {
                    kind: MessageType::Choke,
                    note: note.key as u8,
                    velocity: 1.0,
                    offset,
                });
            }
        }
        CLAP_EVENT_MIDI => {
            let midi = &*(header as *const clap_event_midi);
            // The atom/MIDI byte payload shares the audio_io semantics;
            // the helper lives in dg-common now.
            for event in dg_common::decode_midi_status_bytes(&midi.data) {
                let mut event = event;
                event.offset = offset;
                out.push(event);
            }
        }
        CLAP_EVENT_PARAM_VALUE => {
            let event = &*(header as *const clap_event_param_value);
            match event.param_id {
                PARAM_MASTER_GAIN => instance.core.master_gain(event.value as f32),
                PARAM_TEMPO => instance.core.set_tempo(event.value),
                _ => {}
            }
        }
        _ => {}
    }
}

// Raw MIDI decoding is shared: see `dg_common::decode_midi_status_bytes`,
// used directly in the CLAP_EVENT_MIDI branch above and in tests below.

// ----------------------------------------------------------------------
// params extension
// ----------------------------------------------------------------------

static PARAMS_EXT: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: None,
    text_to_value: None,
    flush: None,
};

unsafe extern "C" fn params_count(_plugin: *const clap_plugin) -> u32 {
    2
}

unsafe extern "C" fn params_get_info(
    _plugin: *const clap_plugin,
    param_index: u32,
    info: *mut clap_param_info,
) -> bool {
    if info.is_null() {
        return false;
    }
    let slot = &mut *info;
    // The host receives a raw struct: every field must be initialised.
    copy_cstr(&mut slot.module, b"\0");
    match param_index {
        0 => {
            slot.id = PARAM_MASTER_GAIN;
            slot.flags = CLAP_PARAM_IS_AUTOMATABLE;
            slot.cookie = ptr::null_mut();
            copy_cstr(&mut slot.name, b"Master gain\0");
            slot.min_value = MASTER_GAIN_MIN;
            slot.max_value = MASTER_GAIN_MAX;
            slot.default_value = MASTER_GAIN_DEFAULT;
            true
        }
        1 => {
            slot.id = PARAM_TEMPO;
            slot.flags = CLAP_PARAM_IS_AUTOMATABLE;
            copy_cstr(&mut slot.name, b"Sequencer tempo\0");
            slot.min_value = TEMPO_MIN;
            slot.max_value = TEMPO_MAX;
            slot.default_value = TEMPO_DEFAULT;
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn params_get_value(
    plugin: *const clap_plugin,
    param_id: clap_id,
    out_value: *mut f64,
) -> bool {
    let Some(instance) = instance_of(plugin) else {
        return false;
    };
    let state = instance.core.state();
    let value = match param_id {
        PARAM_MASTER_GAIN => f64::from(state.master_gain),
        PARAM_TEMPO => state.tempo,
        _ => return false,
    };
    *out_value = value;
    true
}

// ----------------------------------------------------------------------
// state extension
// ----------------------------------------------------------------------

static STATE_EXT: clap_plugin_state = clap_plugin_state {
    save: Some(state_save),
    load: Some(state_load),
};

unsafe extern "C" fn state_save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
    let Some(instance) = instance_of(plugin) else {
        return false;
    };
    let bytes = instance.core.state().to_json();
    write_all(stream, &bytes)
}

unsafe extern "C" fn state_load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
    let Some(instance) = instance_of(plugin) else {
        return false;
    };
    // The stream has no length hint: read chunks until EOF or error.
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match read_some(stream, &mut chunk) {
            read if read > 0 => buffer.extend_from_slice(&chunk[..read as usize]),
            0 => break, // EOF
            _ => return false,
        }
        if buffer.len() > MAX_STATE_BYTES {
            return false; // absurdly large state blob
        }
    }
    match PluginState::from_json(&buffer) {
        Some(state) => {
            instance.core.set_state(state);
            true
        }
        None => false,
    }
}

unsafe fn write_all(stream: *const clap_ostream, bytes: &[u8]) -> bool {
    let stream = &*stream;
    let Some(writer) = stream.write else {
        return false;
    };
    let mut written = 0usize;
    while written < bytes.len() {
        let chunk = &bytes[written..];
        let wrote = writer(stream, chunk.as_ptr().cast(), chunk.len() as u64);
        if wrote <= 0 {
            return false;
        }
        written += wrote as usize;
    }
    true
}

unsafe fn read_some(stream: *const clap_istream, buffer: &mut [u8]) -> i64 {
    let stream = &*stream;
    match stream.read {
        Some(reader) => reader(stream, buffer.as_mut_ptr().cast(), buffer.len() as u64),
        None => -1,
    }
}

// ----------------------------------------------------------------------
// note ports / audio ports extensions
// ----------------------------------------------------------------------

static NOTE_PORTS_EXT: clap_plugin_note_ports = clap_plugin_note_ports {
    count: Some(note_ports_count),
    get: Some(note_port_get),
};

unsafe extern "C" fn note_ports_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
    if is_input {
        1
    } else {
        0
    }
}

unsafe extern "C" fn note_port_get(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    if !is_input || index != 0 || info.is_null() {
        return false;
    }
    let slot = &mut *info;
    slot.id = NOTE_PORT_ID;
    slot.supported_dialects =
        CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI | CLAP_NOTE_DIALECT_MIDI_MPE;
    slot.preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
    copy_cstr(&mut slot.name, b"Drums in\0");
    true
}

static AUDIO_PORTS_EXT: clap_plugin_audio_ports = clap_plugin_audio_ports {
    count: Some(audio_ports_count),
    get: Some(audio_port_get),
};

unsafe extern "C" fn audio_ports_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
    if is_input {
        0
    } else {
        1
    }
}

unsafe extern "C" fn audio_port_get(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    if is_input || index != 0 || info.is_null() {
        return false;
    }
    let slot = &mut *info;
    slot.id = AUDIO_PORT_ID;
    slot.flags = CLAP_AUDIO_PORT_IS_MAIN;
    slot.channel_count = 2;
    slot.port_type = CLAP_PORT_STEREO.as_ptr();
    slot.in_place_pair = CLAP_INVALID_ID;
    copy_cstr(&mut slot.name, b"Main\0");
    true
}

// ----------------------------------------------------------------------
// factory + entry
// ----------------------------------------------------------------------

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_plugin_count),
    get_plugin_descriptor: Some(factory_get_descriptor),
    create_plugin: Some(factory_create_plugin),
};

unsafe extern "C" fn factory_plugin_count(_factory: *const clap_plugin_factory) -> u32 {
    1
}

unsafe extern "C" fn factory_get_descriptor(
    _factory: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    if index == 0 {
        &DESCRIPTOR
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn factory_create_plugin(
    _factory: *const clap_plugin_factory,
    host: *const clap_host,
    plugin_id: *const c_char,
) -> *const clap_plugin {
    if host.is_null() || plugin_id.is_null() {
        return ptr::null();
    }
    if PLUGIN_ID.strip_suffix(&[0]).unwrap_or(PLUGIN_ID)
        != std::ffi::CStr::from_ptr(plugin_id).to_bytes()
    {
        return ptr::null();
    }
    let instance = Box::new(Instance {
        core: PluginCore::new(),
        host,
    });
    let mut plugin = Box::new(PLUGIN_VTABLE);
    plugin.plugin_data = Box::into_raw(instance) as *mut c_void;
    Box::into_raw(plugin)
}

#[no_mangle]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_get_factory),
};

unsafe extern "C" fn entry_init(_plugin_path: *const c_char) -> bool {
    // No global resources: nothing to wire up, always true (CLAP spec:
    // return true unless the entry failed to initialize).
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn entry_get_factory(factory_id: *const c_char) -> *const c_void {
    if factory_id.is_null() {
        return ptr::null();
    }
    let id = std::ffi::CStr::from_ptr(factory_id);
    if id == CLAP_PLUGIN_FACTORY_ID {
        &FACTORY as *const clap_plugin_factory as *const c_void
    } else {
        ptr::null()
    }
}

// ----------------------------------------------------------------------
// tests (ABI structures can only be exercised through a host; the pure
// helpers are tested here)
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::string_sizes::CLAP_NAME_SIZE;

    #[test]
    fn raw_midi_matches_audio_io_semantics() {
        let on = dg_common::decode_midi_status_bytes(&[0x90, 36, 0x7F]);
        assert_eq!(on.len(), 1);
        assert_eq!(on[0].kind, MessageType::OnSet);
        assert_eq!(on[0].note, 36);
        assert!((on[0].velocity - 127.0).abs() < f32::EPSILON);

        let off = dg_common::decode_midi_status_bytes(&[0x80, 36, 0]);
        assert_eq!(off[0].kind, MessageType::NoteOff);

        // Polyphonic pressure doubles as choke when data1 > 0.
        let choke = dg_common::decode_midi_status_bytes(&[0xA0, 40, 1]);
        assert_eq!(choke[0].kind, MessageType::Choke);

        // Realtime and other statuses are dropped. A truncated message
        // keeps the audio_io semantics: payload bytes default to zero.
        assert!(dg_common::decode_midi_status_bytes(&[0xF8, 0, 0]).is_empty());
        assert!(dg_common::decode_midi_status_bytes(&[0xB0, 64, 127]).is_empty());
        assert!(dg_common::decode_midi_status_bytes(&[]).is_empty());
        let truncated = dg_common::decode_midi_status_bytes(&[0x90]);
        assert_eq!(truncated[0].kind, MessageType::OnSet);
        assert_eq!(truncated[0].note, 0);
        assert!((truncated[0].velocity).abs() < f32::EPSILON);
    }

    #[test]
    fn copy_cstr_truncates_and_null_terminates() {
        let mut short = [0 as c_char; CLAP_NAME_SIZE];
        copy_cstr(&mut short, b"Main\0");
        assert_eq!(short[4], 0);
        assert_eq!(short[5], 0); // zeroed tail
        assert_eq!(short[0], b'M' as c_char);

        // Truncate a name longer than the buffer, keeping the last byte NUL.
        let mut tiny = [0 as c_char; 3];
        copy_cstr(&mut tiny, b"abcdefg\0");
        assert_eq!(tiny[0], b'a' as c_char);
        assert_eq!(tiny[1], b'b' as c_char);
        assert_eq!(tiny[2], 0);
    }

    #[test]
    fn descriptor_strings_are_nul_terminated() {
        for ptr in [DESCRIPTOR.id, DESCRIPTOR.name, DESCRIPTOR.vendor] {
            let text = unsafe { std::ffi::CStr::from_ptr(ptr) };
            assert!(!text.to_bytes().is_empty());
        }
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(DESCRIPTOR.id) }.to_bytes(),
            b"com.pliexi.drumgizmo-rust"
        );
        // The feature list is a NUL-terminated table of NUL strings.
        let mut count = 0;
        unsafe {
            while !(*DESCRIPTOR.features.add(count)).is_null() {
                count += 1;
            }
        }
        assert_eq!(count, 2); // instrument, stereo
    }

    #[test]
    fn entry_and_factory_shapes_are_consistent() {
        unsafe {
            assert!(entry_init(ptr::null()));
            let factory =
                entry_get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) as *const clap_plugin_factory;
            assert!(!factory.is_null());
            let count_fn = (*factory).get_plugin_count.expect("count callback set");
            assert_eq!(count_fn(&*factory), 1);

            let descriptor_fn = (*factory)
                .get_plugin_descriptor
                .expect("descriptor callback set");
            assert_eq!(descriptor_fn(&*factory, 0), &DESCRIPTOR);
            assert!(descriptor_fn(&*factory, 1).is_null());
        }
    }

    #[test]
    fn params_info_covers_both_ids() {
        unsafe {
            let mut info = std::mem::MaybeUninit::<clap_param_info>::zeroed().assume_init();
            assert!(params_get_info(ptr::null(), 0, &mut info));
            assert_eq!(info.id, PARAM_MASTER_GAIN);
            assert_eq!(info.min_value, MASTER_GAIN_MIN);
            assert_eq!(info.max_value, MASTER_GAIN_MAX);
            assert!(params_get_info(ptr::null(), 1, &mut info));
            assert_eq!(info.id, PARAM_TEMPO);
            assert_eq!(info.default_value, TEMPO_DEFAULT);
            assert!(!params_get_info(ptr::null(), 2, &mut info));
        }
    }
}
