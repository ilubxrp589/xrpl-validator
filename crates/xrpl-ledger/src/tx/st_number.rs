//! `STNumber` fields in engine state, and the asset-precision rounding the
//! vault and lending transactors apply to them (rippled 3.4.1).
//!
//! The engine keeps a ledger object as JSON. An STNumber field (type 9 —
//! `sfAssetsTotal`, `sfDebtTotal`, `sfPeriodicPayment`, …) is kept as the
//! STRING rippled's JSON shows, `to_string(Number)` (Number.cpp:1413-1505):
//! the binary codec reads that string back to the twelve wire bytes
//! (STNumber.cpp:68-107), so an engine write and a decoded object spell the
//! same value the same way. [`number_to_string`] and [`number_from_str`] are
//! the two directions; [`number_field`] / [`set_number_field`] read and
//! write a field with its template style.
//!
//! Fields flagged `SField::kSmdNeedsAsset` (sfields.macro:217-232) are
//! rounded to the precision of the object's asset whenever a transactor
//! finishes with the object: `associateAsset(sle, asset)`
//! (STTakesAsset.cpp:13-50) → `STNumber::associateAsset` → `roundToAsset`
//! (STNumber.cpp:55-66), which is `value = STAmount{asset, value}`
//! (STAmount.h:733-737) — whole units for XRP and MPT, sixteen significant
//! digits for an IOU, under the thread's rounding mode. [`associate_asset`]
//! is that pass. A port that stored the unrounded Number would serialize
//! different bytes.
//!
//! The helpers beside it: [`Amount`] (an `STAmount` of any asset, as far as
//! this arithmetic needs one), [`round_to_asset`] (STAmount.h:754-767),
//! [`round_to_scale`] (STAmount.cpp:1404-1424) and [`scale`]
//! (STAmount.h:793-796).
//!
//! Everything here runs at the thread's mantissa scale — Large330 whenever a
//! Vault can exist (`ledger::amendments::number_scale`).

use crate::flow::st_amount::StError;
use crate::ledger::field_meta::{self, SMD_NEEDS_ASSET};
use crate::tx::mpt::HeldAsset;
use crate::tx::number::{mantissa_scale, Number, Rounding, IOU_MAX_MANTISSA, IOU_MIN_MANTISSA};
use crate::tx::protocol::{ledger_format, Soe};
use serde_json::Value;

/// `Number::kMaxExponent`.
const NUMBER_MAX_EXPONENT: i32 = 32768;
/// `STAmount::kMinOffset` / `kMaxOffset`: an IOU's exponent range.
const IOU_MIN_OFFSET: i32 = -96;
const IOU_MAX_OFFSET: i32 = 80;
/// `kMaxNativeN` (STAmount.h): 1e17 drops.
const MAX_NATIVE: u64 = 100_000_000_000_000_000;
/// `kMaxMpTokenAmount` (Protocol.h:296).
const MAX_MPT: u64 = 0x7FFF_FFFF_FFFF_FFFF;

// ---------------------------------------------------------------------------
// Number <-> the JSON string
// ---------------------------------------------------------------------------

/// `to_string(Number)` (Number.cpp:1413-1505) at the thread's scale: "0" for
/// zero; scientific notation `<mantissa>e<exponent>` with the mantissa's
/// trailing zeros removed when the internal exponent is non-zero and outside
/// `[-(log + 10), -(log - 10)]` — `[-28, -8]` at Large330, `[-25, -5]` at
/// Small; otherwise plain decimal with leading and trailing zeros cropped.
/// It prints the INTERNAL mantissa and exponent, not the external view.
pub fn number_to_string(n: &Number) -> String {
    if n.is_zero() {
        return "0".to_string();
    }
    let range_log = mantissa_scale().log();
    let (mut exponent, mut mantissa) = (n.exponent, n.mantissa);
    let sign = if n.negative { "-" } else { "" };
    if exponent != 0 && (exponent < -(range_log + 10) || exponent > -(range_log - 10)) {
        while mantissa != 0 && mantissa.is_multiple_of(10) && exponent < NUMBER_MAX_EXPONENT {
            mantissa /= 10;
            exponent += 1;
        }
        let mut ret = format!("{sign}{mantissa}");
        if exponent != 0 {
            ret.push('e');
            ret.push_str(&exponent.to_string());
        }
        return ret;
    }
    // Number.cpp:1445-1503: pad the digits with zeros on both sides, split at
    // the decimal point, crop the padding.
    let pad_prefix = (range_log + 12) as usize;
    let pad_suffix = (range_log + 8) as usize;
    let raw = mantissa.to_string();
    let val = format!("{}{}{}", "0".repeat(pad_prefix), raw, "0".repeat(pad_suffix));
    let offset = (exponent + pad_prefix as i32 + range_log + 1) as usize;
    let (pre, post) = val.split_at(offset.min(val.len()));
    let pre = if pre.len() > pad_prefix { &pre[pad_prefix..] } else { pre };
    let pre = pre.trim_start_matches('0');
    let post = if post.len() > pad_suffix { &post[..post.len() - pad_suffix] } else { post };
    let post = post.trim_end_matches('0');
    let mut ret = String::from(sign);
    ret.push_str(if pre.is_empty() { "0" } else { pre });
    if !post.is_empty() {
        ret.push('.');
        ret.push_str(post);
    }
    ret
}

