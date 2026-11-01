//! Features: what belay does, each with its own setting, and the user's kill switch
//! (`belay pause`, `belay resume`). belay acts within the agent's capabilities; a
//! feature that is off or paused takes no action and records none.

use super::{Controller, Decision};
use crate::record::Body;

/// The test guard: block a weakening edit, only say so, or leave it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardSetting {
    Deny,
    Warn,
    Off,
}

impl GuardSetting {
    pub fn name(self) -> &'static str {
        match self {
            GuardSetting::Deny => "deny",
            GuardSetting::Warn => "warn",
            GuardSetting::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "deny" => Some(GuardSetting::Deny),
            "warn" => Some(GuardSetting::Warn),
            "off" => Some(GuardSetting::Off),
            _ => None,
        }
    }
}

/// How much the user hears about what belay did (read by reporting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportLevel {
    Quiet,
    Normal,
    Verbose,
}

impl ReportLevel {
    pub fn name(self) -> &'static str {
        match self {
            ReportLevel::Quiet => "quiet",
            ReportLevel::Normal => "normal",
            ReportLevel::Verbose => "verbose",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "quiet" => Some(ReportLevel::Quiet),
            "normal" => Some(ReportLevel::Normal),
            "verbose" => Some(ReportLevel::Verbose),
            _ => None,
        }
    }
}

/// A feature the user can pause. Each is one config key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Feature {
    Guard,
    Loops,
    Shape,
    WrapTests,
    Memory,
    Awareness,
    Commons,
}

impl Feature {
    pub const ALL: [Feature; 7] = [
        Feature::Guard,
        Feature::Loops,
        Feature::Shape,
        Feature::WrapTests,
        Feature::Memory,
        Feature::Awareness,
        Feature::Commons,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Feature::Guard => "guard",
            Feature::Loops => "loops",
            Feature::Shape => "shape",
            Feature::WrapTests => "wrap_tests",
            Feature::Memory => "memory",
            Feature::Awareness => "awareness",
            Feature::Commons => "commons",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Feature::ALL.into_iter().find(|f| f.name() == s)
    }
}

/// What `belay pause` without a feature names in the ledger.
pub const ALL_FEATURES: &str = "all";
const PAUSED: &str = "paused";

impl Controller {
    /// The feature's configured value, as the config file writes it.
    fn configured(&self, f: Feature) -> String {
        let on_off = |b: bool| if b { "on" } else { "off" }.to_string();
        match f {
            Feature::Guard => self.cfg.guard.name().into(),
            Feature::Loops => on_off(self.cfg.loops),
            Feature::Shape => on_off(self.cfg.shape),
            Feature::WrapTests => self.cfg.wrap_tests.to_string(),
            Feature::Memory => on_off(self.cfg.memory),
            Feature::Awareness => on_off(self.cfg.awareness),
            Feature::Commons => on_off(self.cfg.commons),
        }
    }

    /// The feature is configured on and not paused.
    pub fn on(&self, f: Feature) -> bool {
        if self.paused.contains(&f) {
            return false;
        }
        match f {
            Feature::Guard => self.cfg.guard != GuardSetting::Off,
            Feature::Loops => self.cfg.loops,
            Feature::Shape => self.cfg.shape,
            Feature::WrapTests => self.cfg.wrap_tests,
            Feature::Memory => self.cfg.memory,
            Feature::Awareness => self.cfg.awareness,
            Feature::Commons => self.cfg.commons,
        }
    }

    /// The guard as it applies now: off while paused.
    pub(super) fn guard_setting(&self) -> GuardSetting {
        if self.paused.contains(&Feature::Guard) {
            GuardSetting::Off
        } else {
            self.cfg.guard
        }
    }

    /// Paused features, in a stable order.
    pub fn paused(&self) -> Vec<Feature> {
        self.paused.iter().copied().collect()
    }

    /// Pauses an earlier daemon of this session recorded (a plugin daemon exits when
    /// idle): a pause holds until the user resumes.
    pub fn restore_paused(&mut self, entries: &[crate::record::Entry]) {
        for e in entries {
            let Body::FeatureChanged {
                feature, from, to, ..
            } = &e.body
            else {
                continue;
            };
            let named: Vec<Feature> = match Feature::parse(feature) {
                Some(f) => vec![f],
                None if feature == ALL_FEATURES => Feature::ALL.to_vec(),
                None => continue,
            };
            for f in named {
                if to == PAUSED {
                    self.paused.insert(f);
                } else if from == PAUSED {
                    self.paused.remove(&f);
                }
            }
        }
    }

    /// The user pauses one feature, or all (`None`). Err is a message for the user.
    pub fn pause(
        &mut self,
        feature: Option<Feature>,
        by: &str,
        now_ms: u64,
    ) -> Result<Decision, String> {
        let wanted: Vec<Feature> = match feature {
            Some(f) => vec![f],
            None => Feature::ALL.to_vec(),
        };
        let fresh: Vec<Feature> = wanted
            .into_iter()
            .filter(|f| !self.paused.contains(f))
            .collect();
        if fresh.is_empty() {
            return Err(match feature {
                Some(f) => format!("{} is already paused.", f.name()),
                None => "Every feature is already paused.".into(),
            });
        }
        let mut d = Decision::default();
        let label = match feature {
            Some(f) => f.name().to_string(),
            None => ALL_FEATURES.into(),
        };
        let from = match feature {
            Some(f) => self.configured(f),
            None => "on".into(),
        };
        self.paused.extend(fresh);
        self.push(
            &mut d,
            now_ms,
            vec![],
            Body::FeatureChanged {
                feature: label,
                from,
                to: PAUSED.into(),
                by: by.into(),
            },
        );
        Ok(d)
    }

    /// The user resumes one feature, or all (`None`). Err is a message for the user.
    pub fn resume(
        &mut self,
        feature: Option<Feature>,
        by: &str,
        now_ms: u64,
    ) -> Result<Decision, String> {
        let was: Vec<Feature> = match feature {
            Some(f) if self.paused.contains(&f) => vec![f],
            Some(f) => return Err(format!("{} is not paused.", f.name())),
            None if self.paused.is_empty() => return Err("Nothing is paused.".into()),
            None => self.paused.iter().copied().collect(),
        };
        for f in &was {
            self.paused.remove(f);
        }
        let mut d = Decision::default();
        let (label, to) = match feature {
            Some(f) => (f.name().to_string(), self.configured(f)),
            None => (ALL_FEATURES.to_string(), "on".to_string()),
        };
        self.push(
            &mut d,
            now_ms,
            vec![],
            Body::FeatureChanged {
                feature: label,
                from: PAUSED.into(),
                to,
                by: by.into(),
            },
        );
        Ok(d)
    }
}
