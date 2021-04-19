// Copyright Materialize, Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License in the LICENSE file at the
// root of this repository, or online at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::Ordering;
use std::convert::{TryFrom, TryInto};
use std::ffi::{CStr, CString};
use std::fmt;
use std::iter::{Product, Sum};
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::{Add, AddAssign, Mul, MulAssign, Neg};
use std::str::FromStr;

use libc::c_char;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeStruct, Serializer};
use serde::Deserialize as AutoDeserialize;

use crate::context::{Class, Context, Status};
use crate::decimal128::Decimal128;
use crate::decimal32::Decimal32;
use crate::decimal64::Decimal64;
use crate::error::{InvalidExponentError, InvalidPrecisionError, ParseDecimalError};

fn validate_n(n: usize) {
    // TODO(benesch): check this at compile time, when that becomes possible.
    if n < 12 || n > 999_999_999 {
        panic!("Decimal<N>:: N is not in the range [12, 999999999]");
    }
}

/// An arbitrary-precision decimal number.
///
/// The maximum number of digits that can be stored in the number is specified
/// by `N * 3`. For example, a value of type `Decimal<3>` has space for nine
/// decimal digits. This somewhat odd design is due to limitations of constant
/// generic parameters in Rust. The intention is to someday make `N` correspond
/// directly to the number of digits of precision.
///
/// `N` must be at least 12 and no greater than 999,999,999, though typically
/// the stack size implies a smaller maximum for `N`. Due to limitations with
/// constant generics it is not yet possible to enforce these restrictions
/// at compile time, so they are checked at runtime.
#[cfg_attr(docsrs, doc(cfg(feature = "arbitrary-precision")))]
#[repr(C)]
#[derive(Clone, Copy, Hash)]
pub struct Decimal<const N: usize> {
    digits: u32,
    exponent: i32,
    bits: u8,
    lsu: [u16; N],
}

impl<const N: usize> Decimal<N> {
    pub(crate) fn as_ptr(&self) -> *const decnumber_sys::decNumber {
        self as *const Decimal<N> as *const decnumber_sys::decNumber
    }

    pub(crate) fn as_mut_ptr(&mut self) -> *mut decnumber_sys::decNumber {
        self as *mut Decimal<N> as *mut decnumber_sys::decNumber
    }

    /// Constructs a decimal number representing an infinite value.
    pub fn infinity() -> Decimal<N> {
        let mut d = Decimal::default();
        d.bits = decnumber_sys::DECINF;
        d
    }

    /// Constructs a decimal number representing a non-signaling NaN.
    pub fn nan() -> Decimal<N> {
        let mut d = Decimal::default();
        d.bits = decnumber_sys::DECNAN;
        d
    }

    /// Constructs a decimal number with `N / 3` digits of precision
    /// representing the number 0.
    pub fn zero() -> Decimal<N> {
        Decimal::default()
    }

    // Constructs a decimal number equal to 2^32. We use this value internally
    // to create decimals from primitive integers with more than 32 bits.
    fn two_pow_32() -> Decimal<N> {
        let mut d = Decimal::default();
        d.digits = 10;
        d.lsu[0..4].copy_from_slice(&[296, 967, 294, 4]);
        d
    }

    /// Computes the number of significant digits in the number.
    ///
    /// If the number is zero or infinite, returns 1. If the number is a NaN,
    /// returns the number of digits in the payload.
    pub fn digits(&self) -> u32 {
        self.digits
    }

    /// Returns the individual digits of the coefficient in 8-bit, unpacked
    /// [binary-coded decimal][bcd] format.
    ///
    /// [bcd]: https://en.wikipedia.org/wiki/Binary-coded_decimal
    pub fn coefficient_digits(&self) -> Vec<u8> {
        let mut buf = vec![0; usize::try_from(self.digits()).unwrap()];
        unsafe {
            decnumber_sys::decNumberGetBCD(self.as_ptr(), buf.as_mut_ptr() as *mut u8);
        };
        buf
    }

    /// Computes the exponent of the number.
    pub fn exponent(&self) -> i32 {
        self.exponent
    }

    /// Computes the number of digits necessary in standard notation to
    /// represent `self`, not including any leading zeroes.
    pub fn precision(&self) -> u64 {
        if self.exponent >= 0 {
            // More zeroes than digits, so dominates precision
            u64::from(self.digits) + u64::try_from(self.exponent.abs()).unwrap()
        } else if self.exponent.abs() as usize > self.digits() as usize {
            // Negative exponent dominates digits, so is left-padded by zeroes
            u64::try_from(self.exponent.abs()).unwrap()
        } else {
            // Decimal point splices digits
            u64::from(self.digits)
        }
    }

    /// Reports whether the number is finite.
    ///
    /// A finite number is one that is neither infinite nor a NaN.
    pub fn is_finite(&self) -> bool {
        (self.bits & decnumber_sys::DECSPECIAL) == 0
    }

    /// Reports whether the number is positive or negative infinity.
    pub fn is_infinite(&self) -> bool {
        (self.bits & decnumber_sys::DECINF) != 0
    }