/// `partsFromString` (STNumber.cpp:160-219): a sign, an unsigned 64-bit
/// mantissa and an exponent from
/// `^[-+]?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][-+]?[0-9]+)?$`. None for anything
/// else, or digits that do not fit 64 bits (rippled's `lexical_cast` throws).
fn parts_from_str(s: &str) -> Option<(bool, u64, i32)> {
    let (negative, rest) = match s.as_bytes().first()? {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (base, exp) = match rest.find(['e', 'E']) {
        Some(i) => (&rest[..i], Some(&rest[i + 1..])),
        None => (rest, None),
    };
    let (int, frac) = match base.find('.') {
        Some(i) => (&base[..i], Some(&base[i + 1..])),
        None => (base, None),
    };
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || (int.len() > 1 && int.starts_with('0')) || frac.is_some_and(|f| !digits(f)) {
        return None;
    }
    let mut exponent: i32 = 0;
    let mantissa: u64 = match frac {
        None => int.parse().ok()?,
        Some(f) => {
            exponent = -(i32::try_from(f.len()).ok()?);
            format!("{int}{f}").parse().ok()?
        }
    };
    if let Some(e) = exp {
        let (minus, e) = match e.as_bytes().first()? {
            b'-' => (true, &e[1..]),
            b'+' => (false, &e[1..]),
            _ => (false, e),
        };
        if !digits(e) {
            return None;
        }
        let e: i32 = e.parse().ok()?;
        exponent = if minus { exponent.checked_sub(e)? } else { exponent.checked_add(e)? };
    }
    Some((negative, mantissa, exponent))
}

/// `numberFromJson` for a string (STNumber.cpp:221-299): the Number the
/// string names at the thread's scale, or None when it is not a number, or
/// names one the scale cannot hold EXACTLY ("number cannot be represented" —
/// rippled refuses a value it would have to round).
pub fn number_from_str(s: &str) -> Option<Number> {
    let (negative, mantissa, exponent) = parts_from_str(s)?;
    if mantissa == 0 {
        return Some(Number::ZERO);
    }
    let n = Number::from_parts(negative, mantissa as u128, exponent, Rounding::ToNearest).ok()?;
    // canonicalizeParts (STNumber.cpp:262-275): drop trailing zeros until
    // the exponents meet; the two must then be the same triple.
    let canonical = |mut m: u64, mut e: i32, neg: bool, other: i32| {
        if m == 0 {
            return (0u64, 0i32, false);
        }
        while e < other && m.is_multiple_of(10) {
            m /= 10;
            e += 1;
        }
        (m, e, neg)
    };
    let (ext_mantissa, ext_exponent) = n.external();
    let parts = canonical(mantissa, exponent, negative, ext_exponent);
    let number = canonical(ext_mantissa.unsigned_abs(), ext_exponent, ext_mantissa < 0, parts.1);
    (parts == number).then_some(n)
}

/// An STNumber as engine JSON holds it — the string, or (as rippled's
/// `numberFromJson` also takes) a JSON integer.
pub fn number_from_json(v: &Value) -> Option<Number> {
    match v {
        Value::String(s) => number_from_str(s),
        Value::Number(n) => {
            let (negative, magnitude) = match (n.as_i64(), n.as_u64()) {
                (Some(i), _) => (i < 0, i.unsigned_abs()),
                (None, Some(u)) => (false, u),
                _ => return None,
            };
            Number::from_parts(negative, magnitude as u128, 0, Rounding::ToNearest).ok()
        }
        _ => None,
    }
}

/// `sle->at(sfField)` for an STNumber: the field's value, zero when the
/// field is absent (a SoeDefault field at its default) or unreadable.
pub fn number_field(obj: &Value, field: &str) -> Number {
    opt_number_field(obj, field).unwrap_or(Number::ZERO)
}

/// `sle->at(~sfField)`: None when the field is absent.
pub fn opt_number_field(obj: &Value, field: &str) -> Option<Number> {
    obj.get(field).and_then(number_from_json)
}

/// `sle->at(sfField) = n` for an STNumber: the string form; a SoeDefault
/// field given zero is made absent (STObject.h:766-772).
pub fn set_number_field(obj: &mut Value, field: &str, n: Number, style: Soe) {
    if n.is_zero() && style == Soe::Default {
        if let Some(map) = obj.as_object_mut() {
            map.remove(field);
        }
        return;
    }
    obj[field] = Value::String(number_to_string(&n));
}

// ---------------------------------------------------------------------------
// STAmount of any asset
// ---------------------------------------------------------------------------

/// An `STAmount`: an asset, a sign, a mantissa and an exponent, canonical —
/// whole units with exponent 0 for XRP and MPT (`integral()`), a mantissa in
/// `[1e15, 1e16)` for an IOU, `(false, 0, 0)` for zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Amount {
    pub asset: HeldAsset,
    pub negative: bool,
    pub mantissa: u64,
    pub exponent: i32,
}

