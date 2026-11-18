//! Content-free removal metadata, independent of the active notification limit.
//! Quickshell's sequential IDs compact into exact ranges: gaps are never filled,
//! so a late post cannot accidentally match a dismissal of another notification.
use anyhow::{Result, ensure};
use edge_protocol::{NotificationKey, NotificationRemoval};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(super) const MAX_REMOVALS: usize = 65_536;

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Removals {
    ranges: BTreeMap<String, BTreeMap<String, Vec<(u64, u64)>>>,
    keys: BTreeSet<NotificationKey>,
}

fn numbered(key: &NotificationKey) -> Option<(&str, u64)> {
    let (prefix, suffix) = key.instance.rsplit_once('/')?;
    let number: u64 = suffix.parse().ok()?;
    (number.to_string() == suffix).then_some((prefix, number))
}

impl Removals {
    pub fn contains(&self, key: &NotificationKey) -> bool {
        if let Some((prefix, number)) = numbered(key) {
            let Some(ranges) = self.ranges.get(&key.origin).and_then(|r| r.get(prefix)) else {
                return false;
            };
            let index = ranges.partition_point(|&(first, _)| first <= number);
            index > 0 && number <= ranges[index - 1].1
        } else {
            self.keys.contains(key)
        }
    }

    pub fn insert(&mut self, key: &NotificationKey) -> Result<bool> {
        if self.contains(key) {
            return Ok(false);
        }
        let removal = match numbered(key) {
            Some((prefix, number)) => NotificationRemoval::Range {
                origin: key.origin.clone(),
                prefix: prefix.into(),
                first: number,
                last: number,
            },
            None => NotificationRemoval::Key(key.clone()),
        };
        self.add(removal)?;
        Ok(true)
    }

    pub fn add(&mut self, removal: NotificationRemoval) -> Result<()> {
        match removal {
            NotificationRemoval::Key(key) => {
                super::notification_hub::validate_key(&key)?;
                ensure!(
                    numbered(&key).is_none(),
                    "numbered removal must use a range"
                );
                self.keys.insert(key);
            }
            NotificationRemoval::Range {
                origin,
                prefix,
                mut first,
                mut last,
            } => {
                super::notification_hub::validate_key(&NotificationKey {
                    origin: origin.clone(),
                    instance: format!("{prefix}/{last}"),
                })?;
                ensure!(first <= last, "invalid notification removal range");
                let ranges = self
                    .ranges
                    .entry(origin)
                    .or_default()
                    .entry(prefix)
                    .or_default();
                let start = ranges.partition_point(|&(_, end)| end.saturating_add(1) < first);
                let mut end = start;
                while end < ranges.len() && ranges[end].0 <= last.saturating_add(1) {
                    first = first.min(ranges[end].0);
                    last = last.max(ranges[end].1);
                    end += 1;
                }
                ranges.splice(start..end, [(first, last)]);
            }
        }
        ensure!(
            self.len() <= MAX_REMOVALS,
            "notification removal metadata is full"
        );
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.keys.len()
            + self
                .ranges
                .values()
                .flat_map(BTreeMap::values)
                .map(Vec::len)
                .sum::<usize>()
    }

    pub fn events(&self) -> impl Iterator<Item = NotificationRemoval> + '_ {
        self.ranges
            .iter()
            .flat_map(|(origin, prefixes)| {
                prefixes.iter().flat_map(move |(prefix, ranges)| {
                    ranges
                        .iter()
                        .map(move |&(first, last)| NotificationRemoval::Range {
                            origin: origin.clone(),
                            prefix: prefix.clone(),
                            first,
                            last,
                        })
                })
            })
            .chain(self.keys.iter().cloned().map(NotificationRemoval::Key))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.len() <= MAX_REMOVALS,
            "notification removal metadata is full"
        );
        let mut normalized = Self::default();
        for event in self.events() {
            normalized.add(event)?;
        }
        ensure!(
            &normalized == self,
            "noncanonical notification removal metadata"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(instance: &str) -> NotificationKey {
        NotificationKey {
            origin: "01234567:89abcdef:01234567:89abcdef".into(),
            instance: instance.into(),
        }
    }

    #[test]
    fn ranges_merge_in_any_order_without_filling_gaps_or_crossing_lifetimes() {
        let mut removed = Removals::default();
        for id in [1, 4, 3, 2, 7, 6] {
            removed.insert(&key(&format!("epoch/{id}"))).unwrap();
        }
        assert_eq!(removed.len(), 2);
        assert!(!removed.contains(&key("epoch/5")));
        assert!(!removed.contains(&key("other/3")));
        assert!(!removed.contains(&key("epoch/03")));
        let mut other_origin = key("epoch/3");
        other_origin.origin = "fedcba98:76543210:fedcba98:76543210".into();
        assert!(!removed.contains(&other_origin));
        removed.insert(&key("epoch/5")).unwrap();
        assert_eq!(removed.len(), 1);
        let restored: Removals =
            serde_json::from_slice(&serde_json::to_vec(&removed).unwrap()).unwrap();
        restored.validate().unwrap();
        for id in 1..=7 {
            assert!(restored.contains(&key(&format!("epoch/{id}"))));
        }
    }

    #[test]
    fn opaque_phone_lifetimes_and_integer_boundaries_remain_exact() {
        let mut removed = Removals::default();
        let phone = key("phone/abc/12/def");
        removed.insert(&phone).unwrap();
        assert!(!removed.contains(&key("phone/abc/12/fed")));
        for id in [0, u64::MAX, u64::MAX - 1] {
            removed.insert(&key(&format!("epoch/{id}"))).unwrap();
        }
        assert_eq!(removed.len(), 3);
        removed.validate().unwrap();
        assert!(removed.contains(&phone));
        assert!(!removed.contains(&key("epoch/1")));
        assert!(!removed.contains(&key(&format!("epoch/{}", u64::MAX - 2))));
    }
}
