// SPDX-License-Identifier: GPL-3.0-or-later
//! Kit loading pipeline, ported from `src/drumkitloader.cc` and
//! `src/domloader.cc`.
//!
//! The original loads kits on a background thread with progress callbacks and
//! publishes an immutable kit object; the realtime side swaps an atomic
//! pointer to it. We keep exactly that structure:
//!
//! * [`KitLoader::start`] spawns the loading thread
//! * [`KitLoader::poll`] receives the finished kit (or an error)
//! * [`KitLoader::progress`] reads live load progress for the UI

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc};

use dg_common::NO_CHANNEL;

use crate::decoder;
use crate::dom::{DrumkitDom, InstrumentDom, MainState};
use crate::parser;

/// A single decoded audio file shared by all samples referencing it.
#[derive(Debug)]
pub struct AudioFileData {
    /// Absolute path of the source file.
    pub path: PathBuf,
    /// Engine sample rate of the decoded data.
    pub samplerate: u32,
    /// Planar decoded data, one plane per file channel.
    pub data: Vec<Vec<f32>>,
}

/// Reference from a sample to one file channel.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFileRef {
    /// Index into the instrument's channel list.
    pub instrument_channel: usize,
    /// Index into the kit file table.
    pub file_index: usize,
    /// 0-based index of the channel within the file (XML uses 1-based).
    pub filechannel: usize,
}

/// One sample of an instrument.
#[derive(Debug, Clone, Default)]
pub struct LoadedSample {
    pub name: String,
    /// Per-sample power for >= 2.0 kits, 0.0 for v1.0 kits.
    pub power: f32,
    /// Uses file power normalization if the sample says so.
    pub normalized: bool,
    pub audiofiles: Vec<AudioFileRef>,
}

/// Choke link rendered into instrument ids at load time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChokeRef {
    /// Target instrument id.
    pub instrument_id: usize,
    /// Choke ramp down time in milliseconds.
    pub choketime_ms: f64,
}

/// One instrument channel mapped onto a kit channel.
#[derive(Debug, Clone, PartialEq)]
pub struct InstrumentChannelRef {
    /// Kit channel number or `NO_CHANNEL` when unmapped/missing.
    pub kit_channel: usize,
    /// Display name (kit channel name, when resolvable).
    pub name: String,
    pub main: MainState,
}

/// Resolution of velocity groups for v1.0 kits.
#[derive(Debug, Clone, PartialEq)]
pub struct VelocityGroup {
    pub lower: f32,
    pub upper: f32,
    /// Sample ids (indices in [`LoadedInstrument::samples`]) covered.
    pub samples: Vec<usize>,
}

/// An instrument in a fully loaded kit.
#[derive(Debug, Clone, Default)]
pub struct LoadedInstrument {
    pub id: usize,
    pub name: String,
    pub group: String,
    pub description: String,
    /// Instrument version string, distinguishes v1.0 from >= 2.0.
    pub version: String,
    pub channels: Vec<InstrumentChannelRef>,
    pub samples: Vec<LoadedSample>,
    pub chokes: Vec<ChokeRef>,
    /// Velocity groups for v1.0 kits, empty for >= 2.0 kits.
    pub velocity_groups: Vec<VelocityGroup>,
}

/// Complete, read-only kit state handed onto the realtime engine.
#[derive(Debug, Default)]
pub struct LoadedKit {
    /// Directory the kit was loaded from.
    pub basepath: PathBuf,
    pub name: String,
    pub description: String,
    pub version: String,
    pub samplerate: u32,
    pub metadata: crate::dom::MetadataDom,
    /// Kit channel display names; the index is the channel number.
    pub channels: Vec<String>,
    pub instruments: Vec<LoadedInstrument>,
    /// Shared file data table; samples reference this by index.
    pub pub_files: Vec<Arc<AudioFileData>>,
}

impl LoadedKit {
    /// Read-only file table alias (samples hold indices into this).
    pub fn files(&self) -> &[Arc<AudioFileData>] {
        &self.pub_files
    }

    /// Returns the instrument id for a name, if present.
    pub fn instrument_id_by_name(&self, instrument: &str) -> Option<usize> {
        self.instruments.iter().position(|i| i.name == instrument)
    }

    /// Builds the note→instrument mapping induced by a midimap; collects all
    /// instruments mapped to each note, just like `MidiMapper::lookup` does.
    pub fn build_midi_map(&self, midimap: &[parser::MidimapEntry]) -> Vec<Vec<usize>> {
        let mut map = vec![Vec::new(); 128];
        for entry in midimap {
            if let Some(instrument_id) = self.instrument_id_by_name(&entry.instrument) {
                map[usize::from(entry.note)].push(instrument_id);
            }
        }
        map
    }
}

