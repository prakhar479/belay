//! Features: what belay does, each with its own setting, and the user's kill switch
//! (`belay pause`, `belay resume`). belay acts within the agent's capabilities; a
//! feature that is off or paused takes no action and records none.

use std::collections::{BTreeMap, BTreeSet};

use super::{Controller, Decision};
use crate::record::{Body, Notice, NoticeLevel};

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
    Verify,
    Memory,
    Awareness,
    Commons,
}

impl Feature {
    pub const ALL: [Feature; 8] = [
        Feature::Guard,
        Feature::Loops,
        Feature::Shape,
        Feature::WrapTests,
        Feature::Verify,
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
            Feature::Verify => "verify",
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

/// Allows of one guard rule after which it only warns, for the rest of the session.
const ALLOWS_TO_WARN: usize = 2;
/// How a guard rule is named in `FeatureChanged`: `guard:<rule>`.
const GUARD_RULE: &str = "guard:";

/// Guard feedback (U6.3): the user's allows, by rule, and the rules they made warn.
#[derive(Debug, Clone, Default)]
pub(super) struct GuardFeedback {
    /// The override entries that allowed a block of each rule.
    allows: BTreeMap<String, Vec<u64>>,
    /// Rules that warn instead of blocking.
    warned: BTreeSet<String>,
}

impl GuardFeedback {
    /// Every rule of a block was allowed often enough: the guard only warns.
    pub(super) fn warns(&self, rules: &[String]) -> bool {
        !rules.is_empty() && rules.iter().all(|r| self.warned.contains(r))
    }

    /// For the state digest.
    pub(super) fn digest(&self) -> String {
        format!("{self:?}")
    }
}

/// The guard rules behind a block's reasons, each once, in a stable order.
pub(super) fn rules_of(reasons: &[String]) -> Vec<String> {
    let rules: BTreeSet<&str> = reasons.iter().map(|r| crate::guard::rule_of(r)).collect();
    rules.into_iter().map(str::to_string).collect()
}

impl Controller {
    /// The feature's configured value, as the config file writes it.
    fn configured(&self, f: Feature) -> String {
        let on_off = |b: bool| if b { "on" } else { "off" }.to_string();
        match f {
            Feature::Guard => self.cfg.guard.name().into(),
            Feature::Loops => on_off(self.cfg.loops),
            Feature::Shape => on_off(self.cfg.shape),
            Feature::WrapTests => self.cfg.wrap_tests.to_string(),
            Feature::Verify => on_off(self.cfg.verify),
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
            Feature::Verify => self.cfg.verify,
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
            if let Some(rule) = feature.strip_prefix(GUARD_RULE) {
                if to == GuardSetting::Warn.name() {
                    self.guard_fb.warned.insert(rule.to_string());
                } else {
                    self.guard_fb.warned.remove(rule);
                    self.guard_fb.allows.remove(rule);
                }
                continue;
            }
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
        // `resume guard` (or all) also gives back deny to rules the user's allows made
        // warn (U6.3).
        let rules: Vec<String> = match feature {
            None | Some(Feature::Guard) => self.guard_fb.warned.iter().cloned().collect(),
            Some(_) => Vec::new(),
        };
        let was: Vec<Feature> = match feature {
            Some(f) if self.paused.contains(&f) => vec![f],
            Some(_) if !rules.is_empty() => vec![],
            Some(f) => return Err(format!("{} is not paused.", f.name())),
            None if self.paused.is_empty() && rules.is_empty() => {
                return Err("Nothing is paused.".into())
            }
            None => self.paused.iter().copied().collect(),
        };
        for f in &was {
            self.paused.remove(f);
        }
        let mut d = Decision::default();
        for rule in rules {
            self.guard_fb.warned.remove(&rule);
            self.guard_fb.allows.remove(&rule);
            self.push(
                &mut d,
                now_ms,
                vec![],
                Body::FeatureChanged {
                    feature: format!("{GUARD_RULE}{rule}"),
                    from: GuardSetting::Warn.name().into(),
                    to: GuardSetting::Deny.name().into(),
                    by: by.into(),
                },
            );
        }
        if was.is_empty() {
            return Ok(d);
        }
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

impl Controller {
    /// The user allowed a block of these rules (override entry `seq`). A rule allowed
    /// `ALLOWS_TO_WARN` times only warns from now on: recorded, with the allows as
    /// causes, and told to the user (U6.3).
    pub(super) fn guard_allowed(&mut self, d: &mut Decision, now: u64, rules: &[String], seq: u64) {
        for rule in rules {
            let allows = self.guard_fb.allows.entry(rule.clone()).or_default();
            allows.push(seq);
            let causes = allows.clone();
            if causes.len() < ALLOWS_TO_WARN
                || self.guard_fb.warned.contains(rule)
                || self.guard_setting() != GuardSetting::Deny
            {
                continue;
            }
            self.guard_fb.warned.insert(rule.clone());
            let changed = self.push(
                d,
                now,
                causes,
                Body::FeatureChanged {
                    feature: format!("{GUARD_RULE}{rule}"),
                    from: GuardSetting::Deny.name().into(),
                    to: GuardSetting::Warn.name().into(),
                    by: "allows".into(),
                },
            );
            let notice = Notice {
                level: NoticeLevel::Act,
                text: format!(
                    "you allowed the guard rule `{rule}` twice: it only warns for the rest of this session. `belay resume guard` restores deny."
                ),
                action: None,
            };
            // The user reads it in `belay allow`'s reply.
            self.push(
                d,
                now,
                vec![changed],
                Body::Notice {
                    notice: notice.clone(),
                    shown: true,
                },
            );
            d.response.notices.push(notice);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::record::{Body, Entry};
    use crate::{Capabilities, Config, Controller, Decision, Event, Feature, Permission};

    fn ctl() -> Controller {
        Controller::new(Config::default(), Capabilities::CLAUDE_CODE)
    }

    /// An edit that removes the `n`th of several assertions.
    fn weakening(n: usize) -> Event {
        let old: String = (0..4).map(|i| format!("assert f({i}) == {i}\n")).collect();
        let new: String = (0..4)
            .filter(|i| *i != n)
            .map(|i| format!("assert f({i}) == {i}\n"))
            .collect();
        Event::EditProposed {
            tool_use_id: format!("w{n}"),
            path: "tests/test_api.py".into(),
            old_text: old,
            new_text: new,
        }
    }

    #[derive(Default)]
    struct Kept {
        ledger: Vec<Entry>,
        blobs: BTreeMap<String, String>,
    }

    impl Kept {
        fn keep(&mut self, d: &Decision) {
            for b in &d.blobs {
                self.blobs.insert(b.hash.clone(), b.content.clone());
            }
            for e in &d.entries {
                let line = serde_json::to_string(e).unwrap();
                self.ledger.push(serde_json::from_str(&line).unwrap());
            }
        }
    }

    fn blocks(d: &Decision) -> bool {
        matches!(d.response.permission, Some(Permission::Deny(_)))
    }

    fn warns(d: &Decision) -> bool {
        d.response.permission.is_none()
            && d.response
                .add_context
                .iter()
                .any(|t| t.contains("may weaken tests"))
    }

    #[test]
    fn two_allows_of_a_rule_make_it_warn_until_resume() {
        let mut c = ctl();
        let mut kept = Kept::default();
        let step = |c: &mut Controller, kept: &mut Kept, ev: Event, now: u64| {
            let d = c.handle(ev, now);
            kept.keep(&d);
            d
        };
        assert!(blocks(&step(&mut c, &mut kept, weakening(0), 1)));
        let first = c.allow(None, 2).unwrap();
        assert!(first.response.notices.is_empty());
        assert!(!first
            .entries
            .iter()
            .any(|e| matches!(e.body, Body::FeatureChanged { .. })));
        kept.keep(&first);
        assert!(blocks(&step(&mut c, &mut kept, weakening(1), 3)));
        let second = c.allow(None, 4).unwrap();
        kept.keep(&second);
        let overrides: Vec<u64> = kept
            .ledger
            .iter()
            .filter(|e| matches!(e.body, Body::Override { .. }))
            .map(|e| e.seq)
            .collect();
        let changed = second
            .entries
            .iter()
            .find(|e| matches!(e.body, Body::FeatureChanged { .. }))
            .expect("the rule changed");
        assert_eq!(
            changed.body,
            Body::FeatureChanged {
                feature: "guard:assertions_removed".into(),
                from: "deny".into(),
                to: "warn".into(),
                by: "allows".into(),
            }
        );
        assert_eq!(changed.causes, overrides);
        assert_eq!(second.response.notices.len(), 1);
        assert!(second.response.notices[0]
            .text
            .contains("`belay resume guard`"));

        // The same rule now only warns, through the normal warn path.
        let d = step(&mut c, &mut kept, weakening(2), 5);
        assert!(warns(&d), "{:?}", d.response);
        // A daemon restart replays the allows: the rule still warns.
        let mut again = ctl();
        let blobs = kept.blobs.clone();
        again.restore(&kept.ledger, &|h| blobs.get(h).cloned());
        assert_eq!(again.state_digest(), c.state_digest());
        assert!(warns(&again.handle(weakening(3), 6)));

        // `belay resume guard` restores deny, and survives a restart too.
        let r = c.resume(Some(Feature::Guard), "user", 7).unwrap();
        assert!(r.entries.iter().any(|e| e.body
            == Body::FeatureChanged {
                feature: "guard:assertions_removed".into(),
                from: "warn".into(),
                to: "deny".into(),
                by: "user".into(),
            }));
        kept.keep(&r);
        assert!(blocks(&c.handle(weakening(3), 8)));
        let mut again = ctl();
        again.restore(&kept.ledger, &|h| blobs.get(h).cloned());
        assert!(blocks(&again.handle(weakening(3), 9)));
        // Nothing more to resume.
        assert!(c.resume(Some(Feature::Guard), "user", 10).is_err());
    }

    #[test]
    fn guard_warn_setting_makes_no_feedback() {
        let mut c = Controller::new(
            Config {
                guard: super::GuardSetting::Warn,
                ..Config::default()
            },
            Capabilities::CLAUDE_CODE,
        );
        for n in 0..3 {
            assert!(warns(&c.handle(weakening(n), n as u64)));
        }
        assert!(c.allow(None, 9).is_err(), "nothing was blocked");
    }
}