    /// Reports whether the number is a NaN.
    pub fn is_nan(&self) -> bool {
        (self.bits & (decnumber_sys::DECNAN | decnumber_sys::DECSNAN)) != 0
    }

    /// Reports whether the number is negative.
    ///
    /// A negative number is either negative zero, less than zero, or NaN
    /// with a sign of one. This corresponds to [`Decimal128::is_signed`], not
    /// [`Decimal128::is_negative`].
    pub fn is_negative(&self) -> bool {
        (self.bits & decnumber_sys::DECNEG) != 0
    }

    /// Reports whether the number is a quiet NaN.
    pub fn is_quiet_nan(&self) -> bool {
        (self.bits & decnumber_sys::DECNAN) != 0
    }

    /// Reports whether the number is a signaling NaN.
    pub fn is_signaling_nan(&self) -> bool {
        (self.bits & decnumber_sys::DECSNAN) != 0
    }

    /// Reports whether the number has a special value.
    ///
    /// A special value is either infinity or NaN. This is the inverse of
    /// [`Decimal::is_finite`].
    pub fn is_special(&self) -> bool {
        (self.bits & decnumber_sys::DECSPECIAL) != 0
    }

    /// Reports whether the number is positive or negative zero.
    pub fn is_zero(&self) -> bool {
        self.is_finite() && self.lsu[0] == 0 && self.digits == 1
    }

    /// Reports whether the quantum of the number matches the quantum of
    /// `rhs`.
    ///
    /// Quantums are considered to match if the numbers have the same exponent,
    /// are both NaNs, or both infinite.
    pub fn quantum_matches(&self, rhs: &Decimal<N>) -> bool {
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        let d = unsafe {
            decnumber_sys::decNumberSameQuantum(
                d.as_mut_ptr() as *mut decnumber_sys::decNumber,
                self.as_ptr(),
                rhs.as_ptr(),
            );
            d.assume_init()
        };
        if d.is_zero() {
            false
        } else {
            debug_assert!(!d.is_special());
            true
        }
    }

    /// Converts this decimal to a 32-bit decimal float.
    ///
    /// The result may be inexact. Use [`Context::<Decimal32>::from_decimal`]
    /// to observe exceptional conditions.
    pub fn to_decimal32(&self) -> Decimal32 {
        Context::<Decimal32>::default().from_decimal(self)
    }

    /// Converts this decimal to a 64-bit decimal float.
    ///
    /// The result may be inexact. Use [`Context::<Decimal64>::from_decimal`]
    /// to observe exceptional conditions.
    pub fn to_decimal64(&self) -> Decimal64 {
        Context::<Decimal64>::default().from_decimal(self)
    }

    /// Converts this decimal to a 128-bit decimal float.
    ///
    /// The result may be inexact. Use [`Context::<Decimal128>::from_decimal`]
    /// to observe exceptional conditions.
    pub fn to_decimal128(&self) -> Decimal128 {
        Context::<Decimal128>::default().from_decimal(self)
    }

    /// Returns the raw parts of this decimal.
    ///
    /// The meaning of these parts are unspecified and subject to change.
    pub fn to_raw_parts(&self) -> (u32, i32, u8, [u16; N]) {
        (self.digits, self.exponent, self.bits, self.lsu)
    }

    /// Returns a string of the number in standard notation, i.e. guaranteed to
    /// not be scientific notation.
    pub fn to_standard_notation_string(&self) -> String {
        to_standard_notation_string!(self)
    }
}

impl<const N: usize> Default for Decimal<N> {
    fn default() -> Decimal<N> {
        validate_n(N);
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        unsafe {
            decnumber_sys::decNumberZero(d.as_mut_ptr() as *mut decnumber_sys::decNumber);
            d.assume_init()
        }
    }
}

impl<const N: usize> PartialOrd for Decimal<N> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Context::<Decimal<N>>::default().partial_cmp(self, other)
    }
}

impl<const N: usize> PartialEq for Decimal<N> {
    fn eq(&self, other: &Self) -> bool {
        self.partial_cmp(other) == Some(Ordering::Equal)
    }
}

impl<const N: usize> fmt::Debug for Decimal<N> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl<const N: usize> fmt::Display for Decimal<N> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // String conversion may need up to `self.digits + 14` characters,
        // per the libdecnumber documentation.
        let mut buf = Vec::with_capacity(self.digits as usize + 14);
        let c_str = unsafe {
            if f.alternate() {
                decnumber_sys::decNumberToEngString(self.as_ptr(), buf.as_mut_ptr() as *mut c_char);
            } else {
                decnumber_sys::decNumberToString(self.as_ptr(), buf.as_mut_ptr() as *mut c_char);
            }
            CStr::from_ptr(buf.as_ptr() as *const c_char)
        };
        f.write_str(
            c_str
                .to_str()
                .expect("decNumberToString yields valid UTF-8"),
        )
    }
}

impl<const N: usize> FromStr for Decimal<N> {
    type Err = ParseDecimalError;

