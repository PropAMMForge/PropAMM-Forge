//! Conversion between human numbers and the chain's raw units.
//!
//! # Why a separate module for what looks like a multiplication
//!
//! The program **does not read** the mints' decimals — that would cost two extra
//! accounts in the SC-002 budget. So the whole conversion of "150.25 SOL/USDC"
//! into `mid_e9` happens here, off chain, and a mistake in it has no on-chain
//! guard beneath it: it simply posts a price three orders of magnitude off, and
//! the vault hands out its inventory at it entirely legitimately.
//!
//! For that reason the module deliberately sees neither the network nor the
//! config: everything here is checked by a plain `cargo test` on numbers.
//!
//! # Why a custom parser rather than `f64`
//!
//! `"0.1".parse::<f64>()` gives 0.100000000000000005…, and at scale 1e9 the
//! difference surfaces in the low digits of the price. Here a decimal string is
//! parsed into a "mantissa + number of digits" pair, and the rest is integer arithmetic in `u128`.
//!
//! # Rounding is visible, not silent
//!
//! [`Decimal::to_raw`] **refuses** if the number is finer than the mint's smallest
//! unit: "transfer 0.0000001 USDC" is not zero, it is a mistake in the argument.
//! [`Decimal::to_mid_e9`] cannot refuse (a price almost never divides exactly),
//! so it also returns an exactness flag, and the command prints the price that
//! will really go on chain.

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Context, Result};

/// Fixed-point scale of the price — the same as [`propamm_quote::PRICE_SCALE`].
const PRICE_SCALE_POW: u32 = 9;

/// Ceiling on the number of decimal places in an entered number.
///
/// No mint has such precision (`decimals` in SPL is a `u8`, and in practice
/// ≤ 9), so a longer tail is almost certainly not a price but glued-together arguments.
const MAX_SCALE: u32 = 30;

/// A decimal number parsed exactly: `mantissa / 10^scale`.
///
/// No negatives on purpose: everything that comes here is prices and sizes, and
/// a minus in them would mean an argument-parsing mistake, not a positive intent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Decimal {
    mantissa: u128,
    scale: u32,
}

impl Decimal {
    /// Strip trailing zeros without changing the value.
    ///
    /// Needed before the precision checks: "1.500000" and "1.5" are one number, and
    /// the first must not be refused on a mint with one decimal place.
    fn normalized(self) -> Self {
        let mut value = self;
        while value.scale > 0 && value.mantissa.is_multiple_of(10) {
            value.mantissa /= 10;
            value.scale -= 1;
        }
        value
    }

    /// Raw number of mint units.
    ///
    /// # Errors
    ///
    /// If the number is finer than the mint's smallest unit, equals zero or does
    /// not fit in `u64`.
    pub fn to_raw(self, decimals: u8) -> Result<u64> {
        let value = self.normalized();
        let decimals = u32::from(decimals);
        if value.scale > decimals {
            bail!(
                "{self} is finer than the mint's smallest unit: it has {decimals} decimals, this has {}",
                value.scale
            );
        }
        // The difference is non-negative by the check above.
        let shift = decimals - value.scale;
        let raw = value
            .mantissa
            .checked_mul(pow10(shift)?)
            .with_context(|| format!("{self} is too large for raw units"))?;
        let raw = u64::try_from(raw).with_context(|| format!("{self} does not fit in u64"))?;
        if raw == 0 {
            bail!("{self} is a zero amount");
        }
        Ok(raw)
    }

    /// The price in `mid_e9` form: raw units of quote per raw unit of base,
    /// multiplied by 1e9.
    ///
    /// Also returns whether the division came out exact. The caller is obliged to
    /// show the result to a human: a conversion that silently drops the tail is a
    /// price the owner believes is posted while the chain sees another.
    ///
    /// # Errors
    ///
    /// If an intermediate product does not fit in `u128` or the result is zero.
    pub fn to_mid_e9(self, base_decimals: u8, quote_decimals: u8) -> Result<(u128, bool)> {
        let value = self.normalized();
        let numerator = value
            .mantissa
            .checked_mul(pow10(u32::from(quote_decimals) + PRICE_SCALE_POW)?)
            .with_context(|| format!("price {self} is too large for u128"))?;
        let denominator = pow10(value.scale + u32::from(base_decimals))?;

        let mid_e9 = numerator / denominator;
        let exact = numerator % denominator == 0;
        if mid_e9 == 0 {
            bail!(
                "price {self} turns into zero on this pair: one raw unit of the base asset costs less than 1e-9 of a raw unit of quote"
            );
        }
        Ok((mid_e9, exact))
    }
}

