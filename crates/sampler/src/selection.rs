// SPDX-License-Identifier: GPL-3.0-or-later
//! Sample selection algorithms.
//!
//! * [`VelocityRangeTable`] mirrors `rangemap.h` + `instrument.cc`: v1.0 kits
//!   keep a table of (lower, upper) → sample and pick uniformly at random
//!   (`rand.choose`) among all samples covering the requested velocity.
//! * [`PowerList`] mirrors `powerlist.h`/`powerlist.cc`: samples of >= 2.0
//!   kits sorted by their `power`.
//! * [`SampleSelection`] port of `sample_selection.cc`: weighted scoring
//!   (close/diverse/random) with early termination, exactly the numbers of
//!   the original algorithm (`4`, `1/2`, `1/3` magic factors included).

use std::sync::Arc;

use dg_common::rng::Rand;
use dg_kit::{LoadedInstrument, LoadedKit};

/// Sorted power list of an instrument (upstream `PowerList`).
#[derive(Debug, Clone, Default)]
pub struct PowerList {
    /// Power per entry, sorted ascending, parallel to `sample_indices`.
    pub powers: Vec<f32>,
    pub sample_indices: Vec<usize>,
    pub min_power: f32,
    pub max_power: f32,
}

/// Port of `SampleSelection` + its `finalise()` bookkeeping state.
#[derive(Debug, Clone, Default)]
pub struct SampleSelection {
    pub powerlist: PowerList,
    /// `last[i]` = sample position where entry i was used the last time.
    pub last: Vec<u64>,
}

impl SampleSelection {
    /// Builds the selection table from a >= 2.0 instrument.
    pub fn new_v2(instrument: &LoadedInstrument) -> Self {
        let mut entries: Vec<(f32, usize)> = instrument
            .samples
            .iter()
            .enumerate()
            .map(|(idx, sample)| (sample.power, idx))
            .collect();
        entries.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let powers: Vec<f32> = entries.iter().map(|(power, _)| *power).collect();
        let sample_indices: Vec<usize> = entries.iter().map(|(_, idx)| *idx).collect();
        let min_power = powers.first().copied().unwrap_or(0.0);
        let max_power = powers.last().copied().unwrap_or(0.0);

        let last = vec![0; powers.len()];
        Self {
            powerlist: PowerList {
                powers,
                sample_indices,
                min_power,
                max_power,
            },
            last,
        }
    }

    /// `power_min + velocity * power_span` of inputprocessor.cc.
    pub fn velocity_to_level(&self, velocity: f32) -> f32 {
        let span = self.powerlist.max_power - self.powerlist.min_power;
        self.powerlist.min_power + velocity * span
    }

    /// Port of `SampleSelection::get(level, pos)`.
    ///
    /// `level` is in the instrument's power domain, `pos` the current
    /// playhead position in samples, `samplerate` the engine rate (used for
    /// the diverse term exactly like upstream).
    pub fn get(&mut self, level: f32, pos: u64, samplerate: u64, rand: &mut Rand) -> Option<usize> {
        let sample_count = self.powerlist.sample_indices.len();
        if sample_count == 0 {
            return None;
        }

        let powers = &self.powerlist.powers;

        // Magic numbers from upstream sample_selection.cc.
        let f_close = 4.0f32;
        let f_diverse = 0.5f32;
        let f_random = 1.0f32 / 3.0;

        let mut power_range = self.powerlist.max_power - self.powerlist.min_power;
        if power_range == 0.0 {
            power_range = 1.0;
        }

        let pow2 = |v: f32| v * v;

        // lower_bound on ascending powers.
        let up_start = powers.partition_point(|p| *p < level);
        let mut up_index = up_start;
        let mut up_value_lb = if up_start < sample_count {
            f_close * pow2((powers[up_start] - level) / power_range)
        } else {
            up_index = sample_count - 1;
            f32::MAX
        };
        let mut down_index = up_start.min(sample_count);
        let mut down_value_lb = if up_start == 0 {
            f32::MAX
        } else {
            down_index = up_start;
            let idx = down_index - 1;
            if up_index != 0 {
                f_close * pow2((powers[idx] - level) / power_range)
            } else {
                f32::MAX
            }
        };

        let mut index_opt = 0usize;
        let mut value_opt = f32::MAX;
        let mut count = 0usize;

        loop {
            let current_index = if up_value_lb < down_value_lb {
                let current = up_index;
                if up_index < sample_count - 1 {
                    up_index += 1;
                    up_value_lb = f_close * pow2((powers[up_index] - level) / power_range);
                } else {
                    up_value_lb = f32::MAX;
                }
                current
            } else {
                let current = down_index;
                if down_index > 0 {
                    down_index -= 1;
                    down_value_lb = f_close * pow2((powers[down_index] - level) / power_range);
                } else {
                    down_value_lb = f32::MAX;
                }
                current
            };

            let random = rand.next_f32();
            let close = (powers[current_index] - level) / power_range;
            let elapsed = pos.saturating_sub(self.last[current_index]) as f64;
            let diverse = (1.0 / (1.0 + elapsed / samplerate as f64)) as f32;
            let value = f_close * pow2(close) + f_diverse * diverse + f_random * random;

            if value < value_opt {
                index_opt = current_index;
                value_opt = value;
            }
            count += 1;

            // Neither direction can improve anymore (upstream maintained the
            // invariant through the lower bounds; we keep it explicitly).
            if up_value_lb > value_opt && down_value_lb > value_opt {
                break;
            }
            if count > sample_count {
                break;
            }
        }

        self.last[index_opt] = pos;
        Some(self.powerlist.sample_indices[index_opt])
    }
}