    fn from_str(s: &str) -> Result<Decimal<N>, ParseDecimalError> {
        Context::<Decimal<N>>::default().parse(s)
    }
}
impl<const N: usize> From<i32> for Decimal<N> {
    fn from(n: i32) -> Decimal<N> {
        validate_n(N);
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        unsafe {
            decnumber_sys::decNumberFromInt32(d.as_mut_ptr() as *mut decnumber_sys::decNumber, n);
            d.assume_init()
        }
    }
}

impl<const N: usize> From<u32> for Decimal<N> {
    fn from(n: u32) -> Decimal<N> {
        validate_n(N);
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        unsafe {
            decnumber_sys::decNumberFromUInt32(d.as_mut_ptr() as *mut decnumber_sys::decNumber, n);
            d.assume_init()
        }
    }
}

impl<const N: usize> From<i64> for Decimal<N> {
    fn from(n: i64) -> Decimal<N> {
        let mut cx = Context::<Decimal<N>>::default();
        let d = decnum_from_signed_int!(Decimal<N>, cx, n);
        debug_assert!(!cx.status().any());
        d
    }
}

impl<const N: usize> From<u64> for Decimal<N> {
    fn from(n: u64) -> Decimal<N> {
        let mut cx = Context::<Decimal<N>>::default();
        let d = decnum_from_unsigned_int!(Decimal<N>, cx, n);
        debug_assert!(!cx.status().any());
        d
    }
}

impl<const N: usize> From<i128> for Decimal<N> {
    fn from(n: i128) -> Decimal<N> {
        let mut cx = Context::<Decimal<N>>::default();
        let d = decnum_from_signed_int!(Decimal<N>, cx, n);
        debug_assert!(!cx.status().any());
        d
    }
}

impl<const N: usize> From<u128> for Decimal<N> {
    fn from(n: u128) -> Decimal<N> {
        let mut cx = Context::<Decimal<N>>::default();
        let d = decnum_from_unsigned_int!(Decimal<N>, cx, n);
        debug_assert!(!cx.status().any());
        d
    }
}

#[cfg(target_pointer_width = "32")]
impl<const N: usize> From<usize> for Decimal<N> {
    fn from(n: usize) -> Decimal<N> {
        Decimal::<N>::from(n as u32)
    }
}

#[cfg(target_pointer_width = "32")]
impl<const N: usize> From<isize> for Decimal<N> {
    fn from(n: isize) -> Decimal<N> {
        Decimal::<N>::from(n as i32)
    }
}

#[cfg(target_pointer_width = "64")]
impl<const N: usize> From<usize> for Decimal<N> {
    fn from(n: usize) -> Decimal<N> {
        Decimal::<N>::from(n as u64)
    }
}

#[cfg(target_pointer_width = "64")]
impl<const N: usize> From<isize> for Decimal<N> {
    fn from(n: isize) -> Decimal<N> {
        Decimal::<N>::from(n as i64)
    }
}

impl<const N: usize> From<Decimal32> for Decimal<N> {
    fn from(n: Decimal32) -> Decimal<N> {
        validate_n(N);
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        unsafe {
            decnumber_sys::decimal32ToNumber(
                &n.inner,
                d.as_mut_ptr() as *mut decnumber_sys::decNumber,
            );
            d.assume_init()
        }
    }
}

impl<const N: usize> From<Decimal64> for Decimal<N> {
    fn from(n: Decimal64) -> Decimal<N> {
        validate_n(N);
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        unsafe {
            decnumber_sys::decimal64ToNumber(
                &n.inner,
                d.as_mut_ptr() as *mut decnumber_sys::decNumber,
            );
            d.assume_init()
        }
    }
}

impl<const N: usize> From<Decimal128> for Decimal<N> {
    fn from(n: Decimal128) -> Decimal<N> {
        validate_n(N);
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        unsafe {
            decnumber_sys::decimal128ToNumber(
                &n.inner,
                d.as_mut_ptr() as *mut decnumber_sys::decNumber,
            );
            d.assume_init()
        }
    }
}

impl<const N: usize> Default for Context<Decimal<N>> {
    fn default() -> Context<Decimal<N>> {
        let mut ctx = MaybeUninit::<decnumber_sys::decContext>::uninit();
        let mut ctx = unsafe {
            decnumber_sys::decContextDefault(ctx.as_mut_ptr(), decnumber_sys::DEC_INIT_BASE);
            ctx.assume_init()
        };
        ctx.traps = 0;
        // TODO(benesch): this could be a static assertion or a where bound,
        // if either of those were supported.
        ctx.digits = i32::try_from(N * decnumber_sys::DECDPUN)
            .expect("decimal digit count does not fit into i32");
        Context {
            inner: ctx,
            _phantom: PhantomData,
        }
    }
}

impl<const N: usize> Neg for Decimal<N> {
    type Output = Decimal<N>;

    /// Note that this clones `self` to generate the negative value. For a
    /// non-allocating method, use `Context::<N>::neg`.
    fn neg(self) -> Decimal<N> {
        let mut n = self.clone();
        unsafe {
            decnumber_sys::decNumberCopyNegate(n.as_mut_ptr(), n.as_ptr());
        }
        n
    }
}