impl FromStr for Decimal {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let (integer, fraction) = match text.split_once('.') {
            Some((integer, fraction)) => (integer, fraction),
            None => (text, ""),
        };
        if integer.is_empty() && fraction.is_empty() {
            bail!("\"{text}\" is not a number");
        }
        // Digit separators and exponents are deliberately not accepted: "1_000" and
        // "1e3" each have two readings depending on the tool, and a price with two
        // readings is a price that cannot be checked by eye.
        let digits: String = format!("{integer}{fraction}");
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("\"{text}\" is not a positive decimal number");
        }
        let scale = u32::try_from(fraction.len()).unwrap_or(u32::MAX);
        if scale > MAX_SCALE {
            bail!("\"{text}\" has {scale} decimal places — that does not look like a number");
        }
        let mantissa = digits
            .parse::<u128>()
            .with_context(|| format!("\"{text}\" is too large"))?;
        Ok(Self { mantissa, scale })
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render(self.mantissa, self.scale))
    }
}

/// A raw amount in human form.
#[must_use]
pub fn format_raw(raw: u64, decimals: u8) -> String {
    render(u128::from(raw), u32::from(decimals))
}

/// `mid_e9` back into a human price.
///
/// This is the reverse side of [`Decimal::to_mid_e9`], and it is what `status`
/// and the `quote` confirmation print: a human checks what they typed against it.
#[must_use]
pub fn format_mid_e9(mid_e9: u128, base_decimals: u8, quote_decimals: u8) -> String {
    let Ok(scaled) = pow10(u32::from(base_decimals)).and_then(|factor| {
        mid_e9
            .checked_mul(factor)
            .context("price is too large for human form")
    }) else {
        // Numbers of that size cannot be a real price, but printing must not
        // fail: the raw value still says something, a panic says nothing.
        return format!("{mid_e9} (mid_e9)");
    };
    render(scaled, PRICE_SCALE_POW + u32::from(quote_decimals))
}

/// `value / 10^scale` as a string, without trailing zeros.
fn render(value: u128, scale: u32) -> String {
    let Ok(divisor) = pow10(scale) else {
        return value.to_string();
    };
    let integer = value / divisor;
    let fraction = value % divisor;
    if fraction == 0 {
        return integer.to_string();
    }
    let width = scale as usize;
    let mut digits = format!("{fraction:0width$}");
    while digits.ends_with('0') {
        digits.pop();
    }
    format!("{integer}.{digits}")
}

