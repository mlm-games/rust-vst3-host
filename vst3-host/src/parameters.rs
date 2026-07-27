//! Parameter types and utilities for VST3 host

use crate::Result;
use serde::{Deserialize, Serialize};

/// Plugin parameter information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameter {
    /// Parameter ID
    pub id: u32,
    /// Parameter name
    pub name: String,
    /// Current normalized value (0.0 to 1.0)
    pub value: f64,
    /// Minimum value in normalized space (VST3 parameters are always 0.0..=1.0;
    /// the plain/engineering range is private to the plugin — use
    /// [`crate::Plugin::format_parameter`] for human-readable values).
    pub min: f64,
    /// Maximum value in normalized space (always 1.0 for VST3 parameters).
    pub max: f64,
    /// Default value
    pub default: f64,
    /// Parameter unit (e.g., "Hz", "dB", "%")
    pub unit: String,
    /// Step count, straight from VST3 `ParameterInfo::stepCount`, which counts the *gaps* between
    /// discrete values rather than the values themselves: `0` is continuous, `1` is a two-state
    /// toggle, and `n` is a list of `n + 1` values. So a three-way selector reports `2`, not `3`.
    pub step_count: i32,
    /// Whether the parameter can be automated
    pub can_automate: bool,
    /// Whether the parameter is read-only
    pub is_read_only: bool,
    /// Whether the parameter is a bypass control
    pub is_bypass: bool,
    /// Parameter flags
    pub flags: u32,
}

impl Parameter {
    /// Convert normalized value (0.0-1.0) to plain value
    pub fn normalized_to_plain(&self, normalized: f64) -> f64 {
        if self.step_count >= 1 {
            // Stepped parameter: snap to the nearest step. Includes toggles (`step_count == 1`),
            // which snap to exactly min or max.
            let steps = self.step_count as f64;
            let step = (normalized * steps).round();
            self.min + (step / steps) * (self.max - self.min)
        } else {
            // Continuous parameter
            self.min + normalized * (self.max - self.min)
        }
    }

    /// Which discrete value a normalized position selects, for a stepped parameter: `0..=step_count`
    /// (so `step_count + 1` possible values). `None` for a continuous parameter.
    pub fn step_index(&self, normalized: f64) -> Option<i32> {
        if self.step_count >= 1 {
            Some((normalized.clamp(0.0, 1.0) * self.step_count as f64).round() as i32)
        } else {
            None
        }
    }

    /// Convert plain value to normalized value (0.0-1.0)
    pub fn plain_to_normalized(&self, plain: f64) -> f64 {
        if (self.max - self.min).abs() < f64::EPSILON {
            0.0
        } else {
            ((plain - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
        }
    }

    /// Approximate a human-readable value string from normalized space.
    ///
    /// This cannot know the plugin's internal mapping (VST3 keeps that private), so
    /// for continuous parameters it just reports the normalized number with the unit.
    /// For accurate display (e.g. `"440.00 Hz"`), use
    /// [`crate::Plugin::format_parameter`], which asks the plugin to format it.
    pub fn format_value(&self, normalized: f64) -> String {
        match self.step_index(normalized) {
            // Toggle: one step, so two states.
            Some(index) if self.step_count == 1 => {
                if index >= 1 { "On" } else { "Off" }.to_string()
            }
            // Stepped: report which value is selected. The plain value lives in normalized space
            // (VST3 keeps the engineering range private), so the index is the only meaningful
            // number to show — use `Plugin::format_parameter` for the plugin's own label.
            Some(index) => {
                if self.unit.is_empty() {
                    format!("{index}")
                } else {
                    format!("{} {}", index, self.unit)
                }
            }
            None => {
                let plain = self.normalized_to_plain(normalized);
                if self.unit.is_empty() {
                    format!("{plain:.3}")
                } else {
                    format!("{:.3} {}", plain, self.unit)
                }
            }
        }
    }

    /// Whether this parameter takes discrete steps rather than a continuous range.
    ///
    /// True for toggles too — a toggle is just the one-step case. See [`Self::step_count`] for the
    /// VST3 counting convention.
    pub fn is_discrete(&self) -> bool {
        self.step_count >= 1
    }

    /// Whether this parameter is a two-state toggle (VST3 `stepCount == 1`).
    pub fn is_boolean(&self) -> bool {
        self.step_count == 1
    }
}

/// Parameter change event
#[derive(Debug, Clone)]
pub struct ParameterChange {
    /// Parameter ID
    pub id: u32,
    /// New normalized value (0.0 to 1.0)
    pub value: f64,
    /// Sample offset within the current block
    pub sample_offset: i32,
}

/// Batch parameter update
pub struct ParameterUpdate<'a> {
    updates: Vec<(u32, f64)>,
    plugin: &'a mut crate::Plugin,
}

impl<'a> ParameterUpdate<'a> {
    pub(crate) fn new(plugin: &'a mut crate::Plugin) -> Self {
        Self {
            updates: Vec::new(),
            plugin,
        }
    }

    /// Set a parameter value
    pub fn set(&mut self, id: u32, value: f64) -> &mut Self {
        self.updates.push((id, value));
        self
    }