impl<const N: usize> Add<Decimal<N>> for Decimal<N> {
    type Output = Decimal<N>;

    fn add(self, rhs: Decimal<N>) -> Decimal<N> {
        let mut d = self.clone();
        Context::<Decimal<N>>::default().add(&mut d, &rhs);
        d
    }
}

impl<const N: usize> AddAssign<Decimal<N>> for Decimal<N> {
    fn add_assign(&mut self, rhs: Decimal<N>) {
        Context::<Decimal<N>>::default().add(self, &rhs);
    }
}

impl<const N: usize> Mul<Decimal<N>> for Decimal<N> {
    type Output = Decimal<N>;

    fn mul(self, rhs: Decimal<N>) -> Decimal<N> {
        let mut d = self.clone();
        Context::<Decimal<N>>::default().mul(&mut d, &rhs);
        d
    }
}

impl<const N: usize> MulAssign<Decimal<N>> for Decimal<N> {
    fn mul_assign(&mut self, rhs: Decimal<N>) {
        Context::<Decimal<N>>::default().mul(self, &rhs);
    }
}

/// This implementation of `Sum` creates a new `Context::<Decimal<N>>` on each
/// invocation, without providing a mechanism to add setting to the `Context` or
/// return its status. Instead, we recommend using `Context::<Decimal<N>>::sum`.
impl<const N: usize> Sum for Decimal<N> {
    fn sum<I>(iter: I) -> Self
    where
        I: Iterator<Item = Decimal<N>>,
    {
        iter.map(|x| x).sum()
    }
}

/// This implementation of `Sum` creates a new `Context::<Decimal<N>>` on each
/// invocation, without providing a mechanism to add setting to the `Context` or
/// return its status. Instead, we recommend using `Context::<Decimal<N>>::sum`.
impl<'a, const N: usize> Sum<&'a Decimal<N>> for Decimal<N> {
    fn sum<I>(iter: I) -> Self
    where
        I: Iterator<Item = &'a Decimal<N>>,
    {
        let mut cx = Context::<Decimal<N>>::default();
        cx.sum(iter)
    }
}

/// This implementation of `Sum` creates a new `Context::<Decimal<N>>` on each
/// invocation, without providing a mechanism to add setting to the `Context` or
/// return its status. Instead, we recommend using `Context::<Decimal<N>>::sum`.
impl<const N: usize> Product for Decimal<N> {
    fn product<I>(iter: I) -> Self
    where
        I: Iterator<Item = Decimal<N>>,
    {
        iter.map(|x| x).product()
    }
}

/// This implementation of `Sum` creates a new `Context::<Decimal<N>>` on each
/// invocation, without providing a mechanism to add setting to the `Context` or
/// return its status. Instead, we recommend using `Context::<Decimal<N>>::sum`.
impl<'a, const N: usize> Product<&'a Decimal<N>> for Decimal<N> {
    fn product<I>(iter: I) -> Self
    where
        I: Iterator<Item = &'a Decimal<N>>,
    {
        let mut cx = Context::<Decimal<N>>::default();
        cx.product(iter)
    }
}

impl<const N: usize> Serialize for Decimal<N> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("Decimal", 4)?;
        s.serialize_field("digits", &self.digits)?;
        s.serialize_field("exponent", &self.exponent)?;
        s.serialize_field("bits", &self.bits)?;
        s.serialize_field("lsu", &self.lsu[..])?;
        s.end()
    }
}

impl<'de, const N: usize> Deserialize<'de> for Decimal<N> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(AutoDeserialize, Debug)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Digits,
            Exponent,
            Bits,
            Lsu,
        }

        struct DecimalVisitor<const N: usize>;

        impl<'de, const N: usize> Visitor<'de> for DecimalVisitor<N> {
            type Value = Decimal<N>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("struct Decimal")
            }

            fn visit_seq<V>(self, mut seq: V) -> Result<Decimal<N>, V::Error>
            where
                V: SeqAccess<'de>,
            {
                let digits = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let exponent = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let bits = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let lsu: Vec<u16> = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(3, &self))?;

                let lsu_len = lsu.len();

                Ok(Decimal::<N> {
                    digits,
                    exponent,
                    bits,
                    lsu: match lsu.try_into() {
                        Ok(lsu) => lsu,
                        Err(_) => {
                            return Err(de::Error::invalid_value(
                                de::Unexpected::Other(&format!("&[u16] of length {}", lsu_len)),
                                &format!("&[u16] of length {}", N).as_str(),
                            ))
                        }
                    },
                })
            }

            fn visit_map<V>(self, mut map: V) -> Result<Decimal<N>, V::Error>
            where
                V: MapAccess<'de>,
            {
                let mut digits = None;
                let mut exponent = None;
                let mut bits = None;
                let mut lsu = None;
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Digits => {
                            if digits.is_some() {
                                return Err(de::Error::duplicate_field("digits"));
                            }
                            digits = Some(map.next_value()?);
                        }
                        Field::Exponent => {
                            if exponent.is_some() {
                                return Err(de::Error::duplicate_field("exponent"));
                            }
                            exponent = Some(map.next_value()?);
                        }
                        Field::Bits => {
                            if bits.is_some() {
                                return Err(de::Error::duplicate_field("exponent"));
                            }
                            bits = Some(map.next_value()?);
                        }
                        Field::Lsu => {
                            if lsu.is_some() {
                                return Err(de::Error::duplicate_field("exponent"));
                            }
                            lsu = Some(map.next_value()?);
                        }
                    }
                }
                let digits = digits.ok_or_else(|| de::Error::missing_field("digits"))?;
                let exponent = exponent.ok_or_else(|| de::Error::missing_field("exponent"))?;
                let bits = bits.ok_or_else(|| de::Error::missing_field("bits"))?;
                let lsu: Vec<u16> = lsu.ok_or_else(|| de::Error::missing_field("lsu"))?;
                let lsu_len = lsu.len();

                Ok(Decimal::<N> {
                    digits,
                    exponent,
                    bits,
                    lsu: match lsu.try_into() {
                        Ok(lsu) => lsu,
                        Err(_) => {
                            return Err(de::Error::invalid_value(
                                de::Unexpected::Other(&format!("&[u16] of length {}", lsu_len)),
                                &format!("&[u16] of length {}", N).as_str(),
                            ))
                        }
                    },
                })
            }
        }

        const FIELDS: &'static [&'static str] = &["digits", "exponent", "bits", "lsu"];
        deserializer.deserialize_struct("Decimal", FIELDS, DecimalVisitor)
    }
}