/// `10^exponent` in `u128`.
///
/// # Errors
///
/// If the power does not fit — i.e. the arguments are no longer about money.
fn pow10(exponent: u32) -> Result<u128> {
    10u128
        .checked_pow(exponent)
        .with_context(|| format!("10^{exponent} does not fit in u128"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(text: &str) -> Decimal {
        text.parse().expect("the number did not parse")
    }

    #[test]
    fn a_plain_integer_parses() {
        assert_eq!(dec("5000").to_raw(6).unwrap(), 5_000_000_000);
    }

    #[test]
    fn a_fraction_parses_exactly() {
        assert_eq!(dec("0.000001").to_raw(6).unwrap(), 1);
        assert_eq!(dec("1.5").to_raw(9).unwrap(), 1_500_000_000);
    }

    /// Trailing zeros must not make a number "too precise" for the mint.
    #[test]
    fn trailing_zeros_do_not_make_a_number_too_precise() {
        assert_eq!(dec("1.500000").to_raw(1).unwrap(), 15);
    }

    /// The main reason `to_raw` refuses rather than rounds: a silent zero would
    /// look like a successful transfer of nothing.
    #[test]
    fn a_number_below_the_smallest_unit_is_refused() {
        let err = dec("0.0000001").to_raw(6).unwrap_err();
        assert!(
            format!("{err}").contains("smallest unit"),
            "the message is not about precision: {err}"
        );
    }

    #[test]
    fn zero_is_not_an_amount() {
        let err = dec("0").to_raw(6).unwrap_err();
        assert!(format!("{err}").contains("zero"), "{err}");
    }

    #[test]
    fn what_is_not_a_number_is_refused() {
        for text in ["", ".", "abc", "1e9", "1_000", "-1", "1.2.3", "1 000"] {
            assert!(
                text.parse::<Decimal>().is_err(),
                "\"{text}\" passed as a number"
            );
        }
    }

    /// The number it is easiest to get wrong by three orders of magnitude: SOL has
    /// 9 decimals, USDC 6, and the difference between them goes into `mid_e9`.
    #[test]
    fn the_canonical_sol_usdc_price_converts() {
        let (mid_e9, exact) = dec("150.25").to_mid_e9(9, 6).unwrap();
        assert_eq!(mid_e9, 150_250_000);
        assert!(exact, "150.25 on a 9/6 pair divides exactly");
    }

    /// A pair with equal precision: `mid_e9` equals the price multiplied by 1e9.
    #[test]
    fn equal_decimals_leave_the_price_scaled_by_1e9() {
        let (mid_e9, exact) = dec("2.5").to_mid_e9(6, 6).unwrap();
        assert_eq!(mid_e9, 2_500_000_000);
        assert!(exact);
    }

    #[test]
    fn a_price_that_does_not_divide_evenly_reports_itself_as_inexact() {
        // 9 base decimals against 6 quote decimals leaves exactly 6 price digits.
        let (_, exact) = dec("150.2500001").to_mid_e9(9, 6).unwrap();
        assert!(!exact, "tail beyond the precision limit went unnoticed");
    }

    #[test]
    fn a_price_that_rounds_to_nothing_is_refused() {
        let err = dec("0.0000000001").to_mid_e9(9, 0).unwrap_err();
        assert!(format!("{err}").contains("zero"), "{err}");
    }

    /// The module's most important property: a human types a price, `status` prints
    /// it back, and it must be the same number.
    #[test]
    fn a_price_survives_the_round_trip_through_mid_e9() {
        for (text, base, quote) in [
            ("150.25", 9u8, 6u8),
            ("1", 6, 6),
            ("0.000001", 6, 9),
            ("42000.5", 8, 6),
            ("2.5", 0, 0),
        ] {
            let (mid_e9, exact) = dec(text).to_mid_e9(base, quote).unwrap();
            assert!(exact, "{text} on {base}/{quote} did not divide exactly");
            assert_eq!(
                format_mid_e9(mid_e9, base, quote),
                text,
                "{text} on the {base}/{quote} pair came back different"
            );
        }
    }

    #[test]
    fn a_raw_amount_survives_the_round_trip() {
        for (text, decimals) in [("5000", 6u8), ("1.5", 9), ("0.000001", 6), ("7", 0)] {
            let raw = dec(text).to_raw(decimals).unwrap();
            assert_eq!(
                format_raw(raw, decimals),
                text,
                "{text} at {decimals} decimals"
            );
        }
    }

    #[test]
    fn rendering_drops_trailing_zeros_but_keeps_leading_ones() {
        assert_eq!(format_raw(1, 6), "0.000001");
        assert_eq!(format_raw(1_500_000, 6), "1.5");
        assert_eq!(format_raw(1_000_000, 6), "1");
        assert_eq!(format_raw(0, 6), "0");
    }

    /// Printing must not fail even on numbers that cannot be a price.
    #[test]
    fn an_absurd_price_still_prints_something() {
        let text = format_mid_e9(u128::MAX, 9, 6);
        assert!(text.contains("mid_e9"), "{text}");
    }
}