impl Amount {
    /// `Asset::integral()`: XRP and MPT amounts are whole numbers.
    pub fn integral(&self) -> bool {
        !matches!(self.asset, HeldAsset::Iou { .. })
    }

    pub fn is_zero(&self) -> bool {
        self.mantissa == 0
    }

    pub fn zero(asset: HeldAsset) -> Amount {
        Amount { asset, negative: false, mantissa: 0, exponent: 0 }
    }

    /// `STAmount(asset, mantissa, exponent, negative)` (STAmount.h:350-371)
    /// → `canonicalize()` (STAmount.cpp:821-875) under `mode`.
    ///
    /// Integral: off Number's Small scale a mantissa above int64's maximum
    /// throws; zero or an exponent ≤ −20 is zero; an exponent above 17 (XRP)
    /// or 18 (MPT) throws; the value is `Number(negative, mantissa,
    /// exponent, Unchecked)` cast to an integer under `mode`; above 1e17
    /// drops or the largest MPT amount throws. An IOU is
    /// `IOUAmount(Number{mantissa, exponent})`: sixteen digits under `mode`,
    /// zero below exponent −96, a throw above 80.
    pub fn new(asset: HeldAsset, mantissa: u128, exponent: i32, negative: bool, mode: Rounding) -> Result<Amount, StError> {
        let (max_exponent, max_value) = match asset {
            HeldAsset::Xrp => (17, MAX_NATIVE),
            HeldAsset::Mpt(_) => (18, MAX_MPT),
            HeldAsset::Iou { .. } => {
                let n = Number::from_parts(negative, mantissa, exponent, mode).map_err(|_| StError::Overflow)?;
                let (negative, mantissa, exponent) = n.iou_parts(mode).map_err(|_| StError::Overflow)?;
                if mantissa == 0 || exponent < IOU_MIN_OFFSET {
                    return Ok(Amount::zero(asset));
                }
                if exponent > IOU_MAX_OFFSET {
                    return Err(StError::Overflow);
                }
                return Ok(Amount { asset, negative, mantissa, exponent });
            }
        };
        if mantissa_scale() != crate::tx::number::MantissaScale::Small && mantissa > i64::MAX as u128 {
            return Err(StError::Overflow);
        }
        if mantissa == 0 || exponent <= -20 {
            return Ok(Amount::zero(asset));
        }
        if exponent > max_exponent {
            return Err(StError::Overflow);
        }
        let value = Number::rep_from_raw(negative, mantissa, exponent, mode).map_err(|_| StError::Overflow)?;
        if value.unsigned_abs() > max_value {
            return Err(StError::Overflow);
        }
        Ok(Amount { asset, negative: value < 0, mantissa: value.unsigned_abs(), exponent: 0 })
    }

    /// `STAmount{asset, number}` — `fromNumber` (STAmount.h:567-583): the
    /// magnitude converted under `mode` (`operator rep()` for an integral
    /// asset, `normalizeToRange` to sixteen digits for an IOU), the sign
    /// carried beside it into the constructor above.
    pub fn from_number(asset: HeldAsset, n: Number, mode: Rounding) -> Result<Amount, StError> {
        let negative = n.negative && !n.is_zero();
        let working = n.abs();
        if !matches!(asset, HeldAsset::Iou { .. }) {
            let value = working.to_drops(mode).map_err(|_| StError::Overflow)?;
            return Amount::new(asset, value.unsigned_abs() as u128, 0, negative, mode);
        }
        let (_, mantissa, exponent) =
            working.normalize_to_range(IOU_MIN_MANTISSA, IOU_MAX_MANTISSA, mode).map_err(|_| StError::Overflow)?;
        Amount::new(asset, mantissa as u128, exponent, negative, mode)
    }

    /// `STAmount::operator Number()` (STAmount.h:541-551): the whole units
    /// of an XRP or MPT amount, or an IOU's mantissa and exponent, as a
    /// Number at the thread's scale — exact at Large330.
    pub fn number(&self) -> Number {
        if self.integral() {
            Number::from_parts(self.negative, self.mantissa as u128, 0, Rounding::ToNearest).unwrap_or(Number::ZERO)
        } else {
            Number::from_iou(self.negative, self.mantissa, self.exponent)
        }
    }

    /// `operator-`: the sign flipped, zero left alone.
    pub fn negated(self) -> Amount {
        if self.is_zero() {
            self
        } else {
            Amount { negative: !self.negative, ..self }
        }
    }