impl<const N: usize> Context<Decimal<N>> {
    /// Returns the context's precision.
    ///
    /// Operations that use this context will be rounded to this length if
    /// necessary.
    pub fn precision(&self) -> usize {
        usize::try_from(self.inner.digits).expect("context digit count does not fit into usize")
    }

    /// Sets the context's precision.
    ///
    /// The precision must be greater than one and no greater than `N * 3`.
    pub fn set_precision(&mut self, precision: usize) -> Result<(), InvalidPrecisionError> {
        if precision < 1 || precision > N * decnumber_sys::DECDPUN {
            return Err(InvalidPrecisionError);
        }
        self.inner.digits = i32::try_from(precision).map_err(|_| InvalidPrecisionError)?;
        Ok(())
    }

    /// Reports whether the context has exponent clamping enabled.
    ///
    /// See the `clamp` field in the documentation of libdecnumber's
    /// [decContext module] for details.
    ///
    /// [decContext module]: http://speleotrove.com/decimal/dncont.html
    pub fn clamp(&self) -> bool {
        self.inner.clamp != 0
    }

    /// Sets whether the context has exponent clamping enabled.
    pub fn set_clamp(&mut self, clamp: bool) {
        self.inner.clamp = u8::from(clamp)
    }

    /// Returns the context's maximum exponent.
    ///
    /// See the `emax` field in the documentation of libdecnumber's
    /// [decContext module] for details.
    ///
    /// [decContext module]: http://speleotrove.com/decimal/dncont.html
    pub fn max_exponent(&self) -> isize {
        isize::try_from(self.inner.emax).expect("context max exponent does not fit into isize")
    }

    /// Sets the context's maximum exponent.
    ///
    /// The maximum exponent must not be negative and no greater than
    /// 999,999,999.
    pub fn set_max_exponent(&mut self, e: isize) -> Result<(), InvalidExponentError> {
        if e < 0 || e > 999999999 {
            return Err(InvalidExponentError);
        }
        self.inner.emax = i32::try_from(e).map_err(|_| InvalidExponentError)?;
        Ok(())
    }

    /// Returns the context's minimum exponent.
    ///
    /// See the `emin` field in the documentation of libdecnumber's
    /// [decContext module] for details.
    ///
    /// [decContext module]: http://speleotrove.com/decimal/dncont.html
    pub fn min_exponent(&self) -> isize {
        isize::try_from(self.inner.emin).expect("context min exponent does not fit into isize")
    }

    /// Sets the context's minimum exponent.
    ///
    /// The minimum exponent must not be positive and no smaller than
    /// -999,999,999.
    pub fn set_min_exponent(&mut self, e: isize) -> Result<(), InvalidExponentError> {
        if e > 0 || e < -999999999 {
            return Err(InvalidExponentError);
        }
        self.inner.emin = i32::try_from(e).map_err(|_| InvalidExponentError)?;
        Ok(())
    }

    /// Parses a number from its string representation.
    pub fn parse<S>(&mut self, s: S) -> Result<Decimal<N>, ParseDecimalError>
    where
        S: Into<Vec<u8>>,
    {
        validate_n(N);
        let c_string = CString::new(s).map_err(|_| ParseDecimalError)?;
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        let d = unsafe {
            decnumber_sys::decNumberFromString(
                d.as_mut_ptr() as *mut decnumber_sys::decNumber,
                c_string.as_ptr(),
                &mut self.inner,
            );
            d.assume_init()
        };
        if (self.inner.status & decnumber_sys::DEC_Conversion_syntax) != 0 {
            Err(ParseDecimalError)
        } else {
            Ok(d)
        }
    }

