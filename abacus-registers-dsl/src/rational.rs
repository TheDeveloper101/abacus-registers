use std::{
    cmp::Ordering,
    fmt,
    ops::{Add, AddAssign, Sub},
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Rational {
    numerator: u128,
    denominator: u128,
}

impl Rational {
    pub(crate) const ONE: Self = Self {
        numerator: 1,
        denominator: 1,
    };

    pub(crate) fn new(numerator: u128, denominator: u128) -> Option<Self> {
        if numerator == 0 || denominator == 0 {
            return None;
        }
        let divisor = gcd(numerator, denominator);
        Some(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    pub(crate) fn scale(self, value: usize) -> Self {
        if value == 0 {
            return Self {
                numerator: 0,
                denominator: 1,
            };
        }
        let divisor = gcd(value as u128, self.denominator);
        Self {
            numerator: self.numerator * (value as u128 / divisor),
            denominator: self.denominator / divisor,
        }
    }

    pub(crate) fn add(self, other: Self) -> Self {
        if self.numerator == 0 {
            return other;
        }
        if other.numerator == 0 {
            return self;
        }
        let divisor = gcd(self.denominator, other.denominator);
        let denominator = self.denominator / divisor * other.denominator;
        let numerator = self.numerator * (other.denominator / divisor)
            + other.numerator * (self.denominator / divisor);
        let reduction = gcd(numerator, denominator);
        Self {
            numerator: numerator / reduction,
            denominator: denominator / reduction,
        }
    }

    pub(crate) fn multiply_usize(self, factor: usize) -> Self {
        self.scale(factor)
    }

    pub(crate) fn as_usize(self) -> Option<usize> {
        (self.denominator == 1).then(|| self.numerator as usize)
    }
}

impl Ord for Rational {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.numerator * other.denominator).cmp(&(other.numerator * self.denominator))
    }
}
impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl fmt::Display for Rational {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.denominator == 1 {
            write!(formatter, "{}", self.numerator)
        } else {
            write!(formatter, "{}/{}", self.numerator, self.denominator)
        }
    }
}
impl Add for Rational {
    type Output = Self;
    fn add(self, other: Self) -> Self {
        Rational::add(self, other)
    }
}
impl AddAssign for Rational {
    fn add_assign(&mut self, other: Self) {
        *self = self.add(other);
    }
}
impl Sub for Rational {
    type Output = Self;
    fn sub(self, other: Self) -> Self {
        let denominator = self.denominator * other.denominator;
        let left = self.numerator * other.denominator;
        let right = other.numerator * self.denominator;
        debug_assert!(left >= right);
        let numerator = left - right;
        if numerator == 0 {
            return Self {
                numerator: 0,
                denominator: 1,
            };
        }
        let reduction = gcd(numerator, denominator);
        Self {
            numerator: numerator / reduction,
            denominator: denominator / reduction,
        }
    }
}

const fn gcd(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}
