//! Fixed-point decimal for order sizes and prices.
//!
//! `value = mantissa / 10^scale`. Built from the exchange's raw strings; floats are never
//! involved, so `0.1 + 0.2` style drift cannot reach an order. Every operation is checked:
//! overflow or too many digits is an error, which callers treat as "not tradable".

use std::cmp::Ordering;
use std::fmt;

/// Most significant digits accepted from a string.
const MAX_DIGITS: usize = 20;
/// Largest scale accepted from a string, after trailing zeros are stripped.
const MAX_SCALE: u32 = 18;
/// Scales above this can no longer be aligned inside an `i128`.
const MAX_INTERNAL_SCALE: u32 = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecimalError {
    /// Not `[+-]digits[.digits]` (exponents, spaces and empty strings are rejected).
    Syntax,
    /// More digits or decimals than the fixed-point form can hold.
    TooPrecise,
    /// An operation overflowed.
    Overflow,
    /// A step or divisor of zero.
    ZeroStep,
}

impl fmt::Display for DecimalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DecimalError::Syntax => "invalid decimal",
            DecimalError::TooPrecise => "decimal has too many digits",
            DecimalError::Overflow => "decimal overflow",
            DecimalError::ZeroStep => "step is zero",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal {
    mantissa: i128,
    scale: u32,
}

// Order values must not leak through `{:?}` into logs (condition 38).
impl fmt::Debug for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Decimal(<redacted>)")
    }
}

fn pow10(exp: u32) -> Result<i128, DecimalError> {
    10i128.checked_pow(exp).ok_or(DecimalError::Overflow)
}

impl Decimal {
    pub const ZERO: Decimal = Decimal {
        mantissa: 0,
        scale: 0,
    };

    fn normalized(mut mantissa: i128, mut scale: u32) -> Decimal {
        while scale > 0 && mantissa % 10 == 0 {
            mantissa /= 10;
            scale -= 1;
        }
        if mantissa == 0 {
            scale = 0;
        }
        Decimal { mantissa, scale }
    }

    pub fn from_int(value: i64) -> Decimal {
        Decimal::normalized(value as i128, 0)
    }

    pub fn from_i128(value: i128) -> Result<Decimal, DecimalError> {
        if value.unsigned_abs() > 10u128.pow(MAX_DIGITS as u32) {
            return Err(DecimalError::Overflow);
        }
        Ok(Decimal::normalized(value, 0))
    }

    /// How many whole `step`s fit in `self`, truncated towards zero.
    pub fn whole_steps(&self, step: &Decimal) -> Result<i128, DecimalError> {
        let (a, s, _) = Decimal::align(self, step)?;
        if s <= 0 {
            return Err(DecimalError::ZeroStep);
        }
        Ok(a / s)
    }

    /// Parse a plain decimal string. Rejects exponents, whitespace, empty parts and
    /// anything that cannot be held exactly.
    pub fn parse(s: &str) -> Result<Decimal, DecimalError> {
        let (negative, rest) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let (int_part, frac_part) = match rest.split_once('.') {
            Some((i, f)) => (i, f),
            None => (rest, ""),
        };
        let digits_ok = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
        if int_part.is_empty() || !digits_ok(int_part) || !digits_ok(frac_part) {
            return Err(DecimalError::Syntax);
        }
        if rest.contains('.') && frac_part.is_empty() {
            return Err(DecimalError::Syntax);
        }
        let frac_trimmed = frac_part.trim_end_matches('0');
        let int_trimmed = int_part.trim_start_matches('0');
        let digits = format!("{int_trimmed}{frac_trimmed}");
        let scale = frac_trimmed.len();
        if digits.len() > MAX_DIGITS || scale as u32 > MAX_SCALE {
            return Err(DecimalError::TooPrecise);
        }
        let mantissa: i128 = if digits.is_empty() {
            0
        } else {
            digits.parse().map_err(|_| DecimalError::Overflow)?
        };
        let mantissa = if negative { -mantissa } else { mantissa };
        Ok(Decimal::normalized(mantissa, scale as u32))
    }

    pub fn is_zero(&self) -> bool {
        self.mantissa == 0
    }