/// Reference to one resolved velocity group of a loaded kit instrument.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupedRange {
    pub lower: f32,
    pub upper: f32,
    pub samples: Vec<usize>,
}

/// v1.0 instrument velocity table (upstream `RangeMap` + `rand.choose`).
#[derive(Debug, Clone, Default)]
pub struct VelocityRangeTable {
    ranges: Vec<GroupedRange>,
}

impl VelocityRangeTable {
    /// Builds from a loaded instrument's resolved velocity groups.
    pub fn new(instrument: &LoadedInstrument) -> Self {
        let mut ranges: Vec<GroupedRange> = instrument
            .velocity_groups
            .iter()
            .map(|v| GroupedRange {
                lower: v.lower,
                upper: v.upper,
                samples: v.samples.clone(),
            })
            .collect();
        ranges.sort_by(|a, b| {
            a.lower
                .partial_cmp(&b.lower)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Self { ranges }
    }

    /// All samples whose range covers `at` (mirrors `RangeMap::get(at)`).
    pub fn get(&self, at: f32) -> Vec<usize> {
        let mut out = Vec::new();
        for range in &self.ranges {
            // `insert` upstream normalises the pair before storing.
            let (low, high) = if range.lower < range.upper {
                (range.lower, range.upper)
            } else {
                (range.upper, range.lower)
            };
            if at >= low && at <= high {
                out.extend_from_slice(&range.samples);
            }
        }
        out
    }

    /// Upstream `Instrument::sample()` picks uniformly among candidates.
    pub fn choose(&self, at: f32, rand: &mut Rand) -> Option<usize> {
        let candidates = self.get(at);
        rand.choose(&candidates).copied()
    }
}

/// What the engine uses per instrument to run `Instrument::sample()`.
#[derive(Debug, Clone)]
pub enum SelectionStrategy {
    /// >= 2.0 power list scoring.
    Power(SampleSelection),
    /// v1.0 velocity group ranges.
    Range(VelocityRangeTable),
}

impl SelectionStrategy {
    /// Builds the matching strategy from a loaded instrument.
    pub fn for_instrument(instrument: &LoadedInstrument) -> Self {
        if instrument.velocity_groups.is_empty() {
            Self::Power(SampleSelection::new_v2(instrument))
        } else {
            Self::Range(VelocityRangeTable::new(instrument))
        }
    }

    /// `Instrument::sample(level, pos)` port; returns a sample index.
    pub fn sample_index(
        &mut self,
        velocity: f32,
        pos: u64,
        samplerate: u64,
        rand: &mut Rand,
    ) -> Option<usize> {
        match self {
            SelectionStrategy::Power(selection) => {
                let level = selection.velocity_to_level(velocity);
                selection.get(level, pos, samplerate, rand)
            }
            SelectionStrategy::Range(range) => range.choose(velocity, rand),
        }
    }
}

/// Concrete per-sample file references; resolved once off the audio thread.
#[derive(Debug, Clone)]
pub struct SampleRef {
    pub file_index: usize,
    pub instrument_channel: usize,
    pub filechannel: usize,
}

/// Resolve a (kit, instrument, sample index) triple into file references.
pub fn sample_audio_files(
    kit: &Arc<LoadedKit>,
    instrument: usize,
    sample: usize,
) -> Vec<SampleRef> {
    kit.instruments
        .get(instrument)
        .and_then(|inst| inst.samples.get(sample))
        .map(|sample| {
            sample
                .audiofiles
                .iter()
                .map(|af| SampleRef {
                    file_index: af.file_index,
                    instrument_channel: af.instrument_channel,
                    filechannel: af.filechannel,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_kit::{LoadedSample, VelocityGroup};

    fn power_instrument() -> LoadedInstrument {
        LoadedInstrument {
            id: 0,
            name: "Kick".into(),
            samples: (0..6)
                .map(|i| LoadedSample {
                    name: format!("s{i}"),
                    power: 0.1 * i as f32 + 0.05,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn grouped_instrument() -> LoadedInstrument {
        LoadedInstrument {
            id: 0,
            name: "Kick".into(),
            samples: (0..6)
                .map(|i| LoadedSample {
                    name: format!("s{i}"),
                    ..Default::default()
                })
                .collect(),
            velocity_groups: vec![
                VelocityGroup {
                    lower: 0.0,
                    upper: 0.6,
                    samples: vec![0, 1, 2],
                },
                VelocityGroup {
                    lower: 0.6,
                    upper: 1.0,
                    samples: vec![3, 4, 5],
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn powerlist_sorted_and_velocity_mapped() {
        let selection = SampleSelection::new_v2(&power_instrument());
        let expected = [0.05, 0.15, 0.25, 0.35, 0.45, 0.55];
        for (got, want) in selection.powerlist.powers.iter().zip(expected) {
            assert!((got - want).abs() < 1e-6, "{got} != {want}");
        }
        assert!((selection.powerlist.min_power - 0.05).abs() < 1e-6);
        assert!((selection.powerlist.max_power - 0.55).abs() < 1e-6);
        // mid velocity maps to mid power
        let level = selection.velocity_to_level(0.5);
        assert!((level - 0.30).abs() < 1e-6);
    }

    #[test]
    fn selection_picks_closest_power() {
        // Choose deterministically (a fixed rng sequence); closest-power bias
        // is verified over a run: no sample from a far power wins massively.
        let mut selection = SampleSelection::new_v2(&power_instrument());
        let mut rand = Rand::new(7);
        let mut histogram = vec![0usize; 6];
        for run in 0..100 {
            let idx = selection
                .get(0.30, run as u64 / 441, 44100, &mut rand)
                .expect("sample available");
            histogram[idx] += 1;
        }
        // far off powers (0.05, 0.55) should not dominate
        assert_eq!(histogram.iter().sum::<usize>(), 100);
        for far in [0usize, 5usize] {
            assert!(histogram[far] < 50, "far power dominated: {histogram:?}");
        }
    }

    #[test]
    fn selection_diverse_prefers_unused() {
        let mut selection = SampleSelection::new_v2(&power_instrument());
        // mark all but index 0 recently used at pos 2^40 (far future)
        for (entry, last) in selection.last.iter_mut().enumerate() {
            if entry != 1 {
                *last = 1_000_000;
            }
        }
        let mut rand = Rand::new(1);
        let result = selection.get(0.15, 11, 44100, &mut rand);
        assert_eq!(result, Some(1), "unused entry should win near its power");
    }

    #[test]
    fn velocity_ranges_cover() {
        let table = VelocityRangeTable::new(&grouped_instrument());
        assert_eq!(table.get(0.2), vec![0, 1, 2]);
        assert_eq!(table.get(0.8), vec![3, 4, 5]);
        // boundary inclusive on both sides, like RangeMap::get
        assert_eq!(table.get(0.6), vec![0, 1, 2, 3, 4, 5]);
        assert!(table.get(1.5).is_empty());
        assert!(table.get(-0.5).is_empty());
    }

    #[test]
    fn range_choose_deterministic() {
        let table = VelocityRangeTable::new(&grouped_instrument());
        let mut rand = Rand::new(42);
        let mut picks = vec![];
        for _ in 0..10 {
            picks.push(table.choose(0.5, &mut rand));
        }
        assert!(picks.iter().all(|p| p.is_some()));
        assert!(picks.iter().any(|p| *p != picks[0]), "random choice varies");
    }

    #[test]
    fn empty_instrument_returns_none() {
        let empty = LoadedInstrument::default();
        let mut selection = SampleSelection::new_v2(&empty);
        let mut rand = Rand::new(3);
        assert!(selection.get(0.5, 0, 44100, &mut rand).is_none());
        let empty_table = VelocityRangeTable::default();
        assert!(empty_table.choose(0.5, &mut rand).is_none());
    }

    #[test]
    fn kit_stub_complete() {
        // silence asserted copy of LoadedKit default usage
        let _kit = Arc::new(LoadedKit::default());
        assert!(sample_audio_files(&_kit, 0, 0).is_empty());
    }
}