    /// `operator+(STAmount, STAmount)` for two IOU amounts of one asset
    /// (STAmount.cpp:372-394): a zero operand returns the other; else
    /// `IOUAmount{Number{a} + Number{b}}` (IOUAmount.cpp:63-78) under `mode`.
    fn iou_add(self, other: Amount, mode: Rounding) -> Result<Amount, StError> {
        if other.is_zero() {
            return Ok(self);
        }
        if self.is_zero() {
            return Ok(Amount { asset: self.asset, ..other });
        }
        let sum = self.number().add(other.number(), mode).map_err(|_| StError::Overflow)?;
        let (negative, mantissa, exponent) = sum.iou_parts(mode).map_err(|_| StError::Overflow)?;
        if exponent > IOU_MAX_OFFSET {
            return Err(StError::Overflow);
        }
        if mantissa == 0 || exponent < IOU_MIN_OFFSET {
            return Ok(Amount::zero(self.asset));
        }
        Ok(Amount { asset: self.asset, negative, mantissa, exponent })
    }

    /// An amount as transaction or ledger JSON spells it: a drops string, an
    /// `{"currency", "issuer", "value"}` object or an
    /// `{"mpt_issuance_id", "value"}` object. None for anything else or a
    /// value out of the asset's range.
    pub fn from_json(v: &Value) -> Option<Amount> {
        if let Some((id, value)) = crate::tx::mpt::parse_mpt_amount(v) {
            return Amount::new(HeldAsset::Mpt(id), value as u128, 0, false, Rounding::ToNearest).ok();
        }
        let (negative, magnitude) = crate::tx::offer::signed_value(v);
        match v {
            Value::String(_) => Amount::new(HeldAsset::Xrp, magnitude.0, magnitude.1, negative, Rounding::ToNearest).ok(),
            Value::Object(_) => {
                let asset = crate::tx::mpt::parse_asset(v).filter(|a| matches!(a, HeldAsset::Iou { .. }))?;
                Amount::new(asset, magnitude.0, magnitude.1, negative, Rounding::ToNearest).ok()
            }
            _ => None,
        }
    }

    /// The amount in the engine's JSON dialect (hex accounts, 40-hex
    /// currency; `STAmount::getText` for an IOU's value).
    pub fn to_json(&self) -> Value {
        let sign = if self.negative && !self.is_zero() { "-" } else { "" };
        match self.asset {
            HeldAsset::Xrp => Value::String(format!("{sign}{}", self.mantissa)),
            HeldAsset::Mpt(id) => serde_json::json!({
                "mpt_issuance_id": hex::encode_upper(id),
                "value": format!("{sign}{}", self.mantissa),
            }),
            HeldAsset::Iou { currency, issuer } => serde_json::json!({
                "currency": hex::encode_upper(currency),
                "issuer": hex::encode(issuer),
                "value": crate::flow::amounts::IouAmount { negative: self.negative, mantissa: self.mantissa, exponent: self.exponent }
                    .to_decimal_string(),
            }),
        }
    }
}

/// An `Asset` (STIssue) in the engine's JSON dialect — what `sfAsset` holds.
pub fn asset_to_json(asset: &HeldAsset) -> Value {
    match asset {
        HeldAsset::Xrp => serde_json::json!({"currency": "XRP"}),
        HeldAsset::Mpt(id) => serde_json::json!({"mpt_issuance_id": hex::encode_upper(id)}),
        HeldAsset::Iou { currency, issuer } => {
            serde_json::json!({"currency": hex::encode_upper(currency), "issuer": hex::encode(issuer)})
        }
    }
}

// ---------------------------------------------------------------------------
// Rounding to an asset
// ---------------------------------------------------------------------------

/// `roundToAsset(asset, value)` in place (STAmount.h:733-737):
/// `value = STAmount{asset, value}` — whole units for XRP and MPT, sixteen
/// significant digits (and the IOU exponent clamp) for an IOU, under `mode`,
/// rippled's thread rounding mode at the call. This is what
/// `STNumber::associateAsset` applies.
pub fn round_number_to_asset(asset: HeldAsset, value: Number, mode: Rounding) -> Result<Number, StError> {
    Ok(Amount::from_number(asset, value, mode)?.number())
}

/// `roundToScale(value, scale, rounding)` (STAmount.cpp:1404-1424): an IOU
/// amount with digits below `10^scale` loses them under `rounding` — by
/// adding and then subtracting `1e15 × 10^scale` of the value's sign, whose
/// sixteen digits end at `10^scale`. Unchanged when the amount is integral,
/// zero, or already has an exponent of `scale` or more.
pub fn round_to_scale(value: Amount, scale: i32, rounding: Rounding) -> Result<Amount, StError> {
    if value.integral() || value.is_zero() || value.exponent >= scale {
        return Ok(value);
    }
    // Built before the rounding guard is installed; the value is exact.
    let reference = Amount::new(value.asset, IOU_MIN_MANTISSA as u128, scale, value.negative, Rounding::ToNearest)?;
    value.iou_add(reference, rounding)?.iou_add(reference.negated(), rounding)
}