    /// Apply all parameter updates
    pub fn apply(self) -> Result<()> {
        for (id, value) in self.updates {
            self.plugin.set_parameter(id, value)?;
        }
        Ok(())
    }
}

/// Parameter automation curve types
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AutomationCurve {
    /// Linear interpolation
    Linear,
    /// Exponential curve
    Exponential,
    /// Logarithmic curve
    Logarithmic,
    /// Step (no interpolation)
    Step,
}

/// Parameter automation point
#[derive(Debug, Clone)]
pub struct AutomationPoint {
    /// Time in seconds
    pub time: f64,
    /// Normalized value (0.0 to 1.0)
    pub value: f64,
    /// Curve type to next point
    pub curve: AutomationCurve,
}

/// Parameter automation data
#[derive(Debug, Clone)]
pub struct ParameterAutomation {
    /// Automation points
    pub points: Vec<AutomationPoint>,
    /// Whether to loop the automation
    pub looping: bool,
}

impl ParameterAutomation {
    /// Create new automation
    pub fn new() -> Self {
        Self {
            points: Vec::new(),
            looping: false,
        }
    }

    /// Add an automation point
    pub fn add_point(mut self, time: f64, value: f64) -> Self {
        self.points.push(AutomationPoint {
            time,
            value,
            curve: AutomationCurve::Linear,
        });
        // `total_cmp` orders NaN deterministically instead of panicking like
        // `partial_cmp(..).unwrap()` would on a NaN time from this public API.
        self.points.sort_by(|a, b| a.time.total_cmp(&b.time));
        self
    }

    /// Set the curve type
    pub fn with_curve(mut self, curve: AutomationCurve) -> Self {
        for point in &mut self.points {
            point.curve = curve;
        }
        self
    }

    /// Enable looping
    pub fn with_loop(mut self, looping: bool) -> Self {
        self.looping = looping;
        self
    }

    /// Get value at specific time
    pub fn value_at_time(&self, time: f64) -> Option<f64> {
        if self.points.is_empty() {
            return None;
        }

        // Handle looping
        let time = if self.looping && !self.points.is_empty() {
            let duration = self.points.last().unwrap().time;
            if duration > 0.0 {
                time % duration
            } else {
                time
            }
        } else {
            time
        };

        // Find surrounding points
        let mut prev = None;
        let mut next = None;

        for (i, point) in self.points.iter().enumerate() {
            if point.time <= time {
                prev = Some(i);
            } else {
                next = Some(i);
                break;
            }
        }

        match (prev, next) {
            (None, _) => Some(self.points[0].value),
            (Some(i), None) => Some(self.points[i].value),
            (Some(i), Some(j)) => {
                let p1 = &self.points[i];
                let p2 = &self.points[j];

                let t = (time - p1.time) / (p2.time - p1.time);

                let value = match p1.curve {
                    AutomationCurve::Linear => p1.value + (p2.value - p1.value) * t,
                    AutomationCurve::Exponential => p1.value + (p2.value - p1.value) * t * t,
                    AutomationCurve::Logarithmic => p1.value + (p2.value - p1.value) * t.sqrt(),
                    AutomationCurve::Step => p1.value,
                };

                Some(value.clamp(0.0, 1.0))
            }
        }
    }

    /// Sample this automation across one audio block, returning `(sample_offset, value)`
    /// points suitable for sample-accurate scheduling (e.g. [`Plugin::set_parameter_at`]).
    ///
    /// `block_start_secs` is the block's start on the automation timeline; `frames` is the
    /// block length; `points_per_block` is the sub-block resolution (1 = one value at the
    /// block start; higher = finer ramps, capped at `frames`). Returns empty if the
    /// automation has no points.
    ///
    /// [`Plugin::set_parameter_at`]: crate::Plugin::set_parameter_at
    pub fn points_for_block(
        &self,
        block_start_secs: f64,
        frames: usize,
        sample_rate: f64,
        points_per_block: usize,
    ) -> Vec<(i32, f64)> {
        if self.points.is_empty() || frames == 0 {
            return Vec::new();
        }
        let n = points_per_block.clamp(1, frames);
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let offset = (i * frames) / n;
            let time = block_start_secs + offset as f64 / sample_rate;
            if let Some(value) = self.value_at_time(time) {
                out.push((offset as i32, value));
            }
        }
        out
    }
}

impl Default for ParameterAutomation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_point_with_nan_time_does_not_panic() {
        // A NaN time is ordered deterministically by `total_cmp` in the sort, never panicking.
        let auto = ParameterAutomation::new()
            .add_point(0.0, 0.1)
            .add_point(f64::NAN, 0.5)
            .add_point(1.0, 0.9);
        assert_eq!(auto.points.len(), 3);

        // Sane ordering: the finite points keep ascending-time order, and the NaN is placed
        // deterministically (`total_cmp` sorts a positive NaN after all finite values) rather
        // than corrupting the sequence or panicking.
        let finite: Vec<f64> = auto
            .points
            .iter()
            .map(|p| p.time)
            .filter(|t| t.is_finite())
            .collect();
        assert_eq!(finite, vec![0.0, 1.0]);
        assert!(auto.points.last().unwrap().time.is_nan());
    }

    #[test]
    fn add_point_with_nan_value_is_not_used_in_ordering() {
        // The sort keys on time only, so a NaN *value* can never reach the comparator and
        // can never break ordering or panic. Lock that in.
        let auto = ParameterAutomation::new()
            .add_point(2.0, f64::NAN)
            .add_point(1.0, 0.5)
            .add_point(0.0, 0.25);
        let times: Vec<f64> = auto.points.iter().map(|p| p.time).collect();
        assert_eq!(times, vec![0.0, 1.0, 2.0]);
    }
}
