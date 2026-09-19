//! {{description}}

/// Adds two `i64`s, checked: `None` on overflow instead of panicking. Stand-in for "every public
/// function gets a unit test" — see `skill.md`.
pub fn checked_add(a: i64, b: i64) -> Option<i64> {
    a.checked_add(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_two_numbers() {
        assert_eq!(checked_add(2, 3), Some(5));
    }

    #[test]
    fn overflow_returns_none() {
        assert_eq!(checked_add(i64::MAX, 1), None);
    }
}