    pub fn is_positive(&self) -> bool {
        self.mantissa > 0
    }

    pub fn is_negative(&self) -> bool {
        self.mantissa < 0
    }

    fn align(a: &Decimal, b: &Decimal) -> Result<(i128, i128, u32), DecimalError> {
        let scale = a.scale.max(b.scale);
        let am = a
            .mantissa
            .checked_mul(pow10(scale - a.scale)?)
            .ok_or(DecimalError::Overflow)?;
        let bm = b
            .mantissa
            .checked_mul(pow10(scale - b.scale)?)
            .ok_or(DecimalError::Overflow)?;
        Ok((am, bm, scale))
    }

    pub fn checked_cmp(&self, other: &Decimal) -> Result<Ordering, DecimalError> {
        let (a, b, _) = Decimal::align(self, other)?;
        Ok(a.cmp(&b))
    }

    pub fn checked_add(&self, other: &Decimal) -> Result<Decimal, DecimalError> {
        let (a, b, scale) = Decimal::align(self, other)?;
        Ok(Decimal::normalized(
            a.checked_add(b).ok_or(DecimalError::Overflow)?,
            scale,
        ))
    }

    pub fn checked_sub(&self, other: &Decimal) -> Result<Decimal, DecimalError> {
        let (a, b, scale) = Decimal::align(self, other)?;
        Ok(Decimal::normalized(
            a.checked_sub(b).ok_or(DecimalError::Overflow)?,
            scale,
        ))
    }

    pub fn checked_mul(&self, other: &Decimal) -> Result<Decimal, DecimalError> {
        let scale = self.scale + other.scale;
        if scale > MAX_INTERNAL_SCALE {
            return Err(DecimalError::Overflow);
        }
        let mantissa = self
            .mantissa
            .checked_mul(other.mantissa)
            .ok_or(DecimalError::Overflow)?;
        Ok(Decimal::normalized(mantissa, scale))
    }

    /// `self / other`, cut towards zero at `scale` decimals (a volume-weighted average price,
    /// which needs no more than the tick that rounds it afterwards).
    pub fn checked_div_trunc(&self, other: &Decimal, scale: u32) -> Result<Decimal, DecimalError> {
        if other.mantissa == 0 {
            return Err(DecimalError::ZeroStep);
        }
        let num = self
            .mantissa
            .checked_mul(pow10(
                scale
                    .checked_add(other.scale)
                    .ok_or(DecimalError::Overflow)?,
            )?)
            .ok_or(DecimalError::Overflow)?;
        let den = other
            .mantissa
            .checked_mul(pow10(self.scale)?)
            .ok_or(DecimalError::Overflow)?;
        Ok(Decimal::normalized(num / den, scale))
    }

    /// Absolute value; `i128::MIN` has none and is an overflow, not a panic.
    pub fn checked_abs(&self) -> Result<Decimal, DecimalError> {
        Ok(Decimal {
            mantissa: self.mantissa.checked_abs().ok_or(DecimalError::Overflow)?,
            scale: self.scale,
        })
    }

    /// Round down to a multiple of `step` (towards negative infinity).
    pub fn floor_to_step(&self, step: &Decimal) -> Result<Decimal, DecimalError> {
        let (a, s, scale) = Decimal::align(self, step)?;
        if s <= 0 {
            return Err(DecimalError::ZeroStep);
        }
        let q = a.div_euclid(s);
        Ok(Decimal::normalized(
            q.checked_mul(s).ok_or(DecimalError::Overflow)?,
            scale,
        ))
    }

    /// Round up to a multiple of `step` (towards positive infinity).
    pub fn ceil_to_step(&self, step: &Decimal) -> Result<Decimal, DecimalError> {
        let (a, s, scale) = Decimal::align(self, step)?;
        if s <= 0 {
            return Err(DecimalError::ZeroStep);
        }
        let mut q = a.div_euclid(s);
        if a.rem_euclid(s) != 0 {
            q = q.checked_add(1).ok_or(DecimalError::Overflow)?;
        }
        Ok(Decimal::normalized(
            q.checked_mul(s).ok_or(DecimalError::Overflow)?,
            scale,
        ))
    }

