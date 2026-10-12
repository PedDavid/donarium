//! Durations as written in `ApiKey` specs and config: `<n><unit>` with unit
//! one of `s`, `m`, `h`, `d`, `w` (e.g. `90d`, `2w`).

use jiff::SignedDuration;

/// Most digits a duration may have. Five keeps every value (up to 99999
/// weeks) far from overflowing a timestamp, and the CRD schema enforces the
/// same limit through [`PATTERN`], so the API server cannot store a value
/// the controller would reject.
pub const MAX_DIGITS: usize = 5;

/// [`parse`]'s grammar as a regex, for the CRD schema.
pub const PATTERN: &str = r"^[0-9]{1,5}(s|m|h|d|w)$";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid duration {0:?}: expected <number><s|m|h|d|w> with at most 5 digits, e.g. 90d")]
pub struct ParseDurationError(pub String);

pub fn parse(input: &str) -> Result<SignedDuration, ParseDurationError> {
    let err = || ParseDurationError(input.to_string());
    let input_trimmed = input.trim();
    let split = input_trimmed.len().checked_sub(1).ok_or_else(err)?;
    if !input_trimmed.is_char_boundary(split) {
        return Err(err());
    }
    let (number, unit) = input_trimmed.split_at(split);
    if number.is_empty() || number.len() > MAX_DIGITS || !number.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(err());
    }
    let n: i64 = number.parse().map_err(|_| err())?;
    let seconds_per_unit: i64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(err()),
    };
    let secs = n.checked_mul(seconds_per_unit).ok_or_else(err)?;
    Ok(SignedDuration::from_secs(secs))
}

/// Formats a duration for humans, with the largest fitting unit: `3d`, `5h`, `12m`.
pub fn humanize(d: SignedDuration) -> String {
    let secs = d.as_secs().abs();
    let (value, unit) = if secs >= 86_400 {
        (secs / 86_400, "d")
    } else if secs >= 3600 {
        (secs / 3600, "h")
    } else if secs >= 60 {
        (secs / 60, "m")
    } else {
        (secs, "s")
    };
    format!("{value}{unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units() {
        assert_eq!(parse("90d").unwrap(), SignedDuration::from_hours(90 * 24));
        assert_eq!(parse("2w").unwrap(), SignedDuration::from_hours(14 * 24));
        assert_eq!(parse("12h").unwrap(), SignedDuration::from_hours(12));
        assert_eq!(parse("5m").unwrap(), SignedDuration::from_mins(5));
        assert_eq!(parse("30s").unwrap(), SignedDuration::from_secs(30));
        assert_eq!(
            parse("99999w").unwrap(),
            SignedDuration::from_hours(99_999 * 7 * 24)
        );
    }

    #[test]
    fn pattern_matches_the_parser() {
        assert!(PATTERN.contains(&format!("{{1,{MAX_DIGITS}}}")));
        assert!(PATTERN.ends_with("(s|m|h|d|w)$"));
        // The largest accepted value still fits a timestamp's range.
        let max = parse("99999w").unwrap();
        assert!(jiff::Timestamp::now().checked_add(max).is_ok());
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "d",
            "10",
            "-1d",
            "1.5d",
            "10y",
            "1dd",
            "99999999999999999w",
            "100000s",
            "1é",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn humanizes() {
        assert_eq!(humanize(SignedDuration::from_hours(49)), "2d");
        assert_eq!(humanize(SignedDuration::from_hours(-3)), "3h");
        assert_eq!(humanize(SignedDuration::from_secs(59)), "59s");
    }
}