/// `roundToAsset(asset, value, scale, rounding)` (STAmount.h:754-767):
/// `STAmount{asset, value}` under `rounding`; an integral asset is done, an
/// IOU is then rounded to `scale` under the same mode.
pub fn round_to_asset(asset: HeldAsset, value: Number, scale: i32, rounding: Rounding) -> Result<Number, StError> {
    let amount = Amount::from_number(asset, value, rounding)?;
    if amount.integral() {
        return Ok(amount.number());
    }
    Ok(round_to_scale(amount, scale, rounding)?.number())
}

/// `scale(number, asset)` (STAmount.h:793-796): the exponent of
/// `STAmount{asset, number}` — 0 for XRP and MPT, the sixteen-digit exponent
/// for an IOU — converted under `mode`.
pub fn scale(number: Number, asset: HeldAsset, mode: Rounding) -> Result<i32, StError> {
    Ok(Amount::from_number(asset, number, mode)?.exponent)
}

/// `associateAsset(sle, asset)` (STTakesAsset.cpp:13-50): every present
/// field of the object flagged `kSmdNeedsAsset` is rounded to the asset's
/// precision (`STNumber::associateAsset`, STNumber.cpp:55-66) under `mode`,
/// and a SoeDefault field that the rounding brought to zero is removed
/// (:42-46). rippled's transactors call it on each object they changed, near
/// the end of doApply, with the thread's rounding mode as it then stands
/// (ToNearest unless the transactor left a guard in force).
///
/// An object whose type has no template here keeps a field rounded to zero.
/// The error is rippled's STAmount throw (an amount out of the asset's
/// range).
pub fn associate_asset(obj: &mut Value, asset: HeldAsset, mode: Rounding) -> Result<(), StError> {
    let format = obj.get("LedgerEntryType").and_then(|t| t.as_str()).and_then(ledger_format);
    let Some(map) = obj.as_object_mut() else { return Ok(()) };
    let fields: Vec<String> = map
        .keys()
        .filter(|name| field_meta::by_name(name).is_some_and(|f| f.type_code == 9 && f.meta & SMD_NEEDS_ASSET != 0))
        .cloned()
        .collect();
    for field in fields {
        let Some(value) = map.get(&field).and_then(number_from_json) else { continue };
        let rounded = round_number_to_asset(asset, value, mode)?;
        let default_style = format.and_then(|f| f.style(&field)) == Some(Soe::Default);
        if rounded.is_zero() && default_style {
            map.remove(&field);
        } else {
            map.insert(field, Value::String(number_to_string(&rounded)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::keylet::vault_devnet_fixture::object;
    use crate::tx::number::{MantissaScale, ScaleGuard};
    use serde_json::json;

    const USD: HeldAsset = HeldAsset::Iou { currency: [0x55; 20], issuer: [0x21; 20] };
    const MPT: HeldAsset = HeldAsset::Mpt([0x07; 24]);

    fn large() -> ScaleGuard {
        ScaleGuard::new(MantissaScale::Large330)
    }

    fn n(m: i64, e: i32) -> Number {
        Number::new(m, e, Rounding::ToNearest).unwrap()
    }

    /// Number.cpp:1413-1505 at Large330 (log 18): scientific outside
    /// [-28, -8], decimal inside and at exponent 0. The strings on the left
    /// are the ones the SDK codec decodes the same wire values to
    /// (`canon::…::the_codec_round_trips_stnumber_byte_exactly`) and the
    /// devnet objects carry.
    #[test]
    fn a_number_prints_as_rippled_prints_it() {
        let _scale = large();
        for (text, mantissa, exponent) in [
            ("0", 0i64, 0),
            ("1", 1, 0),
            ("10000", 10_000, 0),
            ("-25.5", -255, -1),
            ("10000.15696446053", 1_000_015_696_446_053, -11),
            ("25.01593752207985077", 2_501_593_752_207_985_077, -17),
            ("9500000000000000000", 95, 17),
            ("9223372036854775807", i64::MAX, 0),
            ("1e-30", 1, -30),
            ("0.0000000001", 1, -10),
            ("1e-11", 1, -11),
            ("1000000000", 1, 9),
            // 1e11 is 1e18 × 10^-7: the exponent is above -8, so scientific.
            ("1e11", 1, 11),
            // 12345678901.2 is 1.23…e18 × 10^-8: still inside the window.
            ("12345678901.2", 123_456_789_012, -1),
            ("-5e-40", -5, -40),
        ] {
            let number = n(mantissa, exponent);
            assert_eq!(number_to_string(&number), text, "{mantissa}e{exponent}");
            assert_eq!(number_from_str(text), Some(number), "{text} parses back");
        }
        // The real devnet values go string → Number → string unchanged.
        for (name, field) in [
            ("vault_usd_closed_ended", "AssetsTotal"),
            ("vault_usd_closed_ended", "AssetsAvailable"),
            ("vault_xrp", "AssetsTotal"),
            ("loan_broker_usd", "DebtTotal"),
            ("loan_usd", "PeriodicPayment"),
            ("loan_usd", "PrincipalOutstanding"),
            ("loan_usd", "TotalValueOutstanding"),
        ] {
            let (node, _) = object(name);
            let text = node[field].as_str().unwrap();
            assert_eq!(number_to_string(&number_from_str(text).unwrap()), text, "{name}.{field}");
        }
    }

    /// At the Small scale (log 15) the decimal window is [-25, -5].
    #[test]
    fn the_decimal_window_follows_the_scale() {
        assert_eq!(number_to_string(&n(1, 0)), "1");
        assert_eq!(number_to_string(&n(15, -1)), "1.5");
        // 1e8 is 1e15 × 10^-7: inside [-25, -5].
        assert_eq!(number_to_string(&n(1, 8)), "100000000");
        // 1e11 is 1e15 × 10^-4: outside.
        assert_eq!(number_to_string(&n(1, 11)), "1e11");
        let _scale = large();
        assert_eq!(number_to_string(&n(1, 8)), "100000000");
    }

    /// STNumber.cpp:160-299: the grammar, and the refusal of a value the
    /// scale would round.
    #[test]
    fn a_string_that_is_not_an_exact_number_is_refused() {
        let _scale = large();
        for bad in ["", "-", "abc", "01", "1.", ".5", "1e", "1e+", "1 ", "0x10", "1.5.2", "--1", "99999999999999999999"] {
            assert_eq!(number_from_str(bad), None, "{bad:?}");
        }
        for (good, mantissa, exponent) in [("+7", 7i64, 0), ("0.50", 5, -1), ("-0", 0, 0), ("0e5", 0, 0), ("12E-3", 12, -3), ("3e+2", 3, 2)] {
            assert_eq!(number_from_str(good), Some(n(mantissa, exponent)), "{good}");
        }
        // Twenty significant digits do not fit nineteen: refused, not rounded.
        assert_eq!(number_from_str("1.2345678901234567891"), None);
        // 9223372036854775808 (2^63) would be held as …5810: refused.
        assert_eq!(number_from_str("9223372036854775808"), None);
        assert_eq!(number_from_str("9223372036854775810"), Some(n(922_337_203_685_477_581, 1)));
        assert_eq!(number_from_json(&json!(42)), Some(n(42, 0)));
        assert_eq!(number_from_json(&json!(-3)), Some(n(-3, 0)));
        assert_eq!(number_from_json(&json!(1.5)), None);
        assert_eq!(number_from_json(&json!(null)), None);
    }

    /// A SoeDefault field at zero is absent; a required one is written "0".
    #[test]
    fn number_fields_follow_their_template_style() {
        let _scale = large();
        let mut vault = json!({"LedgerEntryType": "Vault"});
        assert_eq!(number_field(&vault, "AssetsTotal"), Number::ZERO);
        assert_eq!(opt_number_field(&vault, "AssetsTotal"), None);
        set_number_field(&mut vault, "AssetsTotal", n(125, -1), Soe::Default);
        assert_eq!(vault["AssetsTotal"], json!("12.5"));
        assert_eq!(number_field(&vault, "AssetsTotal"), n(125, -1));
        set_number_field(&mut vault, "AssetsTotal", Number::ZERO, Soe::Default);
        assert!(vault.get("AssetsTotal").is_none());
        set_number_field(&mut vault, "PeriodicPayment", Number::ZERO, Soe::Required);
        assert_eq!(vault["PeriodicPayment"], json!("0"));
    }

    /// STAmount.cpp:821-875 and STAmount.h:567-583: an integral asset takes
    /// the Number as a whole number under the mode, an IOU its sixteen
    /// digits; the ranges throw.
    #[test]
    fn an_amount_from_a_number_is_canonical_for_its_asset() {
        let _scale = large();
        let from = |asset, number, mode| Amount::from_number(asset, number, mode).map(|a| (a.negative, a.mantissa, a.exponent));
        // 12.5 units: to nearest is even (12), up 13, down 12; −12.5 rounds by magnitude.
        assert_eq!(from(HeldAsset::Xrp, n(125, -1), Rounding::ToNearest), Ok((false, 12, 0)));
        assert_eq!(from(MPT, n(125, -1), Rounding::Upward), Ok((false, 13, 0)));
        assert_eq!(from(MPT, n(125, -1), Rounding::Downward), Ok((false, 12, 0)));
        assert_eq!(from(MPT, n(-125, -1), Rounding::Upward), Ok((true, 13, 0)));
        assert_eq!(from(MPT, n(4, -1), Rounding::ToNearest), Ok((false, 0, 0)));
        // Nineteen digits to an IOU's sixteen.
        assert_eq!(from(USD, n(1_234_567_890_123_456_789, -18), Rounding::ToNearest), Ok((false, 1_234_567_890_123_457, -15)));
        assert_eq!(from(USD, n(1_234_567_890_123_456_789, -18), Rounding::Downward), Ok((false, 1_234_567_890_123_456, -15)));
        assert_eq!(from(USD, n(-1_234_567_890_123_456_111, -18), Rounding::Upward), Ok((true, 1_234_567_890_123_457, -15)));
        // The ranges: 1e17 drops and 2^63 − 1 MPT units are the last; an IOU below 1e-81 is zero.
        assert_eq!(from(HeldAsset::Xrp, n(1, 17), Rounding::ToNearest), Ok((false, 100_000_000_000_000_000, 0)));
        assert_eq!(from(HeldAsset::Xrp, n(2, 17), Rounding::ToNearest), Err(StError::Overflow));
        assert_eq!(from(MPT, n(i64::MAX, 0), Rounding::ToNearest), Ok((false, i64::MAX as u64, 0)));
        assert_eq!(from(MPT, n(1, 19), Rounding::ToNearest), Err(StError::Overflow));
        assert_eq!(from(USD, n(1, -90), Rounding::ToNearest), Ok((false, 0, 0)));
        assert_eq!(from(USD, n(1, 96), Rounding::ToNearest), Err(StError::Overflow));
        assert_eq!(from(USD, n(1, 95), Rounding::ToNearest), Ok((false, 1_000_000_000_000_000, 80)));
        // And back: exact.
        let amount = Amount::from_number(USD, n(15, -1), Rounding::ToNearest).unwrap();
        assert_eq!(amount.number(), n(15, -1));
        assert_eq!(Amount::from_number(MPT, n(-7, 0), Rounding::ToNearest).unwrap().number(), n(-7, 0));
        assert!(amount.negated().negative && !Amount::zero(USD).negated().negative);
    }

    #[test]
    fn amounts_read_and_write_engine_json() {
        let _scale = large();
        let xrp = Amount::from_json(&json!("1500000")).unwrap();
        assert_eq!((xrp.asset, xrp.mantissa, xrp.exponent), (HeldAsset::Xrp, 1_500_000, 0));
        assert_eq!(xrp.to_json(), json!("1500000"));
        let mpt = Amount::from_json(&json!({"mpt_issuance_id": "07".repeat(24), "value": "42"})).unwrap();
        assert_eq!((mpt.asset, mpt.mantissa), (MPT, 42));
        assert_eq!(mpt.to_json(), json!({"mpt_issuance_id": "07".repeat(24), "value": "42"}));
        let usd_json = json!({"currency": hex::encode_upper([0x55u8; 20]), "issuer": hex::encode([0x21u8; 20]), "value": "-12.5"});
        let usd = Amount::from_json(&usd_json).unwrap();
        assert_eq!((usd.asset, usd.negative, usd.mantissa, usd.exponent), (USD, true, 1_250_000_000_000_000, -14));
        assert_eq!(usd.to_json(), usd_json);
        assert_eq!(Amount::from_json(&json!(5)), None);
        assert_eq!(asset_to_json(&HeldAsset::Xrp), json!({"currency": "XRP"}));
        assert_eq!(crate::tx::mpt::parse_asset(&asset_to_json(&USD)), Some(USD));
        assert_eq!(crate::tx::mpt::parse_asset(&asset_to_json(&MPT)), Some(MPT));
    }

    /// STAmount.cpp:1404-1424. 123.456789 to scale −2 (hundredths): 123.45
    /// down, 123.46 up and to nearest; a negative value rounds as its sum
    /// with a negative reference does. Integral, zero and coarse values are
    /// returned untouched.
    #[test]
    fn round_to_scale_drops_the_digits_below_the_scale() {
        let _scale = large();
        let usd = |m: i64, e: i32| Amount::from_number(USD, n(m, e), Rounding::ToNearest).unwrap();
        let value = usd(123_456_789, -6);
        assert_eq!(round_to_scale(value, -2, Rounding::Downward), Ok(usd(12_345, -2)));
        assert_eq!(round_to_scale(value, -2, Rounding::Upward), Ok(usd(12_346, -2)));
        assert_eq!(round_to_scale(value, -2, Rounding::ToNearest), Ok(usd(12_346, -2)));
        assert_eq!(round_to_scale(value, -2, Rounding::TowardsZero), Ok(usd(12_345, -2)));
        let negative = usd(-123_456_789, -6);
        assert_eq!(round_to_scale(negative, -2, Rounding::Downward), Ok(usd(-12_346, -2)));
        assert_eq!(round_to_scale(negative, -2, Rounding::Upward), Ok(usd(-12_345, -2)));
        // The value's own exponent is −13 (1.23456789e2 × 1e-15 … sixteen digits): a scale at or below it changes nothing.
        assert_eq!(round_to_scale(value, value.exponent, Rounding::Downward), Ok(value));
        assert_eq!(round_to_scale(value, -40, Rounding::Downward), Ok(value));
        assert_eq!(round_to_scale(Amount::zero(USD), -2, Rounding::Upward), Ok(Amount::zero(USD)));
        let drops = Amount::from_number(HeldAsset::Xrp, n(1_234_567, 0), Rounding::ToNearest).unwrap();
        assert_eq!(round_to_scale(drops, 3, Rounding::Downward), Ok(drops));
        // Less than one unit of the scale: down to zero, up to one unit.
        assert_eq!(round_to_scale(usd(4, -3), 0, Rounding::Downward), Ok(Amount::zero(USD)));
        assert_eq!(round_to_scale(usd(4, -3), 0, Rounding::Upward), Ok(usd(1, 0)));
    }

    /// STAmount.h:754-796.
    #[test]
    fn round_to_asset_and_scale_follow_the_asset() {
        let _scale = large();
        assert_eq!(round_to_asset(HeldAsset::Xrp, n(1_234_567, -3), -6, Rounding::Upward), Ok(n(1_235, 0)));
        assert_eq!(round_to_asset(MPT, n(1_234_567, -3), -6, Rounding::Downward), Ok(n(1_234, 0)));
        assert_eq!(round_to_asset(USD, n(123_456_789, -6), -2, Rounding::Downward), Ok(n(12_345, -2)));
        assert_eq!(round_to_asset(USD, n(123_456_789, -6), -2, Rounding::Upward), Ok(n(12_346, -2)));
        // Nineteen digits, a scale finer than sixteen digits reach: the sixteen-digit rounding alone.
        assert_eq!(
            round_to_asset(USD, n(1_234_567_890_123_456_789, -18), -30, Rounding::Downward),
            Ok(n(1_234_567_890_123_456, -15))
        );
        assert_eq!(scale(n(1_234_567, -3), HeldAsset::Xrp, Rounding::ToNearest), Ok(0));
        assert_eq!(scale(n(1_234_567, -3), MPT, Rounding::ToNearest), Ok(0));
        // 10000.15696446053 as an IOU is 1000015696446053e-11.
        assert_eq!(scale(n(1_000_015_696_446_053, -11), USD, Rounding::ToNearest), Ok(-11));
        assert_eq!(scale(n(1, 0), USD, Rounding::ToNearest), Ok(-15));
        assert_eq!(scale(Number::ZERO, USD, Rounding::ToNearest), Ok(0));
        assert_eq!(round_number_to_asset(USD, n(1_234_567_890_123_456_789, -18), Rounding::ToNearest), Ok(n(1_234_567_890_123_457, -15)));
    }

    /// STTakesAsset.cpp:13-50: the needs-asset fields are rounded, the
    /// others are not, and a SoeDefault field rounded to zero goes.
    #[test]
    fn associate_asset_rounds_the_needs_asset_fields() {
        let _scale = large();
        // A vault of XRP: whole drops.
        let mut vault = json!({
            "LedgerEntryType": "Vault", "Flags": 0,
            "AssetsTotal": "1000.5", "AssetsAvailable": "999.4", "LossUnrealized": "0.4", "AssetsMaximum": "2500",
        });
        associate_asset(&mut vault, HeldAsset::Xrp, Rounding::ToNearest).unwrap();
        assert_eq!(vault, json!({
            "LedgerEntryType": "Vault", "Flags": 0,
            "AssetsTotal": "1000", "AssetsAvailable": "999", "AssetsMaximum": "2500",
        }));
        // A loan of an IOU: the three outstanding amounts go to sixteen
        // digits; the fees and the periodic payment carry no asset flag
        // (sfields.macro:224-231) and keep all nineteen.
        let mut loan = json!({
            "LedgerEntryType": "Loan", "Flags": 0,
            "PeriodicPayment": "25.01593752207985077",
            "LoanServiceFee": "0.1234567890123456789",
            "PrincipalOutstanding": "4875.077276850131234",
            "TotalValueOutstanding": "4878.107816805585555",
            "ManagementFeeOutstanding": "1e-90",
        });
        associate_asset(&mut loan, USD, Rounding::ToNearest).unwrap();
        assert_eq!(loan, json!({
            "LedgerEntryType": "Loan", "Flags": 0,
            "PeriodicPayment": "25.01593752207985077",
            "LoanServiceFee": "0.1234567890123456789",
            "PrincipalOutstanding": "4875.077276850131",
            "TotalValueOutstanding": "4878.107816805586",
        }));
        let mut down = json!({"LedgerEntryType": "LoanBroker", "DebtTotal": "4878.107816805585555", "CoverAvailable": "7"});
        associate_asset(&mut down, USD, Rounding::Downward).unwrap();
        assert_eq!(down["DebtTotal"], json!("4878.107816805585"));
        assert_eq!(down["CoverAvailable"], json!("7"));
        // An amount out of the asset's range is rippled's throw.
        let mut over = json!({"LedgerEntryType": "Vault", "AssetsTotal": "2e17"});
        assert_eq!(associate_asset(&mut over, HeldAsset::Xrp, Rounding::ToNearest), Err(StError::Overflow));
        // The real devnet objects are already rounded: the pass changes nothing.
        for (name, asset) in [("vault_usd_closed_ended", USD), ("loan_broker_usd", USD), ("loan_usd", USD), ("vault_xrp", HeldAsset::Xrp)] {
            let (node, _) = object(name);
            let mut after = node.clone();
            associate_asset(&mut after, asset, Rounding::ToNearest).unwrap();
            assert_eq!(after, node, "{name}");
        }
    }
}
