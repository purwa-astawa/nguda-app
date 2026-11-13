//! Lexorank order keys for flexible items (AD-6).
//!
//! Keys are non-empty strings over the base-62 alphabet `0-9A-Za-z`, whose ASCII order
//! matches digit order, so keys compare as byte strings. No key ends in `0`, so another
//! key always fits before any key.

use std::fmt;

use crate::error::DomainError;

const BASE: u8 = 62;

fn digit_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A'..=b'Z' => Some(c - b'A' + 10),
        b'a'..=b'z' => Some(c - b'a' + 36),
        _ => None,
    }
}

fn digit_char(d: u8) -> u8 {
    match d {
        0..=9 => b'0' + d,
        10..=35 => b'A' + d - 10,
        _ => b'a' + d - 36,
    }
}

/// A position among flexible items. Ordered by byte string.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderKey(String);

impl OrderKey {
    /// Validates a stored key: non-empty, base-62 digits only, not ending in `0`.
    pub fn new(key: impl Into<String>) -> Result<Self, DomainError> {
        let key = key.into();
        let valid =
            !key.is_empty() && key.bytes().all(|c| digit_value(c).is_some()) && !key.ends_with('0');
        if valid {
            Ok(Self(key))
        } else {
            Err(DomainError::InvalidOrder)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A key strictly between `low` and `high`. `None` means an open end: before
    /// everything or after everything. Fails with `InvalidOrder` unless `low < high`.
    pub fn between(low: Option<&OrderKey>, high: Option<&OrderKey>) -> Result<Self, DomainError> {
        if let (Some(a), Some(b)) = (low, high)
            && a >= b
        {
            return Err(DomainError::InvalidOrder);
        }
        let to_digits = |k: &OrderKey| -> Vec<u8> {
            k.0.bytes()
                .map(|c| digit_value(c).expect("validated key"))
                .collect()
        };
        let a = low.map(to_digits).unwrap_or_default();
        let b = high.map(to_digits);
        let mid = midpoint(&a, b.as_deref());
        Ok(Self(
            mid.into_iter().map(|d| digit_char(d) as char).collect(),
        ))
    }
}

impl fmt::Display for OrderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Digits strictly between `a` (missing digits read as 0) and `b` (`None` reads as
/// past the last digit). Requires `a < b` and neither ending in 0; never returns a
/// trailing 0.
fn midpoint(a: &[u8], b: Option<&[u8]>) -> Vec<u8> {
    if let Some(b) = b {
        let n = b
            .iter()
            .enumerate()
            .take_while(|&(i, &d)| a.get(i).copied().unwrap_or(0) == d)
            .count();
        if n > 0 {
            let mut out = b[..n].to_vec();
            out.extend(midpoint(a.get(n..).unwrap_or(&[]), Some(&b[n..])));
            return out;
        }
    }
    let da = a.first().copied().unwrap_or(0);
    let db = b.map(|b| b[0]).unwrap_or(BASE);
    if db - da > 1 {
        vec![(da + db) / 2]
    } else if let Some(b) = b.filter(|b| b.len() > 1) {
        vec![b[0]]
    } else {
        let mut out = vec![da];
        out.extend(midpoint(a.get(1..).unwrap_or(&[]), None));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::key;

    fn between(a: Option<&str>, b: Option<&str>) -> OrderKey {
        let (a, b) = (a.map(key), b.map(key));
        let k = OrderKey::between(a.as_ref(), b.as_ref()).unwrap();
        if let Some(a) = &a {
            assert!(a < &k, "{a} < {k}");
        }
        if let Some(b) = &b {
            assert!(&k < b, "{k} < {b}");
        }
        assert!(!k.as_str().ends_with('0'));
        k
    }

    #[test]
    fn alphabet_order_matches_byte_order() {
        let chars: Vec<u8> = (0..BASE).map(digit_char).collect();
        assert!(chars.windows(2).all(|w| w[0] < w[1]));
        assert!(
            chars
                .iter()
                .enumerate()
                .all(|(i, &c)| digit_value(c) == Some(i as u8))
        );
    }

    #[test]
    fn between_open_ends_and_neighbours() {
        assert_eq!(between(None, None).as_str(), "V");
        between(None, Some("1"));
        between(None, Some("01"));
        between(Some("z"), None);
        between(Some("zz"), None);
        between(Some("a"), Some("b"));
        between(Some("a"), Some("a1"));
        between(Some("a"), Some("a01"));
        between(Some("az"), Some("b"));
        between(Some("1"), Some("2"));
        between(Some("V"), Some("W"));
        between(Some("Vz"), Some("W1"));
    }

    #[test]
    fn rejects_unordered_bounds() {
        let (a, b) = (key("b"), key("a"));
        assert_eq!(
            OrderKey::between(Some(&a), Some(&b)),
            Err(DomainError::InvalidOrder)
        );
        assert_eq!(
            OrderKey::between(Some(&a), Some(&a)),
            Err(DomainError::InvalidOrder)
        );
    }

    #[test]
    fn rejects_malformed_keys() {
        for bad in ["", "a0", "a-b", "é"] {
            assert_eq!(
                OrderKey::new(bad),
                Err(DomainError::InvalidOrder),
                "{bad:?}"
            );
        }
    }

    fn assert_sorted_unique(keys: &[OrderKey]) {
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn thousand_inserts_at_the_front() {
        let mut keys: Vec<OrderKey> = Vec::new();
        for _ in 0..1000 {
            let k = OrderKey::between(None, keys.first()).unwrap();
            keys.insert(0, k);
        }
        assert_sorted_unique(&keys);
    }

    #[test]
    fn thousand_inserts_at_the_end() {
        let mut keys: Vec<OrderKey> = Vec::new();
        for _ in 0..1000 {
            let k = OrderKey::between(keys.last(), None).unwrap();
            keys.push(k);
        }
        assert_sorted_unique(&keys);
    }

    #[test]
    fn thousand_inserts_between_neighbours() {
        let mut keys = vec![key("1"), key("2")];
        // Always insert right after the first key, narrowing the same gap.
        for _ in 0..1000 {
            let k = OrderKey::between(Some(&keys[0]), Some(&keys[1])).unwrap();
            keys.insert(1, k);
        }
        assert_sorted_unique(&keys);
        // And spread inserts across varying positions.
        let mut keys = vec![key("V")];
        for i in 0..1000usize {
            let pos = (i * 7919) % (keys.len() + 1);
            let k = OrderKey::between(pos.checked_sub(1).map(|p| &keys[p]), keys.get(pos)).unwrap();
            keys.insert(pos, k);
        }
        assert_sorted_unique(&keys);
    }
}
