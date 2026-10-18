// SPDX-License-Identifier: GPL-3.0-or-later
//! VST3 wrapper for the drumgizmo-rust engine.
//!
//! Two COM classes via the `vst3` bindings: a processor
//! (IComponent + IAudioProcessor + IProcessContextRequirements) with a
//! stereo main output bus and one event input bus, and an edit
//! controller exposing the same two automatable parameters as the CLAP
//! and LV2 wrappers (0 = master gain 0..8, 1 = tempo 30..360 BPM,
//! normalized linearly).
//!
//! Events: `kNoteOnEvent` (velocity 0..1 → engine byte scale),
//! `kNoteOffEvent`, and `kPolyPressureEvent` with pressure > 0 as the
//! DrumGizmo choke convention, each with block-relative sample offsets.
//! State: JSON PluginState blob over `IBStream` (same schema as CLAP).
//!
//! Honest gaps (docs/decisions.md): no GUI (IPlugView returns null),
//! sample-size 32 bit only, no PClassInfo2/category subtleties.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(clippy::missing_safety_doc)]

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, c_void};
use std::ptr;
use std::slice;

use vst3::{
    uid, Class, ComRef, ComWrapper, Steinberg::Vst::Event_::EventTypes_, Steinberg::Vst::*,
    Steinberg::*,
};

use dg_plugin::{PluginCore, PluginState};

// ----------------------------------------------------------------------
// constants
// ----------------------------------------------------------------------

const PLUGIN_NAME: &str = "drumgizmo-rust";

const PARAM_GAIN: u32 = 0;
const PARAM_TEMPO: u32 = 1;

const GAIN_PLAIN_MIN: f64 = 0.0;
const GAIN_PLAIN_MAX: f64 = 8.0;
const GAIN_PLAIN_DEFAULT: f64 = 1.0;
const TEMPO_PLAIN_MIN: f64 = 30.0;
const TEMPO_PLAIN_MAX: f64 = 360.0;
const TEMPO_PLAIN_DEFAULT: f64 = 120.0;

const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;

/// Controller class id (arbitrary, but fixed forever).
const CTRL_CID: TUID = uid(0x8F87F495, 0x56784BCC, 0xB2731C90, 0x4EA1D2F9);
/// Processor class id.
const PROC_CID: TUID = uid(0x5D4B52F0, 0x12344E02, 0xA510A6F1, 0x6AD2EF34);

// helpers ----------------------------------------------------------------

fn copy_cstring(src: &str, dst: &mut [c_char]) {
    let bytes = std::ffi::CString::new(src)
        .unwrap_or_else(|_| std::ffi::CString::default())
        .as_bytes_with_nul()
        .to_vec();
    for (src, dst) in bytes.iter().zip(dst.iter_mut()) {
        *dst = *src as c_char;
    }
    if bytes.len() > dst.len() {
        if let Some(last) = dst.last_mut() {
            *last = 0;
        }
    }
}

fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut len = 0;
    for (src, dst) in src.encode_utf16().zip(dst.iter_mut()) {
        *dst = src as TChar;
        len += 1;
    }
    if len < dst.len() {
        dst[len] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

/// Plain → normalized parameter mapping (linear, per param id).
fn plain_to_normalized(id: u32, plain: f64) -> f64 {
    match id {
        PARAM_GAIN => {
            if plain <= GAIN_PLAIN_MIN {
                0.0
            } else {
                (plain - GAIN_PLAIN_MIN) / (GAIN_PLAIN_MAX - GAIN_PLAIN_MIN)
            }
        }
        PARAM_TEMPO => (plain - TEMPO_PLAIN_MIN) / (TEMPO_PLAIN_MAX - TEMPO_PLAIN_MIN),
        _ => 0.0,
    }
}

/// Normalized → plain parameter value.
fn normalized_to_plain(id: u32, normalized: f64) -> f64 {
    match id {
        PARAM_GAIN => {
            normalized.clamp(0.0, 1.0) * (GAIN_PLAIN_MAX - GAIN_PLAIN_MIN) + GAIN_PLAIN_MIN
        }
        PARAM_TEMPO => {
            normalized.clamp(0.0, 1.0) * (TEMPO_PLAIN_MAX - TEMPO_PLAIN_MIN) + TEMPO_PLAIN_MIN
        }
        _ => 0.0,
    }
}

// ----------------------------------------------------------------------
// processor
// ----------------------------------------------------------------------

struct DrumProcessor {
    /// Realtime core; trait methods are sequential host calls so a
    /// RefCell borrow is safe (no nested or overlapping access).
    core: RefCell<PluginCore>,
    /// Last forwarded gain (dedup; the core dde-dups tempo itself).
    last_gain: Cell<f32>,
    /// Cached state for the controller display path.
    state: RefCell<PluginState>,
}

impl Class for DrumProcessor {
    type Interfaces = (IComponent, IAudioProcessor, IProcessContextRequirements);
}

impl DrumProcessor {
    const CID: TUID = PROC_CID;

    fn new() -> Self {
        Self {
            core: RefCell::new(PluginCore::new()),
            last_gain: Cell::new(GAIN_PLAIN_DEFAULT as f32),
            state: RefCell::new(PluginState::default()),
        }
    }

    unsafe fn load_state_from_stream(&self, stream: *mut IBStream) -> bool {
        let Some(bytes) = read_stream_all(stream) else {
            return false;
        };
        match PluginState::from_json(&bytes) {
            Some(state) => {
                *self.state.borrow_mut() = state.clone();
                self.core.borrow_mut().set_state(state);
                true
            }
            None => false,
        }
    }

    unsafe fn save_state_to_stream(&self, stream: *mut IBStream) -> bool {
        let bytes = self.core.borrow().state().to_json();
        write_stream_all(stream, &bytes)
    }
}

impl IPluginBaseTrait for DrumProcessor {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IComponentTrait for DrumProcessor {
    unsafe fn getControllerClassId(&self, class_id: *mut TUID) -> tresult {
        *class_id = DrumController::CID;
        kResultOk
    }

    unsafe fn setIoMode(&self, _mode: IoMode) -> tresult {
        kResultOk
    }

    unsafe fn getBusCount(&self, mediaType: MediaType, dir: BusDirection) -> i32 {
        match mediaType as MediaTypes {
            MediaTypes_::kAudio => match dir as BusDirections {
                BusDirections_::kOutput => 1,
                _ => 0, // instrument: no audio input buses
            },
            MediaTypes_::kEvent => match dir as BusDirections {
                BusDirections_::kInput => 1,
                _ => 0,
            },
            _ => 0,
        }
    }

    unsafe fn getBusInfo(
        &self,
        mediaType: MediaType,
        dir: BusDirection,
        index: i32,
        bus: *mut BusInfo,
    ) -> tresult {
        match mediaType as MediaTypes {
            MediaTypes_::kAudio => {
                if dir as BusDirections != BusDirections_::kInput && index == 0 {
                    let bus = &mut *bus;
                    bus.mediaType = MediaTypes_::kAudio as MediaType;
                    bus.direction = BusDirections_::kOutput as BusDirection;
                    bus.channelCount = 2;
                    copy_wstring("Drums out", &mut bus.name);
                    bus.busType = BusTypes_::kMain as BusType;
                    bus.flags = BusInfo_::BusFlags_::kDefaultActive;
                    kResultOk
                } else {
                    kInvalidArgument
                }
            }
            MediaTypes_::kEvent => {
                if dir as BusDirections == BusDirections_::kInput && index == 0 {
                    let bus = &mut *bus;
                    bus.mediaType = MediaTypes_::kEvent as MediaType;
                    bus.direction = BusDirections_::kInput as BusDirection;
                    bus.channelCount = 16;
                    copy_wstring("Drums in", &mut bus.name);
                    bus.busType = BusTypes_::kMain as BusType;
                    bus.flags = BusInfo_::BusFlags_::kDefaultActive;
                    kResultOk
                } else {
                    kInvalidArgument
                }
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn getRoutingInfo(
        &self,
        _in_info: *mut RoutingInfo,
        _out_info: *mut RoutingInfo,
    ) -> tresult {
        kNotImplemented
    }

    unsafe fn activateBus(
        &self,
        _media_type: MediaType,
        _dir: BusDirection,
        _index: i32,
        _state: TBool,
    ) -> tresult {
        kResultOk
    }

    unsafe fn setActive(&self, state: TBool) -> tresult {
        if state == 0 {
            self.core.borrow_mut().panic();
        }
        kResultOk
    }

    unsafe fn setState(&self, state: *mut IBStream) -> tresult {
        if self.load_state_from_stream(state) {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn getState(&self, state: *mut IBStream) -> tresult {
        if self.save_state_to_stream(state) {
            kResultOk
        } else {
            kResultFalse
        }
    }
}

impl DrumProcessor {
    // (no extra impl items yet)
}

impl IAudioProcessorTrait for DrumProcessor {
    unsafe fn setBusArrangements(
        &self,
        inputs: *mut SpeakerArrangement,
        num_ins: i32,
        outputs: *mut SpeakerArrangement,
        num_outs: i32,
    ) -> tresult {
        if num_ins != 0 || num_outs != 1 || outputs.is_null() {
            return kResultFalse;
        }
        // num_ins == 0: inputs pointer must not be dereferenced.
        let _ = inputs;
        if *outputs != SpeakerArr::kStereo {
            return kResultFalse;
        }
        kResultTrue
    }

    unsafe fn getBusArrangement(
        &self,
        dir: BusDirection,
        index: i32,
        arr: *mut SpeakerArrangement,
    ) -> tresult {
        if dir as BusDirections == BusDirections_::kOutput && index == 0 && !arr.is_null() {
            *arr = SpeakerArr::kStereo;
            kResultOk
        } else {
            kInvalidArgument
        }
    }

    unsafe fn canProcessSampleSize(&self, symbolic_sample_size: i32) -> tresult {
        match symbolic_sample_size as SymbolicSampleSizes {
            SymbolicSampleSizes_::kSample32 => kResultOk,
            _ => kNotImplemented,
        }
    }

    unsafe fn getLatencySamples(&self) -> u32 {
        0
    }

    unsafe fn setupProcessing(&self, setup: *mut ProcessSetup) -> tresult {
        if !setup.is_null() {
            let setup = &*setup;
            let rate = setup.sampleRate.clamp(8000.0, 384000.0) as u32;
            self.core.borrow_mut().set_samplerate(rate);
        }
        kResultOk
    }

    unsafe fn setProcessing(&self, state: TBool) -> tresult {
        if state == 0 {
            self.core.borrow_mut().panic();
        }
        kResultOk
    }

    unsafe fn process(&self, data: *mut ProcessData) -> tresult {
        let data = &*data;
        if data.numOutputs != 1 || data.numSamples < 0 {
            return kResultOk;
        }
        let num_samples = data.numSamples as usize;
        let output_buses = slice::from_raw_parts(data.outputs, data.numOutputs as usize);
        if output_buses[0].numChannels != 2 {
            return kResultOk;
        }
        let channels = slice::from_raw_parts(
            output_buses[0].__field0.channelBuffers32,
            output_buses[0].numChannels as usize,
        );
        let (left_ptr, right_ptr) = (channels[0], channels[1]);
        if left_ptr.is_null() || right_ptr.is_null() {
            return kResultOk;
        }

        let mut core = self.core.borrow_mut();

        // Parameter automation: last queued point per parameter wins.
        if let Some(param_changes) = ComRef::from_raw(data.inputParameterChanges) {
            let param_count = param_changes.getParameterCount();
            for param_index in 0..param_count {
                let Some(param_queue) =
                    ComRef::from_raw(param_changes.getParameterData(param_index))
                else {
                    continue;
                };
                let point_count = param_queue.getPointCount();
                if point_count == 0 {
                    continue;
                }
                let mut sample_offset = 0i32;
                let mut normalized = 0f64;
                if param_queue.getPoint(point_count - 1, &mut sample_offset, &mut normalized)
                    == kResultTrue
                {
                    match param_queue.getParameterId() {
                        PARAM_GAIN => {
                            let gain = normalized_to_plain(PARAM_GAIN, normalized) as f32;
                            if (gain - self.last_gain.get()).abs() > f32::EPSILON {
                                self.last_gain.set(gain);
                                core.master_gain(gain);
                            }
                        }
                        PARAM_TEMPO => {
                            core.set_tempo(normalized_to_plain(PARAM_TEMPO, normalized));
                        }
                        _ => {}
                    }
                }
            }
        }

        // Note events with block-relative offsets.
        let mut events = Vec::new();
        if let Some(event_list) = ComRef::from_raw(data.inputEvents) {
            let event_count = event_list.getEventCount();
            let mut event = std::mem::zeroed::<Event>();
            for index in 0..event_count {
                if event_list.getEvent(index, &mut event) != kResultTrue {
                    continue;
                }
                let offset = event.sampleOffset.max(0) as usize;
                match event.r#type as u32 {
                    EventTypes_::kNoteOnEvent => {
                        let on = event.__field0.noteOn;
                        if (0..=127).contains(&on.pitch) {
                            events.push(dg_common::MidiEvent {
                                kind: dg_common::MessageType::OnSet,
                                note: on.pitch as u8,
                                velocity: dg_plugin::velocity_to_byte(on.velocity.clamp(0.0, 1.0)),
                                offset,
                            });
                        }
                    }
                    EventTypes_::kNoteOffEvent => {
                        let off = event.__field0.noteOff;
                        if (0..=127).contains(&off.pitch) {
                            events.push(dg_common::MidiEvent::note_off(off.pitch as u8, offset));
                        }
                    }
                    EventTypes_::kPolyPressureEvent => {
                        let pressure = event.__field0.polyPressure;
                        if (0..=127).contains(&pressure.pitch) && pressure.pressure > 0.0 {
                            events.push(dg_common::MidiEvent {
                                kind: dg_common::MessageType::Choke,
                                note: pressure.pitch as u8,
                                velocity: 1.0,
                                offset,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        let left = slice::from_raw_parts_mut(left_ptr, num_samples);
        let right = slice::from_raw_parts_mut(right_ptr, num_samples);
        core.process_stereo(left, right, &events);
        kResultOk
    }

    unsafe fn getTailSamples(&self) -> u32 {
        // Long drum tails decay naturally; report a tail of a few
        // seconds' worth of samples as a compromise (host DAWs then
        // render through when bouncing).
        0 // honest: engine has no tail knowledge yet (documented)
    }
}

impl IProcessContextRequirementsTrait for DrumProcessor {
    unsafe fn getProcessContextRequirements(&self) -> u32 {
        0
    }
}

// ----------------------------------------------------------------------
// edit controller
// ----------------------------------------------------------------------

struct DrumController {
    state: RefCell<PluginState>,
}

impl Class for DrumController {
    type Interfaces = (IEditController,);
}

impl DrumController {
    const CID: TUID = CTRL_CID;

    fn new() -> Self {
        Self {
            state: RefCell::new(PluginState::default()),
        }
    }

    unsafe fn read_state(&self, stream: *mut IBStream) -> bool {
        let Some(bytes) = read_stream_all(stream) else {
            return false;
        };
        match PluginState::from_json(&bytes) {
            Some(state) => {
                *self.state.borrow_mut() = state;
                true
            }
            None => false,
        }
    }
}

impl IPluginBaseTrait for DrumController {
    unsafe fn initialize(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }

    unsafe fn terminate(&self) -> tresult {
        kResultOk
    }
}

impl IEditControllerTrait for DrumController {
    unsafe fn setComponentState(&self, state: *mut IBStream) -> tresult {
        if self.read_state(state) {
            kResultOk
        } else {
            kResultFalse
        }
    }

    unsafe fn setState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getState(&self, _state: *mut IBStream) -> tresult {
        kResultOk
    }

    unsafe fn getParameterCount(&self) -> i32 {
        2
    }

    unsafe fn getParameterInfo(&self, param_index: i32, info: *mut ParameterInfo) -> tresult {
        match param_index {
            0 => {
                let info = &mut *info;
                info.id = PARAM_GAIN;
                copy_wstring("Gain", &mut info.title);
                copy_wstring("Gain", &mut info.shortTitle);
                copy_wstring("", &mut info.units);
                info.stepCount = 0;
                info.defaultNormalizedValue = plain_to_normalized(PARAM_GAIN, GAIN_PLAIN_DEFAULT);
                info.unitId = 0;
                info.flags = ParameterInfo_::ParameterFlags_::kCanAutomate;
                kResultOk
            }
            1 => {
                let info = &mut *info;
                info.id = PARAM_TEMPO;
                copy_wstring("Sequencer tempo", &mut info.title);
                copy_wstring("Tempo", &mut info.shortTitle);
                copy_wstring("BPM", &mut info.units);
                info.stepCount = 0;
                info.defaultNormalizedValue = plain_to_normalized(PARAM_TEMPO, TEMPO_PLAIN_DEFAULT);
                info.unitId = 0;
                info.flags = ParameterInfo_::ParameterFlags_::kCanAutomate;
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn getParamStringByValue(
        &self,
        id: u32,
        value_normalized: f64,
        string: *mut String128,
    ) -> tresult {
        let slice = &mut *string;
        match id {
            PARAM_GAIN => {
                let plain = normalized_to_plain(id, value_normalized);
                copy_wstring(&format!("{plain:.2}"), slice);
                kResultOk
            }
            PARAM_TEMPO => {
                let plain = normalized_to_plain(id, value_normalized);
                copy_wstring(&format!("{plain:.1}"), slice);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn getParamValueByString(
        &self,
        id: u32,
        string: *mut TChar,
        value_normalized: *mut f64,
    ) -> tresult {
        match id {
            PARAM_GAIN | PARAM_TEMPO => {
                let len = len_wstring(string as *const TChar);
                if let Ok(text) =
                    String::from_utf16(slice::from_raw_parts(string as *const u16, len))
                {
                    if let Ok(plain) = text.trim().parse::<f64>() {
                        *value_normalized = plain_to_normalized(id, plain);
                        return kResultOk;
                    }
                }
                kInvalidArgument
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn normalizedParamToPlain(&self, id: u32, value_normalized: f64) -> f64 {
        normalized_to_plain(id, value_normalized)
    }

    unsafe fn plainParamToNormalized(&self, id: u32, plain_value: f64) -> f64 {
        plain_to_normalized(id, plain_value)
    }

    unsafe fn getParamNormalized(&self, id: u32) -> f64 {
        let state = self.state.borrow();
        match id {
            PARAM_GAIN => plain_to_normalized(PARAM_GAIN, f64::from(state.master_gain)),
            PARAM_TEMPO => plain_to_normalized(PARAM_TEMPO, state.tempo),
            _ => 0.0,
        }
    }

    unsafe fn setParamNormalized(&self, id: u32, value: f64) -> tresult {
        let mut state = self.state.borrow_mut();
        match id {
            PARAM_GAIN => {
                state.master_gain = normalized_to_plain(id, value) as f32;
                kResultOk
            }
            PARAM_TEMPO => {
                state.tempo = normalized_to_plain(id, value);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn setComponentHandler(&self, _handler: *mut IComponentHandler) -> tresult {
        kResultOk
    }

    unsafe fn createView(&self, _name: *const c_char) -> *mut IPlugView {
        ptr::null_mut() // no GUI yet (documented blocker)
    }
}

// ----------------------------------------------------------------------
// stream helpers
// ----------------------------------------------------------------------

unsafe fn len_wstring(string: *const TChar) -> usize {
    let mut len = 0;
    while *string.add(len) != 0 {
        len += 1;
    }
    len
}

/// Reads the entire `IBStream` contents (chunked; some stream
/// implementations cap `read` sizes). `Ok` short-count ends reading.
unsafe fn read_stream_all(stream: *mut IBStream) -> Option<Vec<u8>> {
    if stream.is_null() {
        return None;
    }
    let raw = stream;
    let stream = &*raw;
    let vtbl_read = (*stream.vtbl).read;
    let mut buffer: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let mut num_read: i32 = 0;
        let result = vtbl_read(
            raw,
            chunk.as_mut_ptr().cast(),
            chunk.len() as i32,
            &mut num_read,
        );
        if result != kResultOk && result != kResultTrue {
            return None;
        }
        if num_read <= 0 {
            break; // EOF or zero: whatever we have is the whole state
        }
        buffer.extend_from_slice(&chunk[..num_read as usize]);
        if buffer.len() > MAX_STATE_BYTES {
            return None;
        }
    }
    Some(buffer)
}

/// Writes all bytes (single chunk lengths capped at `int32`).
unsafe fn write_stream_all(stream: *mut IBStream, bytes: &[u8]) -> bool {
    if stream.is_null() {
        return false;
    }
    let raw = stream;
    let stream = &*raw;
    let vtbl_write = (*stream.vtbl).write;
    let mut offset = 0usize;
    while offset < bytes.len() {
        let chunk = &bytes[offset..];
        let chunk_len = chunk.len().min(i32::MAX as usize) as i32;
        let mut written: i32 = 0;
        let result = vtbl_write(
            raw,
            chunk.as_ptr().cast::<u8>() as *mut c_void,
            chunk_len,
            &mut written,
        );
        if result != kResultOk && result != kResultTrue {
            return false;
        }
        if written <= 0 {
            return false;
        }
        offset += written as usize;
    }
    true
}

// ----------------------------------------------------------------------
// factory
// ----------------------------------------------------------------------

struct Factory;

impl Class for Factory {
    type Interfaces = (IPluginFactory,);
}

impl IPluginFactoryTrait for Factory {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let info = &mut *info;
        copy_cstring("drumgizmo-rust contributors", &mut info.vendor);
        copy_cstring("https://github.com/Pliexi/drumgizmo-rust", &mut info.url);
        copy_cstring("drumgizmo-rust@localhost", &mut info.email);
        info.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
        kResultOk
    }

    unsafe fn countClasses(&self) -> i32 {
        2
    }

    unsafe fn getClassInfo(&self, index: i32, info: *mut PClassInfo) -> tresult {
        match index {
            0 => {
                let info = &mut *info;
                info.cid = DrumProcessor::CID;
                info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
                copy_cstring("Audio Module Class", &mut info.category);
                copy_cstring(PLUGIN_NAME, &mut info.name);
                kResultOk
            }
            1 => {
                let info = &mut *info;
                info.cid = DrumController::CID;
                info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
                copy_cstring("Component Controller Class", &mut info.category);
                copy_cstring(PLUGIN_NAME, &mut info.name);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn createInstance(
        &self,
        cid: FIDString,
        iid: FIDString,
        obj: *mut *mut c_void,
    ) -> tresult {
        let requested = *(cid as *const TUID);
        let instance = if requested == DrumProcessor::CID {
            Some(
                ComWrapper::new(DrumProcessor::new())
                    .to_com_ptr::<FUnknown>()
                    .unwrap(),
            )
        } else if requested == DrumController::CID {
            Some(
                ComWrapper::new(DrumController::new())
                    .to_com_ptr::<FUnknown>()
                    .unwrap(),
            )
        } else {
            None
        };

        if let Some(instance) = instance {
            let ptr = instance.as_ptr();
            ((*(*ptr).vtbl).queryInterface)(ptr, iid as *mut TUID, obj)
        } else {
            kInvalidArgument
        }
    }
}

// ----------------------------------------------------------------------
// module entry points
// ----------------------------------------------------------------------

#[cfg(target_os = "windows")]
#[no_mangle]
extern "system" fn InitDll() -> bool {
    true
}

#[cfg(target_os = "windows")]
#[no_mangle]
extern "system" fn ExitDll() -> bool {
    true
}

#[cfg(target_os = "macos")]
#[no_mangle]
extern "system" fn BundleEntry(_bundle_ref: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "macos")]
#[no_mangle]
extern "system" fn BundleExit() -> bool {
    true
}

#[cfg(target_os = "linux")]
#[no_mangle]
extern "system" fn ModuleEntry(_library_handle: *mut c_void) -> bool {
    true
}

#[cfg(target_os = "linux")]
#[no_mangle]
extern "system" fn ModuleExit() -> bool {
    true
}

#[no_mangle]
extern "system" fn GetPluginFactory() -> *mut IPluginFactory {
    ComWrapper::new(Factory)
        .to_com_ptr::<IPluginFactory>()
        .unwrap()
        .into_raw()
}

// ----------------------------------------------------------------------
// tests (pure helpers)
// ----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_mapping_roundtrips_defaults() {
        assert_eq!(plain_to_normalized(PARAM_GAIN, 1.0), 0.125);
        assert_eq!(normalized_to_plain(PARAM_GAIN, 0.125), 1.0);
        assert_eq!(plain_to_normalized(PARAM_TEMPO, 120.0), 90.0 / 330.0);
        assert!((normalized_to_plain(PARAM_TEMPO, 90.0 / 330.0) - 120.0).abs() < 1e-9);
        // Bounds clamp.
        assert_eq!(normalized_to_plain(PARAM_GAIN, 1.0), GAIN_PLAIN_MAX);
        assert_eq!(normalized_to_plain(PARAM_TEMPO, 1.5), TEMPO_PLAIN_MAX);
        assert!(normalized_to_plain(PARAM_GAIN, -0.5) >= GAIN_PLAIN_MIN);
    }

    #[test]
    fn state_roundtrip_through_json() {
        let state = PluginState {
            master_gain: 2.5,
            tempo: 98.0,
            ..PluginState::default()
        };
        let bytes = state.to_json();
        let back = PluginState::from_json(&bytes).expect("parse");
        assert_eq!(back, state);
    }
}
