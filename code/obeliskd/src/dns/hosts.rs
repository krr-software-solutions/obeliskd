//! What DNS answers with: every host, exact names and wildcard patterns.
//!
//! A `*` is a whole label and matches exactly one label, so a pattern only matches names with the
//! same number of labels. When several match, the most specific wins, judged from the right (from
//! the zone outward): at the first label where two patterns differ, a literal beats `*`. Two
//! different patterns that both match a name always differ somewhere, and there one is a literal and
//! the other `*`, so there are no ties. An exact name beats every pattern.

use std::collections::HashMap;
use std::net::Ipv4Addr;

const WILDCARD: &str = "*";

#[derive(Clone, Debug, Default)]
pub struct HostTable {
    exact: HashMap<String, Ipv4Addr>,
    /// Each pattern's labels, right to left (zone first).
    patterns: Vec<(Vec<String>, Ipv4Addr)>,
}

impl HostTable {
    /// The address `name` (lowercase, no trailing dot) resolves to.
    #[must_use]
    pub fn lookup(&self, name: &str) -> Option<Ipv4Addr> {
        if let Some(&address) = self.exact.get(name) {
            return Some(address);
        }
        let labels = reversed(name);
        self.patterns
            .iter()
            .filter(|(pattern, _)| matches(pattern, &labels))
            .max_by_key(|(pattern, _)| specificity(pattern))
            .map(|&(_, address)| address)
    }

    /// Every host that matches `name`, the one [`lookup`](Self::lookup) answers with first: the
    /// exact name if there is one, then the patterns from most to least specific.
    #[must_use]
    pub fn matching(&self, name: &str) -> Vec<String> {
        let labels = reversed(name);
        let mut patterns: Vec<&Vec<String>> = self
            .patterns
            .iter()
            .map(|(pattern, _)| pattern)
            .filter(|pattern| matches(pattern, &labels))
            .collect();
        patterns.sort_by_key(|pattern| std::cmp::Reverse(specificity(pattern)));
        self.exact
            .contains_key(name)
            .then(|| name.to_string())
            .into_iter()
            .chain(patterns.into_iter().map(|p| unreversed(p)))
            .collect()
    }

    /// Whether some host is strictly below `name`: then `name` exists too (an "empty non-terminal",
    /// RFC 8020), even without a record of its own.
    #[must_use]
    pub fn has_below(&self, name: &str) -> bool {
        let labels = reversed(name);
        let below =
            |host: &[String]| host.len() > labels.len() && matches(&host[..labels.len()], &labels);
        self.exact.keys().any(|h| below(&reversed(h)))
            || self.patterns.iter().any(|(pattern, _)| below(pattern))
    }
}

impl FromIterator<(String, Ipv4Addr)> for HostTable {
    fn from_iter<I: IntoIterator<Item = (String, Ipv4Addr)>>(hosts: I) -> Self {
        let mut table = HostTable::default();
        for (name, address) in hosts {
            if name.split('.').any(|l| l == WILDCARD) {
                table.patterns.push((reversed(&name), address));
            } else {
                table.exact.insert(name, address);
            }
        }
        table
    }
}

impl<const N: usize> From<[(String, Ipv4Addr); N]> for HostTable {
    fn from(hosts: [(String, Ipv4Addr); N]) -> Self {
        hosts.into_iter().collect()
    }
}

fn reversed(name: &str) -> Vec<String> {
    name.rsplit('.').map(str::to_string).collect()
}

fn unreversed(labels: &[String]) -> String {
    labels
        .iter()
        .rev()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(".")
}

/// Same length, and every label equal or `*` (both right to left).
fn matches(pattern: &[String], labels: &[String]) -> bool {
    pattern.len() == labels.len()
        && pattern
            .iter()
            .zip(labels)
            .all(|(p, l)| p == WILDCARD || p == l)
}

/// Compared lexicographically, right to left: a literal (`true`) beats `*` at the first difference.
fn specificity(pattern: &[String]) -> Vec<bool> {
    pattern.iter().map(|l| l != WILDCARD).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> HostTable {
        let host = |name: &str, last: u8| {
            (
                format!("{name}.obd.internal"),
                Ipv4Addr::new(10, 57, 0, last),
            )
        };
        HostTable::from([
            host("*.*.x", 1),
            host("a.*.x", 2),
            host("*.b.x", 3),
            host("c.d.x", 4),
        ])
    }

    fn lookup(name: &str) -> Option<u8> {
        table()
            .lookup(&format!("{name}.obd.internal"))
            .map(|a| a.octets()[3])
    }

    #[test]
    fn the_most_specific_match_wins_from_the_right() {
        assert_eq!(lookup("c.d.x"), Some(4), "exact beats *.*.x");
        assert_eq!(
            lookup("a.b.x"),
            Some(3),
            "*.b.x beats a.*.x: b is nearer the zone"
        );
        assert_eq!(lookup("a.q.x"), Some(2), "a.*.x beats *.*.x");
        assert_eq!(lookup("z.b.x"), Some(3), "*.b.x beats *.*.x");
        assert_eq!(lookup("p.q.x"), Some(1), "only *.*.x");
    }

    #[test]
    fn matching_lists_the_winner_then_what_it_shadows() {
        let t = HostTable::from([
            (
                "*.*.x.obd.internal".to_string(),
                Ipv4Addr::new(10, 57, 0, 1),
            ),
            (
                "a.*.x.obd.internal".to_string(),
                Ipv4Addr::new(10, 57, 0, 2),
            ),
            (
                "*.b.x.obd.internal".to_string(),
                Ipv4Addr::new(10, 57, 0, 3),
            ),
            (
                "a.b.x.obd.internal".to_string(),
                Ipv4Addr::new(10, 57, 0, 4),
            ),
        ]);
        assert_eq!(
            t.matching("a.b.x.obd.internal"),
            [
                "a.b.x.obd.internal",
                "*.b.x.obd.internal",
                "a.*.x.obd.internal",
                "*.*.x.obd.internal"
            ]
        );
        assert_eq!(
            t.matching("z.b.x.obd.internal"),
            ["*.b.x.obd.internal", "*.*.x.obd.internal"]
        );
        assert_eq!(t.matching("q.x.obd.internal"), Vec::<String>::new());
        // The first match is always the answer.
        for name in ["a.b.x", "z.b.x", "a.q.x", "p.q.x", "c.d.x"] {
            let name = format!("{name}.obd.internal");
            let first = t.matching(&name).first().cloned();
            let answer = first.and_then(|host| t.lookup(&host));
            assert_eq!(answer, t.lookup(&name), "{name}");
        }
    }

    #[test]
    fn a_wildcard_is_exactly_one_label() {
        assert_eq!(lookup("q.x"), None, "too short");
        assert_eq!(
            lookup("a.b.c.x"),
            None,
            "too long: * never spans two labels"
        );
        assert_eq!(lookup("x"), None);
    }

    #[test]
    fn names_with_hosts_below_them_exist() {
        let t = table();
        for name in ["x", "b.x", "q.x", "d.x"] {
            assert!(t.has_below(&format!("{name}.obd.internal")), "{name}");
        }
        assert!(
            !t.has_below("c.d.x.obd.internal"),
            "nothing below an exact leaf"
        );
        assert!(!t.has_below("y.obd.internal"));
    }
}
