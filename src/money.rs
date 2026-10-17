//! Amounts of money. They are whole numbers of the currency's smallest unit
//! ("minor units": cents for EUR, yen for JPY), never floating point, so what
//! was donated is stored and summed exactly.

/// Digits after the decimal point for a currency: 2 for most, 0 for yen and
/// won, 3 for the Gulf dinars.
pub fn minor_exponent(currency: &str) -> u32 {
    match currency {
        "BIF" | "CLP" | "DJF" | "GNF" | "ISK" | "JPY" | "KMF" | "KRW" | "PYG" | "RWF" | "UGX"
        | "UYI" | "VND" | "VUV" | "XAF" | "XOF" | "XPF" => 0,
        "BHD" | "IQD" | "JOD" | "KWD" | "LYD" | "OMR" | "TND" => 3,
        _ => 2,
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct MoneyError(pub String);

fn err<T>(msg: impl Into<String>) -> Result<T, MoneyError> {
    Err(MoneyError(msg.into()))
}

/// Normalises a currency code typed by an admin: three letters, upper-cased.
pub fn parse_currency(input: &str) -> Result<String, MoneyError> {
    let code = input.trim().to_ascii_uppercase();
    if code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase()) {
        Ok(code)
    } else {
        err("enter the currency as a three-letter code, e.g. EUR")
    }
}

/// Largest amount accepted, in minor units: far beyond any donation, and far
/// from overflowing a signed 64-bit total.
const MAX_MINOR: i64 = 1_000_000_000_000;

/// Reads an amount such as `12`, `12.5` or `12,50` into minor units. Thousands
/// separators are refused rather than guessed at, because `1,234` is either
/// 1.234 or 1234 depending on where you live.
pub fn parse_amount(input: &str, currency: &str) -> Result<i64, MoneyError> {
    let exponent = minor_exponent(currency) as usize;
    let input = input.trim();
    let (whole, fraction) = match input.split_once(['.', ',']) {
        Some((w, f)) => (w, f),
        None => (input, ""),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if input.is_empty()
        || whole.is_empty() && fraction.is_empty()
        || !digits(whole)
        || !digits(fraction)
        || fraction.contains(['.', ','])
    {
        return err("enter the amount as a number such as 12.50, without thousands separators");
    }
    if fraction.len() > exponent {
        return err(format!(
            "{currency} has {exponent} decimal place{}",
            if exponent == 1 { "" } else { "s" }
        ));
    }
    let mut minor: i64 = 0;
    for digit in whole.bytes().chain(
        fraction
            .bytes()
            .chain(std::iter::repeat_n(b'0', exponent - fraction.len())),
    ) {
        minor = minor
            .checked_mul(10)
            .and_then(|m| m.checked_add(i64::from(digit - b'0')))
            .filter(|m| *m <= MAX_MINOR)
            .ok_or_else(|| MoneyError("that amount is too large".into()))?;
    }
    if minor == 0 {
        return err("the amount must be greater than zero");
    }
    Ok(minor)
}

/// `12.50` for 1250 EUR cents; `1500` for 1500 yen.
pub fn format_amount(minor: i64, currency: &str) -> String {
    let exponent = minor_exponent(currency);
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    if exponent == 0 {
        return format!("{sign}{abs}");
    }
    let unit = 10u64.pow(exponent);
    format!(
        "{sign}{}.{:0width$}",
        abs / unit,
        abs % unit,
        width = exponent as usize
    )
}

/// An exchange rate: how many units of the base currency one unit of the
/// donated currency is worth.
pub fn parse_rate(input: &str) -> Result<f64, MoneyError> {
    let rate: f64 = input
        .trim()
        .replace(',', ".")
        .parse()
        .map_err(|_| MoneyError("the rate must be a number, e.g. 0.92".into()))?;
    if !rate.is_finite() || rate <= 0.0 || rate > 1_000_000.0 {
        return err("the rate must be greater than 0");
    }
    Ok(rate)
}

/// Rates are used to this many decimal places, which is more than any source
/// publishes (the ECB gives four to six significant digits).
const RATE_DECIMALS: u32 = 10;

/// Converts an amount in `from`'s minor units to `to`'s minor units, rounding
/// half away from zero. `rate` is units of `to` per unit of `from`.
///
/// The arithmetic is on integers: with floating point, `10.00 * 0.9255` is
/// 9.254999… and a tie that should round up rounds down.
pub fn convert(minor: i64, from: &str, rate: f64, to: &str) -> Option<i64> {
    if !rate.is_finite() || rate <= 0.0 || minor <= 0 {
        return None;
    }
    let scaled_rate = (rate * 10f64.powi(RATE_DECIMALS as i32)).round();
    if !(1.0..1e22).contains(&scaled_rate) {
        return None;
    }
    let scaled_rate = scaled_rate as i128;
    // minor / 10^from_exp * rate * 10^to_exp, as one fraction.
    let numerator = i128::from(minor) * scaled_rate * 10i128.pow(minor_exponent(to));
    let denominator = 10i128.pow(minor_exponent(from) + RATE_DECIMALS);
    // Round half up (the numbers are positive, so that is away from zero).
    let converted = (numerator * 2 + denominator) / (denominator * 2);
    if converted < 1 || converted > i128::from(MAX_MINOR) {
        // Rounds to nothing, or is too large to store.
        return None;
    }
    i64::try_from(converted).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_places_per_currency() {
        assert_eq!(minor_exponent("EUR"), 2);
        assert_eq!(minor_exponent("USD"), 2);
        assert_eq!(minor_exponent("JPY"), 0);
        assert_eq!(minor_exponent("KRW"), 0);
        assert_eq!(minor_exponent("KWD"), 3);
    }

    #[test]
    fn amounts_are_parsed_exactly() {
        let p = |s: &str, c: &str| parse_amount(s, c);
        assert_eq!(p("12", "EUR"), Ok(1200));
        assert_eq!(p("12.5", "EUR"), Ok(1250));
        assert_eq!(p("12,50", "EUR"), Ok(1250));
        assert_eq!(p(" 0.05 ", "EUR"), Ok(5));
        assert_eq!(p(".5", "EUR"), Ok(50));
        assert_eq!(p("12.", "EUR"), Ok(1200));
        assert_eq!(p("1500", "JPY"), Ok(1500));
        assert_eq!(p("1.234", "KWD"), Ok(1234));
        // The classic float failure: 0.1 + 0.2, and 1.15 * 100.
        assert_eq!(p("1.15", "EUR"), Ok(115));
        assert_eq!(p("0.29", "EUR"), Ok(29));
        assert_eq!(p("10000000000", "EUR"), Ok(1_000_000_000_000));
    }

    #[test]
    fn bad_amounts_are_refused() {
        for (input, currency) in [
            ("", "EUR"),
            (".", "EUR"),
            ("abc", "EUR"),
            ("-5", "EUR"),
            ("+5", "EUR"),
            ("1e3", "EUR"),
            ("1,234.56", "EUR"),
            ("1.234,56", "EUR"),
            ("1 000", "EUR"),
            ("12.345", "EUR"),
            ("1.5", "JPY"),
            ("0", "EUR"),
            ("0.00", "EUR"),
            ("10000000001", "EUR"),
            ("99999999999999999999", "EUR"),
        ] {
            assert!(
                parse_amount(input, currency).is_err(),
                "{input:?} {currency}"
            );
        }
        assert!(
            parse_amount("12.345", "EUR")
                .unwrap_err()
                .0
                .contains("2 decimal places")
        );
        assert!(
            parse_amount("1.5", "JPY")
                .unwrap_err()
                .0
                .contains("0 decimal places")
        );
    }

    #[test]
    fn amounts_are_formatted() {
        assert_eq!(format_amount(1250, "EUR"), "12.50");
        assert_eq!(format_amount(5, "EUR"), "0.05");
        assert_eq!(format_amount(100, "EUR"), "1.00");
        assert_eq!(format_amount(1500, "JPY"), "1500");
        assert_eq!(format_amount(1234, "KWD"), "1.234");
        assert_eq!(format_amount(7, "KWD"), "0.007");
        assert_eq!(format_amount(-250, "EUR"), "-2.50");
        assert_eq!(format_amount(0, "EUR"), "0.00");
        // What is formatted can be parsed back.
        for minor in [1, 99, 100, 123_456, 1_000_000_000_000] {
            assert_eq!(parse_amount(&format_amount(minor, "EUR"), "EUR"), Ok(minor));
        }
    }

    #[test]
    fn currencies_and_rates() {
        assert_eq!(parse_currency(" eur "), Ok("EUR".into()));
        for bad in ["", "EU", "EURO", "E1R", "€", "US D"] {
            assert!(parse_currency(bad).is_err(), "{bad:?}");
        }
        assert_eq!(parse_rate("0.92"), Ok(0.92));
        assert_eq!(parse_rate("0,92"), Ok(0.92));
        for bad in ["", "x", "0", "-1", "NaN", "inf", "1e9"] {
            assert!(parse_rate(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn conversion_rounds_to_the_target_currencys_unit() {
        // 10.00 USD at 0.925 EUR is 9.25 EUR.
        assert_eq!(convert(1000, "USD", 0.925, "EUR"), Some(925));
        // Ties round up: 10.00 * 0.9255 is exactly 9.255, and floats get it wrong.
        assert_eq!(convert(1000, "USD", 0.9255, "EUR"), Some(926));
        assert_eq!(convert(1000, "USD", 0.9254, "EUR"), Some(925));
        assert_eq!(convert(1000, "USD", 0.92549, "EUR"), Some(925));
        assert_eq!(convert(5, "USD", 0.5, "EUR"), Some(3)); // 0.05 * 0.5 = 0.025
        // The same sum for many amounts: no drift from accumulated error.
        for cents in 1..=2000i64 {
            let exact = (cents as i128 * 9255 * 2 + 10_000) / 20_000;
            assert_eq!(
                convert(cents, "USD", 0.9255, "EUR"),
                i64::try_from(exact).ok().filter(|v| *v >= 1),
                "{cents}"
            );
        }
        // Yen have no cents: 1500 JPY at 0.0061 EUR is 9.15 EUR.
        assert_eq!(convert(1500, "JPY", 0.0061, "EUR"), Some(915));
        // And into yen.
        assert_eq!(convert(1000, "EUR", 160.0, "JPY"), Some(1600));
        assert_eq!(convert(1000, "EUR", 1.0, "KWD"), Some(10_000));
        // An amount that rounds to nothing cannot be stored.
        assert_eq!(convert(1, "JPY", 0.0001, "EUR"), None);
        // Nonsense in, nothing out.
        assert_eq!(convert(0, "EUR", 1.0, "EUR"), None);
        assert_eq!(convert(100, "EUR", f64::NAN, "EUR"), None);
        assert_eq!(convert(100, "EUR", 0.0, "EUR"), None);
        assert_eq!(convert(100, "EUR", f64::INFINITY, "EUR"), None);
        assert_eq!(convert(i64::MAX, "EUR", 1e6, "EUR"), None);
    }
}