/// Reported during background loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStage {
    Idle,
    Parsing,
    Decoding,
    Finalising,
    Done,
}

impl std::fmt::Display for LoadStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LoadStage::Idle => "idle",
            LoadStage::Parsing => "parsing",
            LoadStage::Decoding => "loading samples",
            LoadStage::Finalising => "processing",
            LoadStage::Done => "done",
        })
    }
}

/// Live progress snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadProgress {
    pub stage: LoadStage,
    /// Number of finished sub tasks (files decoded so far).
    pub done: u64,
    /// Total number of sub tasks of the current stage.
    pub total: u64,
    /// Index of the instrument currently being decoded.
    pub instrument_index: u32,
}

impl LoadProgress {
    /// Progress as a fraction in `[0, 1]`.
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return f32::from(u8::from(self.stage == LoadStage::Done));
        }
        (self.done as f64 / self.total as f64) as f32
    }
}

/// Errors raised while loading a kit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError(pub String);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

impl From<crate::dom::ParseError> for LoadError {
    fn from(e: crate::dom::ParseError) -> Self {
        LoadError(e.0)
    }
}

impl From<decoder::DecodeError> for LoadError {
    fn from(e: decoder::DecodeError) -> Self {
        LoadError(e.0)
    }
}

#[derive(Debug, Default)]
pub struct ProgressShared {
    stage: AtomicU8,
    done: AtomicU64,
    total: AtomicU64,
    instrument: AtomicU32,
}

impl ProgressShared {
    fn set(&self, stage: LoadStage, done: u64, total: u64) {
        self.stage.store(stage as u8, Ordering::Relaxed);
        self.done.store(done, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    fn set_instrument(&self, index: usize) {
        self.instrument.store(index as u32, Ordering::Relaxed);
    }

    fn get(&self) -> LoadProgress {
        LoadProgress {
            stage: match self.stage.load(Ordering::Relaxed) {
                0 => LoadStage::Idle,
                1 => LoadStage::Parsing,
                2 => LoadStage::Decoding,
                3 => LoadStage::Finalising,
                _ => LoadStage::Done,
            },
            done: self.done.load(Ordering::Relaxed),
            total: self.total.load(Ordering::Relaxed),
            instrument_index: self.instrument.load(Ordering::Relaxed),
        }
    }
}

/// Background kit loader.
pub struct KitLoader {
    shared: Arc<ProgressShared>,
    rx: Option<mpsc::Receiver<Result<Arc<LoadedKit>, LoadError>>>,
}

impl KitLoader {
    /// Starts loading the kit at `path` (a drumkit XML file or a directory
    /// containing it) on a worker thread.
    pub fn start(path: impl AsRef<Path>, engine_samplerate: u32) -> Self {
        let shared = Arc::new(ProgressShared::default());
        let shared_thread = Arc::clone(&shared);
        let (tx, rx) = mpsc::channel::<Result<Arc<LoadedKit>, LoadError>>();
        let path = path.as_ref().to_path_buf();

        // If spawning fails the receiver stays connected-then-closed, which
        // `poll` reports as a load error.
        let _ = std::thread::Builder::new()
            .name("kitloader".into())
            .spawn(move || {
                let result = load_kit_blocking(&path, engine_samplerate, &shared_thread);
                shared_thread.set(LoadStage::Done, 1, 1);
                let _ = tx.send(result);
            });

        Self {
            shared,
            rx: Some(rx),
        }
    }

    /// Latest progress snapshot.
    pub fn progress(&self) -> LoadProgress {
        self.shared.get()
    }

    /// Non-blocking poll of the finished kit.
    pub fn poll(&mut self) -> Option<Result<Arc<LoadedKit>, LoadError>> {
        let recv = self.rx.as_ref()?;
        match recv.try_recv() {
            Ok(result) => {
                self.rx = None;
                Some(result)
            }
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                Some(Err(LoadError("Kit loader thread disappeared".into())))
            }
        }
    }
}

/// Lexically normalize `..` and `.` without touching the filesystem; mirrors
/// the `getPath()` behaviour used by domloader.
pub fn lex_normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// Directory that instrument/audio files resolve against: the directory of
/// the instrument XML file (which itself is relative to the kit base path).
fn audio_base(basepath: &Path, instrument_file: &str) -> PathBuf {
    let xml = basepath.join(instrument_file);
    match xml.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => basepath.to_path_buf(),
    }
}

