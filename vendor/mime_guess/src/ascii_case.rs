// Original Puffinbox ASCII comparison; MIT OR Apache-2.0.
use std::cmp::Ordering;
use std::fmt;

#[derive(Copy, Clone)]
pub struct AsciiCase<'a>(&'a str);

impl<'a> AsciiCase<'a> {
    pub const fn new(value: &'a str) -> Self {
        Self(value)
    }
}

impl<'a> From<&'a str> for AsciiCase<'a> {
    fn from(value: &'a str) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for AsciiCase<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(output)
    }
}

impl Ord for AsciiCase<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .cmp(other.0.bytes().map(|byte| byte.to_ascii_lowercase()))
    }
}

impl PartialOrd for AsciiCase<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for AsciiCase<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for AsciiCase<'_> {}