    /// Round towards zero to a multiple of `step` (how order quantities are rounded).
    pub fn trunc_to_step(&self, step: &Decimal) -> Result<Decimal, DecimalError> {
        let (a, s, scale) = Decimal::align(self, step)?;
        if s <= 0 {
            return Err(DecimalError::ZeroStep);
        }
        let q = a / s; // `/` truncates towards zero
        Ok(Decimal::normalized(
            q.checked_mul(s).ok_or(DecimalError::Overflow)?,
            scale,
        ))
    }

    /// Round to the nearest multiple of `step`. A tie goes to the larger multiple
    /// (half up), so the result never depends on float-style banker's rounding.
    pub fn round_to_step(&self, step: &Decimal) -> Result<Decimal, DecimalError> {
        let (a, s, scale) = Decimal::align(self, step)?;
        if s <= 0 {
            return Err(DecimalError::ZeroStep);
        }
        let q = a.div_euclid(s);
        let rem = a.rem_euclid(s);
        // rem >= s - rem  <=>  the remainder is at least half a step.
        let up = rem >= s - rem;
        let q = if up {
            q.checked_add(1).ok_or(DecimalError::Overflow)?
        } else {
            q
        };
        Ok(Decimal::normalized(
            q.checked_mul(s).ok_or(DecimalError::Overflow)?,
            scale,
        ))
    }

    /// `self` is a whole multiple of `step`.
    pub fn is_multiple_of(&self, step: &Decimal) -> Result<bool, DecimalError> {
        let (a, s, _) = Decimal::align(self, step)?;
        if s <= 0 {
            return Err(DecimalError::ZeroStep);
        }
        Ok(a % s == 0)
    }

    /// The value as a whole non-negative integer, if it is one.
    pub fn to_u32_exact(&self) -> Option<u32> {
        if self.scale == 0 {
            u32::try_from(self.mantissa).ok()
        } else {
            None
        }
    }

    /// Decimal string without an exponent, e.g. `0.00100`-style input prints as `0.001`.
    pub fn to_plain_string(&self) -> String {
        let negative = self.mantissa < 0;
        let digits = self.mantissa.unsigned_abs().to_string();
        let scale = self.scale as usize;
        let body = if scale == 0 {
            digits
        } else if digits.len() > scale {
            let (i, f) = digits.split_at(digits.len() - scale);
            format!("{i}.{f}")
        } else {
            format!("0.{}{digits}", "0".repeat(scale - digits.len()))
        };
        if negative { format!("-{body}") } else { body }
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_plain_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::parse(s).unwrap()
    }

    #[test]
    fn division_cuts_towards_zero_at_the_given_scale() {
        assert_eq!(
            d("60010")
                .checked_div_trunc(&d("3"), 4)
                .unwrap()
                .to_plain_string(),
            "20003.3333"
        );
        assert_eq!(
            d("0.12")
                .checked_div_trunc(&d("0.04"), 2)
                .unwrap()
                .to_plain_string(),
            "3"
        );
        assert_eq!(
            d("-7")
                .checked_div_trunc(&d("2"), 0)
                .unwrap()
                .to_plain_string(),
            "-3"
        );
        assert_eq!(
            d("1").checked_div_trunc(&Decimal::ZERO, 2),
            Err(DecimalError::ZeroStep)
        );
    }

    #[test]
    fn parses_and_prints_plain_strings() {
        assert_eq!(d("0.00100000").to_plain_string(), "0.001");
        assert_eq!(d("100000.00000000").to_plain_string(), "100000");
        assert_eq!(d("0").to_plain_string(), "0");
        assert_eq!(d("-12.50").to_plain_string(), "-12.5");
        assert_eq!(d("000.5").to_plain_string(), "0.5");
        assert_eq!(d("0.00000001").to_plain_string(), "0.00000001");
    }

