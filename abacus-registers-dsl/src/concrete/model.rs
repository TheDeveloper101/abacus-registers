use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::Span;
use syn::{
    parse::{Parse, ParseStream},
    Error, LitInt, Result, Token,
};

/// Exact nonnegative rational ticks. Arithmetic overflow is a diagnostic, never wrapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cost(pub u128, pub u128);
impl Cost {
    pub const ZERO: Self = Self(0, 1);
    pub fn new(n: u128, d: u128) -> Result<Self> {
        if d == 0 {
            return Err(error("cost denominator must be nonzero"));
        }
        let (mut a, mut b) = (n, d);
        while b != 0 {
            (a, b) = (b, a % b);
        }
        Ok(Self(n / a, d / a))
    }
    /// Round the exact bound upward to a single whole-tick number for reporting.
    pub fn reported(self) -> u128 {
        self.0 / self.1 + u128::from(self.0 % self.1 != 0)
    }
    pub fn add(self, rhs: Self) -> Result<Self> {
        let n = self
            .0
            .checked_mul(rhs.1)
            .and_then(|a| rhs.0.checked_mul(self.1).and_then(|b| a.checked_add(b)));
        let d = self.1.checked_mul(rhs.1);
        Self::new(n.ok_or_else(overflow)?, d.ok_or_else(overflow)?)
    }
    pub fn max(self, rhs: Self) -> Result<Self> {
        let a = self.0.checked_mul(rhs.1).ok_or_else(overflow)?;
        let b = rhs.0.checked_mul(self.1).ok_or_else(overflow)?;
        Ok(if a >= b { self } else { rhs })
    }
}
fn overflow() -> Error {
    error("cost arithmetic overflow")
}
pub fn error(message: impl std::fmt::Display) -> Error {
    Error::new(Span::call_site(), message)
}
impl Parse for Cost {
    fn parse(input: ParseStream) -> Result<Self> {
        let n: LitInt = input.parse()?;
        let d = if input.peek(Token![/]) {
            input.parse::<Token![/]>()?;
            input.parse::<LitInt>()?.base10_parse()?
        } else {
            1
        };
        Self::new(n.base10_parse()?, d)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Int,
    Bool,
    Unit,
}
#[derive(Clone)]
pub struct Instance {
    pub model: String,
    pub states: BTreeSet<String>,
}
#[derive(Clone)]
pub struct Operation {
    pub target: String,
    pub cost: Option<Cost>,
    pub result: Type,
    pub arity: usize,
}
#[derive(Clone)]
pub struct Peripheral {
    pub background: BTreeMap<String, Option<Cost>>,
    pub transient: BTreeSet<String>,
    pub operations: BTreeMap<(String, String), Operation>,
    pub register_type: String,
    pub enum_type: String,
    pub state_types: BTreeMap<String, String>,
}