    /// Classifies the number.
    pub fn class(&mut self, n: &Decimal<N>) -> Class {
        Class::from_c(unsafe { decnumber_sys::decNumberClass(n.as_ptr(), &mut self.inner) })
    }

    /// Computes the absolute value of `n`, storing the result in `n`.
    ///
    /// This has the same effect as [`Context::<Decimal<N>>::plus`] unless
    /// `n` is negative, in which case it has the same effect as
    /// [`Context::<Decimal<N>>::minus`].
    pub fn abs(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberAbs(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Adds `lhs` and `rhs`, storing the result in `lhs`.
    pub fn add(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberAdd(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Carries out the digitwise logical and of `lhs` and `rhs`, storing
    /// the result in `lhs`.
    pub fn and(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberAnd(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Divides `lhs` by `rhs`, storing the result in `lhs`.
    pub fn div(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberDivide(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Divides `lhs` by `rhs`, storing the integer part of the result in `lhs`.
    pub fn div_integer(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberDivideInteger(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Raises *e* to the power of `n`, storing the result in `n`.
    pub fn exp(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberExp(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Calculates the fused multiply-add `(x * y) + z` and stores the result
    /// in `x`.
    ///
    /// The multiplication is carried out first and is exact, so this operation
    /// only has the one, final rounding.
    pub fn fma(&mut self, x: &mut Decimal<N>, y: &Decimal<N>, z: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberFMA(
                x.as_mut_ptr(),
                x.as_ptr(),
                y.as_ptr(),
                z.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Constructs a number from an `i128`.
    ///
    /// Note that this function can return inexact results for numbers with more
    /// than `N` * 3 places of precision, e.g. where `N` is 12,
    /// `9_999_999_999_999_999_999_999_999_999_999_999_999i128`,
    /// `-9_999_999_999_999_999_999_999_999_999_999_999_999i128`, `i128::MAX`,
    /// `i128::MIN`, etc.
    ///
    /// However, some numbers more than `N` * 3 places of precision retain their
    /// exactness, e.g. `1_000_000_000_000_000_000_000_000_000_000_000_000i128`.
    ///
    /// ```
    /// const N: usize = 12;
    /// use dec::Decimal;
    /// let mut ctx = dec::Context::<Decimal::<N>>::default();
    /// let d = ctx.from_i128(i128::MAX);
    /// // Inexact result
    /// assert!(ctx.status().inexact());
    ///
    /// let mut ctx = dec::Context::<Decimal::<N>>::default();
    /// let d = ctx.from_i128(1_000_000_000_000_000_000_000_000_000_000_000_000i128);
    /// // Exact result
    /// assert!(!ctx.status().inexact());
    /// ```
    ///
    /// To avoid inexact results when converting from large `i64`, use
    /// [`crate::Decimal128`] instead.
    pub fn from_i128(&mut self, n: i128) -> Decimal<N> {
        decnum_from_signed_int!(Decimal<N>, self, n)
    }

    /// Constructs a number from an `u128`.
    ///
    /// Note that this function can return inexact results for numbers with more
    /// than `N` * 3 places of precision, e.g. where `N` is 12,
    /// `10_000_000_000_000_000_000_000_000_000_000_001u128` and `u128::MAX`.
    ///
    /// However, some numbers more than `N` * 3 places of precision retain their
    /// exactness,  e.g. `10_000_000_000_000_000_000_000_000_000_000_000u128`.
    ///
    /// ```
    /// const N: usize = 12;
    /// use dec::Decimal;
    /// let mut ctx = dec::Context::<Decimal::<N>>::default();
    /// let d = ctx.from_u128(u128::MAX);
    /// // Inexact result
    /// assert!(ctx.status().inexact());
    ///
    /// let mut ctx = dec::Context::<Decimal::<N>>::default();
    /// let d = ctx.from_u128(1_000_000_000_000_000_000_000_000_000_000_000_000u128);
    /// // Exact result
    /// assert!(!ctx.status().inexact());
    /// ```
    pub fn from_u128(&mut self, n: u128) -> Decimal<N> {
        decnum_from_unsigned_int!(Decimal<N>, self, n)
    }

    /// Computes the digitwise logical inversion of `n`, storing the result in
    /// `n`.
    pub fn invert(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberInvert(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Computes the natural logarithm of `n`, storing the result in `n`.
    pub fn ln(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberLn(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Computes the base-10 logarithm of `n`, storing the result in `n`.
    pub fn log10(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberLog10(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Computes the adjusted exponent of the number, according to IEEE 754
    /// rules.
    pub fn logb(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberLogB(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Places whichever of `lhs` and `rhs` is larger in `lhs`.
    ///
    /// The comparison is performed using the same rules as for
    /// [`total_cmp`](Context::<Decimal128>::total_cmp).
    pub fn max(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberMax(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Places whichever of `lhs` and `rhs` has the larger absolute value in
    /// `lhs`.
    pub fn max_abs(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberMaxMag(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Places whichever of `lhs` and `rhs` is smaller in `lhs`.
    ///
    /// The comparison is performed using the same rules as for
    /// [`total_cmp`](Context::<Decimal128>::total_cmp).
    pub fn min(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberMin(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Places whichever of `lhs` and `rhs` has the smaller absolute value in
    /// `lhs`.
    pub fn min_abs(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberMinMag(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Subtracts `n` from zero, storing the result in `n`. Note that unlike
    /// `neg`, exceptions and errors can occur.
    pub fn minus(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberMinus(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Multiples `lhs` by `rhs`, storing the result in `lhs`.
    pub fn mul(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberMultiply(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Subtracts `n` from zero, storing the result in `n`. Note that unlike
    /// `minus`, this is a "quiet" negation, i.e. no exception or error can
    /// occur.
    pub fn neg(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberCopyNegate(n.as_mut_ptr(), n.as_ptr());
        }
    }

    /// Computes the next number to `n` in the direction of negative infinity,
    /// storing the result in `n`.
    ///
    /// This operation is a generalization of the IEEE 754 *nextDown* operation.
    pub fn next_minus(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberNextMinus(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Computes the next number to `n` in the direction of positive infinity,
    /// storing the result in `n`.
    ///
    /// This operation is a generalization of the IEEE 754 *nextUp* operation.
    pub fn next_plus(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberNextPlus(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Computes the next number to `x` in the direction of `y`, storing the
    /// result in `x`.
    ///
    /// This operation is a generalization of the IEEE 754 *nextAfter*
    /// operation.
    pub fn next_toward(&mut self, x: &mut Decimal<N>, y: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberNextToward(
                x.as_mut_ptr(),
                x.as_ptr(),
                y.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Carries out the digitwise logical or of `lhs` and `rhs`, storing
    /// the result in `lhs`.
    pub fn or(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberOr(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Determines the ordering of `lhs` relative to `rhs`, using a partial
    /// order.
    ///
    /// If either `lhs` or `rhs` is a NaN, returns `None`. To force an ordering
    /// upon NaNs, use [`total_cmp`](Context::<Decimal<N>>::total_cmp).
    pub fn partial_cmp(&mut self, lhs: &Decimal<N>, rhs: &Decimal<N>) -> Option<Ordering> {
        validate_n(N);
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        let d = unsafe {
            decnumber_sys::decNumberCompare(
                d.as_mut_ptr() as *mut decnumber_sys::decNumber,
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
            d.assume_init()
        };
        if d.is_nan() {
            None
        } else if d.is_negative() {
            Some(Ordering::Less)
        } else if d.is_zero() {
            Some(Ordering::Equal)
        } else {
            debug_assert!(!d.is_special());
            Some(Ordering::Greater)
        }
    }

    /// Adds `n` to zero, storing the result in `n`.
    pub fn plus(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberPlus(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Raises `x` to the power of `y`, storing the result in `x`.
    pub fn pow(&mut self, x: &mut Decimal<N>, y: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberPower(x.as_mut_ptr(), x.as_ptr(), y.as_ptr(), &mut self.inner);
        }
    }

    /// Takes product of elements in `iter`.
    pub fn product<'a, I>(&mut self, iter: I) -> Decimal<N>
    where
        I: Iterator<Item = &'a Decimal<N>>,
    {
        iter.fold(Decimal::<N>::from(1), |mut product, d| {
            self.mul(&mut product, &d);
            product
        })
    }

    /// Rounds or pads `lhs` so that it has the same exponent as `rhs`, storing
    /// the result in `lhs`.
    pub fn quantize(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberQuantize(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Reduces `n`'s coefficient to its shortest possible form without
    /// changing the value of the result, storing the result in `n`.
    pub fn reduce(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberReduce(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Integer-divides `lhs` by `rhs`, storing the remainder in `lhs`.
    pub fn rem(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberRemainder(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Like [`rem`](Context::<Decimal<N>>::rem), but uses the IEEE 754
    /// rules for remainder operations.
    pub fn rem_near(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberRemainderNear(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Rescales `n` to have an exponent of `exp`.
    pub fn rescale(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberRescale(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Shifts the digits of `lhs` by `rhs`, storing the result in `lhs`.
    ///
    /// If `rhs` is positive, shifts to the left. If `rhs` is negative, shifts
    /// to the right. Any digits "shifted in" will be zero.
    ///
    /// `rhs` specifies the number of positions to shift, and must be a finite
    /// integer.
    pub fn shift(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberShift(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Rotates the digits of `lhs` by `rhs`, storing the result in `lhs`.
    ///
    /// If `rhs` is positive, rotates to the left. If `rhs` is negative, rotates
    /// to the right.
    ///
    /// `rhs` specifies the number of positions to rotate, and must be a finite
    /// integer.
    pub fn rotate(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberRotate(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Multiplies `x` by 10<sup>`y`</sup>, storing the result in `x`.
    pub fn scaleb(&mut self, x: &mut Decimal<N>, y: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberScaleB(x.as_mut_ptr(), x.as_ptr(), y.as_ptr(), &mut self.inner);
        }
    }

    /// Computes the square root of `n`, storing the result in `n`.
    pub fn sqrt(&mut self, n: &mut Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberSquareRoot(n.as_mut_ptr(), n.as_ptr(), &mut self.inner);
        }
    }

    /// Subtracts `rhs` from `lhs`, storing the result in `lhs`.
    pub fn sub(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberSubtract(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Sums all elements of `iter`.
    pub fn sum<'a, I>(&mut self, iter: I) -> Decimal<N>
    where
        I: Iterator<Item = &'a Decimal<N>>,
    {
        iter.fold(Decimal::<N>::zero(), |mut sum, d| {
            self.add(&mut sum, d);
            sum
        })
    }

    /// Determines the ordering of `lhs` relative to `rhs`, using the
    /// total order predicate defined in IEEE 754-2008.
    ///
    /// For a brief description of the ordering, consult [`f32::total_cmp`].
    pub fn total_cmp(&mut self, lhs: &Decimal<N>, rhs: &Decimal<N>) -> Ordering {
        validate_n(N);
        let mut d = MaybeUninit::<Decimal<N>>::uninit();
        let d = unsafe {
            decnumber_sys::decNumberCompareTotal(
                d.as_mut_ptr() as *mut decnumber_sys::decNumber,
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
            d.assume_init()
        };
        debug_assert!(!d.is_special());
        if d.is_negative() {
            Ordering::Less
        } else if d.is_zero() {
            Ordering::Equal
        } else {
            Ordering::Greater
        }
    }

    /// Carries out the digitwise logical xor of `lhs` and `rhs`, storing
    /// the result in `lhs`.
    pub fn xor(&mut self, lhs: &mut Decimal<N>, rhs: &Decimal<N>) {
        unsafe {
            decnumber_sys::decNumberXor(
                lhs.as_mut_ptr(),
                lhs.as_ptr(),
                rhs.as_ptr(),
                &mut self.inner,
            );
        }
    }

    /// Returns `m` cast as a `Decimal::<N>`.
    ///
    /// `Context` uses similar statuses to arithmetic to express under- and
    /// overflow for values whose total precisions exceeds this context's.
    pub fn to_width<const M: usize>(&mut self, mut m: Decimal<M>) -> Decimal<N> {
        // Check max_exponent, min_exponent over/underflow.
        let m_precision = m.precision();
        if m.exponent() >= 0 && m_precision as i64 > self.max_exponent() as i64 {
            // If the adjusted exponent for a result or conversion would be
            // larger than emax then an overflow results. (ed: in this library,
            // infinity can represent overflow.)
            // http://speleotrove.com/decimal/dncont.html
            let mut inexact_overflow_rounded = Status::default();
            inexact_overflow_rounded.set_inexact();
            inexact_overflow_rounded.set_overflow();
            inexact_overflow_rounded.set_rounded();
            self.add_status(inexact_overflow_rounded);

            let mut r = Decimal::<N>::infinity();
            if m.is_negative() {
                self.neg(&mut r);
            }
            return r;
        } else if m.exponent() < 0
            && m_precision > u64::try_from(self.min_exponent().abs()).unwrap()
        {
            // Underflow is rescaled, which does not necessarily return 0, which
            // matches arithmetic semantics.
            let mut cx_m = Context::<Decimal<M>>::default();
            cx_m.rescale(&mut m, &Decimal::<M>::from(self.min_exponent() as i32));
            assert!(cx_m.status().inexact());

            //  If the result is also inexact, an underflow results.
            //  http://speleotrove.com/decimal/dncont.html
            let mut inexact_rounded_subnormal_underflow = Status::default();
            inexact_rounded_subnormal_underflow.set_inexact();
            inexact_rounded_subnormal_underflow.set_rounded();
            inexact_rounded_subnormal_underflow.set_subnormal();
            inexact_rounded_subnormal_underflow.set_underflow();
            self.add_status(inexact_rounded_subnormal_underflow);
        }

        // If going to too-few digits, rescale to an equivalent number that fits
        if m.digits() as u64 > self.precision() as u64 {
            let mut cx_m = Context::<Decimal<M>>::default();
            // digits and precision can only differ as much as exponent, which
            // is i32.
            let precision_diff =
                i32::try_from(u64::from(m.digits()) - u64::try_from(self.precision()).unwrap())
                    .unwrap();
            let f = Decimal::<M>::from(m.exponent() + precision_diff);
            // Rescale adjusts digits and exponent.
            cx_m.rescale(&mut m, &f);

            // Set appropriate status.
            let mut inexact_rounded_status = Status::default();
            inexact_rounded_status.set_inexact();
            inexact_rounded_status.set_rounded();
            self.add_status(inexact_rounded_status);
        };

        let mut n = Decimal::<N>::default();

        let lsu_min_len = std::cmp::min(n.lsu.len(), m.lsu.len());

        n.lsu[..lsu_min_len].copy_from_slice(&m.lsu[..lsu_min_len]);
        n.bits = m.bits;
        // These are guaranteed to fit due to the potential rescaling done
        // above.
        n.digits = m.digits;
        n.exponent = m.exponent;

        n
    }
}