    #[test]
    fn rejects_exponents_floats_and_junk() {
        for s in [
            "", ".", "1.", ".5", "1e-8", "1E3", " 1", "1 ", "0x10", "--1", "1,5", "NaN",
        ] {
            assert!(Decimal::parse(s).is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn rejects_values_too_precise_to_hold_exactly() {
        assert_eq!(
            Decimal::parse("0.0000000000000000001"),
            Err(DecimalError::TooPrecise)
        );
        assert_eq!(
            Decimal::parse("123456789012345678901"),
            Err(DecimalError::TooPrecise)
        );
        // Trailing zeros do not count against the limit.
        assert!(Decimal::parse("0.100000000000000000000000").is_ok());
    }

    #[test]
    fn rounding_directions() {
        let step = d("0.01");
        assert_eq!(d("1.239").floor_to_step(&step).unwrap(), d("1.23"));
        assert_eq!(d("1.231").ceil_to_step(&step).unwrap(), d("1.24"));
        assert_eq!(d("1.23").ceil_to_step(&step).unwrap(), d("1.23"));
        assert_eq!(d("1.239").trunc_to_step(&step).unwrap(), d("1.23"));
        assert_eq!(d("-1.239").trunc_to_step(&step).unwrap(), d("-1.23"));
        assert_eq!(d("1.234").round_to_step(&step).unwrap(), d("1.23"));
        assert_eq!(d("1.236").round_to_step(&step).unwrap(), d("1.24"));
    }

    #[test]
    fn nearest_tie_goes_up_and_odd_steps_work() {
        assert_eq!(d("1.235").round_to_step(&d("0.01")).unwrap(), d("1.24"));
        // tick 0.5 and 0.25, which a power-of-ten tick type cannot represent
        assert_eq!(d("10.24").round_to_step(&d("0.5")).unwrap(), d("10"));
        assert_eq!(d("10.26").round_to_step(&d("0.5")).unwrap(), d("10.5"));
        assert_eq!(d("10.13").round_to_step(&d("0.25")).unwrap(), d("10.25"));
        assert_eq!(d("10.125").round_to_step(&d("0.25")).unwrap(), d("10.25"));
    }

    #[test]
    fn zero_step_is_an_error_not_a_panic() {
        let z = Decimal::ZERO;
        assert_eq!(d("1").floor_to_step(&z), Err(DecimalError::ZeroStep));
        assert_eq!(d("1").ceil_to_step(&z), Err(DecimalError::ZeroStep));
        assert_eq!(d("1").trunc_to_step(&z), Err(DecimalError::ZeroStep));
        assert_eq!(d("1").round_to_step(&z), Err(DecimalError::ZeroStep));
        assert_eq!(d("1").is_multiple_of(&z), Err(DecimalError::ZeroStep));
    }

    #[test]
    fn arithmetic_and_compare_are_exact() {
        assert_eq!(d("0.1").checked_add(&d("0.2")).unwrap(), d("0.3"));
        assert_eq!(d("2.5").checked_mul(&d("0.4")).unwrap(), d("1"));
        assert_eq!(d("1").checked_sub(&d("0.75")).unwrap(), d("0.25"));
        assert_eq!(d("0.3").checked_cmp(&d("0.30")).unwrap(), Ordering::Equal);
        assert_eq!(d("0.29").checked_cmp(&d("0.3")).unwrap(), Ordering::Less);
    }

    #[test]
    fn overflow_is_reported() {
        let big = d("99999999999999999999");
        assert_eq!(big.checked_mul(&big), Err(DecimalError::Overflow));
        let tiny = d("0.000000000000000001");
        assert_eq!(tiny.checked_mul(&tiny).map(|_| ()), Ok(()));
        let huge_scale = tiny.checked_mul(&tiny).unwrap();
        assert_eq!(huge_scale.checked_mul(&tiny), Err(DecimalError::Overflow));
    }

    #[test]
    fn debug_hides_the_value() {
        assert!(!format!("{:?}", d("123.45")).contains("123"));
    }

    #[test]
    fn checked_abs_handles_the_most_negative_mantissa() {
        let min = Decimal {
            mantissa: i128::MIN,
            scale: 0,
        };
        assert_eq!(min.checked_abs().unwrap_err(), DecimalError::Overflow);
        assert_eq!(d("-1.5").checked_abs().unwrap(), d("1.5"));
        assert_eq!(d("2").checked_abs().unwrap(), d("2"));
    }
}