/// The synchronous loader: used directly by tests and on the background
/// thread started by [`KitLoader::start`].
pub fn load_kit_blocking(
    path: &Path,
    engine_samplerate: u32,
    progress: &ProgressShared,
) -> Result<Arc<LoadedKit>, LoadError> {
    let (basepath, xml_path) = resolve_kit_xml(path)?;
    progress.set(LoadStage::Parsing, 0, 1);

    let kit_dom: DrumkitDom = parser::parse_drumkit_file(&xml_path)?;

    // Parse all instrument XMLs referenced by the kit, keeping a slot per
    // reference (None when the instrument is missing or mismatched; matches
    // the warn-and-skip of domloader).
    let mut instrument_doms: Vec<Option<InstrumentDom>> =
        Vec::with_capacity(kit_dom.instruments.len());
    for (index, reference) in kit_dom.instruments.iter().enumerate() {
        let instrument_file = basepath.join(&reference.file);
        match parser::parse_instrument_file(&instrument_file) {
            Ok(dom) => {
                if dom.name != reference.name {
                    instrument_doms.push(None);
                    continue;
                }
                instrument_doms.push(Some(dom));
            }
            Err(e) => {
                return Err(LoadError(format!("Instrument {}: {e}", reference.name)));
            }
        }
        progress.set(
            LoadStage::Parsing,
            (index + 1) as u64,
            kit_dom.instruments.len() as u64,
        );
    }

    // Collect the distinct audio files in kit order for deterministic loading.
    let mut file_paths: BTreeSet<PathBuf> = BTreeSet::new();
    for reference in &kit_dom.instruments {
        let Some(dom) = instrument_doms.iter().find_map(|d| {
            d.as_ref()
                .filter(|inst_dom| inst_dom.name == reference.name)
                .cloned()
        }) else {
            continue;
        };
        let base = audio_base(&basepath, &reference.file);
        for sample_dom in &dom.samples {
            for af in &sample_dom.audiofiles {
                let full = base.join(&af.file);
                if full.exists() {
                    file_paths.insert(lex_normalise(&full));
                }
            }
        }
    }
    let total_files = file_paths.len() as u64;
    progress.set(LoadStage::Decoding, 0, total_files);

    let mut pub_files: Vec<Arc<AudioFileData>> = Vec::with_capacity(total_files as usize);
    let mut file_index_of: HashMap<PathBuf, usize> = HashMap::new();

    for (progress_index, path) in file_paths.iter().enumerate() {
        let decoded = decoder::decode_file(path, Some(f64::from(engine_samplerate)))?;
        let file_index = pub_files.len();
        file_index_of.insert(path.clone(), file_index);
        pub_files.push(Arc::new(AudioFileData {
            path: path.clone(),
            samplerate: decoded.samplerate,
            data: decoded.data,
        }));
        progress.set_instrument(progress_index);
        progress.set(
            LoadStage::Decoding,
            (progress_index + 1) as u64,
            total_files,
        );
    }

    progress.set(LoadStage::Finalising, 0, 1);

    let channels: Vec<String> = kit_dom.channels.iter().map(|c| c.name.clone()).collect();

    // Pass 1 of domloader.cc.
    let mut instruments: Vec<LoadedInstrument> = Vec::new();
    let mut reference_anchor: Vec<Option<usize>> = Vec::with_capacity(kit_dom.instruments.len());
    for (reference_index, reference) in kit_dom.instruments.iter().enumerate() {
        let Some(inst_dom) = instrument_doms[reference_index].clone() else {
            reference_anchor.push(None);
            continue;
        };
        reference_anchor.push(Some(instruments.len()));

        let channelmap: HashMap<&str, (&str, MainState)> = reference
            .channel_map
            .iter()
            .map(|cm| (cm.r#in.as_str(), (cm.out.as_str(), cm.main)))
            .collect();

        // Instrument channel list: declared channels first, then audiofile
        // channels (in sample order), which matches `addOrGetChannel` order.
        let mut instrument_channels: Vec<String> = inst_dom
            .instrument_channels
            .iter()
            .map(|c| c.name.clone())
            .collect();
        for sample_dom in &inst_dom.samples {
            for af in &sample_dom.audiofiles {
                if !instrument_channels.contains(&af.instrument_channel) {
                    instrument_channels.push(af.instrument_channel.clone());
                }
            }
        }

        let has_explicit_main = channelmap
            .values()
            .any(|(_, main)| *main != MainState::Unset);
        let default_main_state = if has_explicit_main {
            MainState::IsNotMain
        } else {
            MainState::Unset
        };

        let channel_refs: Vec<InstrumentChannelRef> = instrument_channels
            .iter()
            .map(|name| {
                let (out_name, mut main_state) = channelmap
                    .get(name.as_str())
                    .map(|(out, m)| (*out, *m))
                    .unwrap_or((name.as_str(), MainState::Unset));
                if main_state == MainState::Unset {
                    main_state = default_main_state;
                }
                let kit_channel = channels
                    .iter()
                    .position(|c| c == out_name)
                    .unwrap_or(NO_CHANNEL);
                InstrumentChannelRef {
                    kit_channel,
                    name: name.clone(),
                    main: main_state,
                }
            })
            .collect();

        let shape = inst_dom.samples.len();
        let mut samples: Vec<LoadedSample> = Vec::with_capacity(shape);
        for sample_dom in &inst_dom.samples {
            let mut sample = LoadedSample {
                name: sample_dom.name.clone(),
                power: sample_dom.power as f32,
                normalized: sample_dom.normalized,
                audiofiles: Vec::new(),
            };
            for af in &sample_dom.audiofiles {
                let full = audio_base(&basepath, &reference.file).join(&af.file);
                let normalized_path = lex_normalise(&full);
                let Some(file_index) = file_index_of.get(&normalized_path).copied() else {
                    continue; // missing file: upstream skips (debug warns)
                };
                let Some(ic_index) = channel_refs
                    .iter()
                    .position(|c| c.name == af.instrument_channel)
                else {
                    continue;
                };
                sample.audiofiles.push(AudioFileRef {
                    instrument_channel: ic_index,
                    file_index,
                    filechannel: af.filechannel.saturating_sub(1),
                });
            }
            samples.push(sample);
        }

        // Velocity groups (v1.0 only).
        let mut velocity_groups = Vec::new();
        if crate::dom::uses_velocities(&inst_dom.version) {
            for velocity in &inst_dom.velocities {
                let mut sample_ids = Vec::new();
                for sample_ref in &velocity.samplerefs {
                    let Some(id) = samples.iter().position(|s| s.name == sample_ref.name) else {
                        return Err(LoadError(format!(
                            "Missing sample '{}' in the velocity group of '{}'",
                            sample_ref.name, inst_dom.name
                        )));
                    };
                    sample_ids.push(id);
                }
                velocity_groups.push(VelocityGroup {
                    lower: velocity.lower as f32,
                    upper: velocity.upper as f32,
                    samples: sample_ids,
                });
            }
        }

        instruments.push(LoadedInstrument {
            id: 0,
            name: inst_dom.name.clone(),
            group: reference.group.clone(),
            description: inst_dom.description.clone(),
            version: inst_dom.version.clone(),
            channels: channel_refs,
            samples,
            chokes: Vec::new(),
            velocity_groups,
        });
    }

    // Assign stable ids (index) and resolve chokes via names (pass 2).
    for (id, instrument) in instruments.iter_mut().enumerate() {
        instrument.id = id;
    }

    let id_of: HashMap<String, usize> =
        instruments.iter().map(|i| (i.name.clone(), i.id)).collect();
    for (reference_index, reference) in kit_dom.instruments.iter().enumerate() {
        let Some(instrument_id) = reference_anchor[reference_index] else {
            continue;
        };
        let instrument = &mut instruments[instrument_id];
        for choke in &reference.chokes {
            if let Some(&choke_id) = id_of.get(&choke.instrument) {
                instrument.chokes.push(ChokeRef {
                    instrument_id: choke_id,
                    choketime_ms: choke.choketime,
                });
            }
            // unresolved targets are skipped like domloader does
        }
    }

    let loaded = LoadedKit {
        basepath,
        name: kit_dom.metadata.title.clone(),
        description: kit_dom.metadata.description.clone(),
        version: kit_dom.version.clone(),
        samplerate: kit_dom.samplerate.round() as u32,
        metadata: kit_dom.metadata,
        channels,
        instruments,
        pub_files,
    };
    Ok(Arc::new(loaded))
}

/// Resolves a kit path into (basepath, drumkit-xml file).
pub fn resolve_kit_xml(path: &Path) -> Result<(PathBuf, PathBuf), LoadError> {
    if path.is_file() {
        Ok((
            lex_normalise(path.parent().unwrap_or(Path::new("."))),
            lex_normalise(path),
        ))
    } else if path.is_dir() {
        for name in ["drumkit.xml", "DrumKit.xml"] {
            let candidate = path.join(name);
            if candidate.is_file() {
                return Ok((lex_normalise(path), candidate));
            }
        }
        Err(LoadError(format!(
            "No drumkit.xml found in {}",
            path.display()
        )))
    } else {
        Err(LoadError(format!(
            "Specified drumkit path does not exist: {}",
            path.display()
        )))
    }
}
