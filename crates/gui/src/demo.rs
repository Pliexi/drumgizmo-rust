// SPDX-License-Identifier: GPL-3.0-or-later
//! Demo beat seeding for freshly loaded kits: an empty pattern plays a
//! classic one-bar rock groove so the transport produces sound the
//! moment a kit loads (instant audible feedback that the chain works).
//! Heuristics match the lane's instrument name; kits with other naming
//! fall back to the first three lanes.

use dg_sequencer::Pattern;

/// Step sets of the three groove voices inside an ordinary 16-step bar.
const KICK: [usize; 2] = [0, 10];
const SNARE: [usize; 2] = [4, 12];
const HAT: [usize; 4] = [2, 6, 10, 14];

/// Finds the lane index whose instrument name contains any needle
/// (needles are tried in order, so "closed" matches before "hat").
fn pick_lane(names: &[String], needles: &[&str]) -> Option<usize> {
    for needle in needles {
        let needle = needle.to_ascii_lowercase();
        if let Some(index) = names
            .iter()
            .position(|name| name.to_ascii_lowercase().contains(&needle))
        {
            return Some(index);
        }
    }
    None
}

/// Free lane index skipping already-assigned ones.
fn pick_free(names: &[String], taken: &[usize]) -> Option<usize> {
    (0..names.len()).find(|lane| !taken.contains(lane))
}

/// Rebuilds the enabled steps of `pattern.lanes` with the demo groove
/// (lane order stays, one filled bar per `bars`). `names` is parallel to
/// the current lane list.
pub fn seed_pattern(pattern: &mut Pattern, names: &[String]) {
    if names.is_empty() {
        return;
    }
    let bars = (pattern.length.max(16) / 16).max(1);

    let mut taken: Vec<usize> = Vec::new();
    let kick_lane = pick_lane(names, &["kick"]).or_else(|| pick_free(names, &taken));
    if let Some(lane) = kick_lane {
        taken.push(lane);
    }
    let snare_lane = pick_lane(names, &["snare"]).or_else(|| pick_free(names, &taken));
    if let Some(lane) = snare_lane {
        taken.push(lane);
    }
    let hat_lane = pick_lane(names, &["closed"])
        .or_else(|| pick_lane(names, &["hat"]))
        .or_else(|| pick_free(names, &taken));
    if let Some(lane) = hat_lane {
        taken.push(lane);
    }

    // Clear the pattern, then paint every voice across all bars.
    for lane in &mut pattern.lanes {
        for step in &mut lane.steps {
            step.enabled = false;
        }
    }
    let paint = |pattern: &mut Pattern, lane_index: usize, steps: &[usize]| {
        if let Some(lane) = pattern.lanes.get_mut(lane_index) {
            for bar in 0..bars {
                for step in steps {
                    let step = bar * 16 + step;
                    if step < pattern.length {
                        lane.ensure_length(step + 1);
                        lane.steps[step].enabled = true;
                        lane.steps[step].velocity = 0.85;
                    }
                }
            }
        }
    };
    if let Some(lane) = kick_lane {
        paint(pattern, lane, &KICK);
    }
    if let Some(lane) = snare_lane {
        paint(pattern, lane, &SNARE);
    }
    if let Some(lane) = hat_lane {
        paint(pattern, lane, &HAT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_sequencer::Lane;

    fn lanes_for(pattern: &mut Pattern, count: usize) {
        pattern.lanes.clear();
        for id in 0..count {
            pattern.lanes.push(Lane::new(id, 36 + id as u8, 16));
        }
    }

    fn enabled_steps(pattern: &Pattern, lane: usize) -> Vec<usize> {
        pattern.lanes[lane]
            .steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step.enabled)
            .map(|(index, _)| index)
            .collect()
    }

    #[test]
    fn classic_beat_on_named_kits() {
        let names = vec![
            "Kick".to_string(),
            "Snare".to_string(),
            "HiHat Open".to_string(),
            "HiHat Closed".to_string(),
        ];
        let mut pattern = Pattern::default();
        lanes_for(&mut pattern, names.len());
        seed_pattern(&mut pattern, &names);
        assert_eq!(enabled_steps(&pattern, 0), vec![0, 10]);
        assert_eq!(enabled_steps(&pattern, 1), vec![4, 12]);
        // Closed hat preferred over open (needle order).
        assert_eq!(enabled_steps(&pattern, 3), vec![2, 6, 10, 14]);
        assert!(enabled_steps(&pattern, 2).is_empty());
    }

    #[test]
    fn fallback_beat_without_matching_names() {
        let names = vec!["A".to_string(), "B".to_string(), "C".to_string()];
        let mut pattern = Pattern::default();
        lanes_for(&mut pattern, names.len());
        seed_pattern(&mut pattern, &names);
        assert_eq!(enabled_steps(&pattern, 0), vec![0, 10]);
        assert_eq!(enabled_steps(&pattern, 1), vec![4, 12]);
        assert_eq!(enabled_steps(&pattern, 2), vec![2, 6, 10, 14]);
    }

    #[test]
    fn two_bar_pattern_extends_the_groove() {
        let names = vec!["Kick".to_string(), "Snare".to_string()];
        let mut pattern = Pattern {
            length: 32,
            ..Default::default()
        };
        lanes_for(&mut pattern, names.len());
        seed_pattern(&mut pattern, &names);
        assert_eq!(enabled_steps(&pattern, 0), vec![0, 10, 16, 26]);
        assert_eq!(enabled_steps(&pattern, 1), vec![4, 12, 20, 28]);
    }
}
