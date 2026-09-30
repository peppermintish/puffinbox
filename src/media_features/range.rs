//! Small, deliberately strict parser for single HTTP byte ranges.
//!
//! Media is served from seekable local files, so the server can honor one range
//! without buffering the file. Multipart ranges are rejected rather than
//! silently being served as a full response.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteRange {
    start: u64,
    end_inclusive: u64,
}

impl ByteRange {
    /// The first byte in the inclusive range.
    pub fn start(self) -> u64 {
        self.start
    }

    /// The last byte in the inclusive range.
    pub fn end_inclusive(self) -> u64 {
        self.end_inclusive
    }

    /// The range length. This is safe because ranges can only be constructed
    /// by the parser and are bounded by a `u64` representation length.
    pub fn len(self) -> u64 {
        self.end_inclusive - self.start + 1
    }

    /// Parsed ranges are always nonempty.
    pub fn is_empty(self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeError {
    /// The header does not contain one syntactically valid `bytes` range.
    Malformed,
    /// The requested range is outside the representation.
    Unsatisfiable,
    /// Multiple ranges are valid HTTP, but are not implemented by this server.
    Multiple,
}

/// Parse a single `Range` header against a representation of `total_len` bytes.
///
/// `Ok(None)` means the caller should send the complete representation. A
/// suffix range is clamped to the full representation, and an explicit end is
/// clamped to the last byte as described by HTTP range semantics.
pub fn parse_range_header(value: &str, total_len: u64) -> Result<Option<ByteRange>, RangeError> {
    let value = value.trim();
    let (unit, spec) = value.split_once('=').ok_or(RangeError::Malformed)?;
    if !unit.eq_ignore_ascii_case("bytes") || spec.is_empty() {
        return Err(RangeError::Malformed);
    }
    if spec.contains(',') {
        return Err(RangeError::Multiple);
    }

    let (start, end) = spec.split_once('-').ok_or(RangeError::Malformed)?;
    if total_len == 0 {
        return Err(RangeError::Unsatisfiable);
    }

    if start.is_empty() {
        let suffix_len = parse_decimal(end)?;
        if suffix_len == 0 {
            return Err(RangeError::Unsatisfiable);
        }
        let suffix_len = suffix_len.min(total_len);
        return Ok(Some(ByteRange {
            start: total_len - suffix_len,
            end_inclusive: total_len - 1,
        }));
    }

    let start = parse_decimal(start)?;
    let requested_end = if end.is_empty() {
        None
    } else {
        Some(parse_decimal(end)?)
    };
    if start >= total_len {
        return Err(RangeError::Unsatisfiable);
    }

    let end_inclusive = requested_end.unwrap_or(total_len - 1).min(total_len - 1);
    if end_inclusive < start {
        return Err(RangeError::Unsatisfiable);
    }
    Ok(Some(ByteRange {
        start,
        end_inclusive,
    }))
}

fn parse_decimal(value: &str) -> Result<u64, RangeError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RangeError::Malformed);
    }
    value.parse().map_err(|_| RangeError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::{ByteRange, RangeError, parse_range_header};

    #[test]
    fn parses_closed_open_and_suffix_ranges() {
        assert_eq!(
            parse_range_header("bytes=2-5", 10),
            Ok(Some(ByteRange {
                start: 2,
                end_inclusive: 5,
            }))
        );
        assert_eq!(
            parse_range_header("bytes=7-", 10),
            Ok(Some(ByteRange {
                start: 7,
                end_inclusive: 9,
            }))
        );
        assert_eq!(
            parse_range_header("bytes=-4", 10),
            Ok(Some(ByteRange {
                start: 6,
                end_inclusive: 9,
            }))
        );
    }

    #[test]
    fn clamps_to_representation_length() {
        assert_eq!(
            parse_range_header("bytes=8-100", 10),
            Ok(Some(ByteRange {
                start: 8,
                end_inclusive: 9,
            }))
        );
        assert_eq!(
            parse_range_header("bytes=-100", 10),
            Ok(Some(ByteRange {
                start: 0,
                end_inclusive: 9,
            }))
        );
    }

    #[test]
    fn rejects_malformed_and_unsatisfiable_ranges() {
        assert_eq!(
            parse_range_header("items=0-1", 10),
            Err(RangeError::Malformed)
        );
        assert_eq!(
            parse_range_header("bytes=1-2,4-5", 10),
            Err(RangeError::Multiple)
        );
        assert_eq!(
            parse_range_header("bytes=10-", 10),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse_range_header("bytes=-0", 10),
            Err(RangeError::Unsatisfiable)
        );
        assert_eq!(
            parse_range_header("bytes=0-", 0),
            Err(RangeError::Unsatisfiable)
        );
    }

    #[test]
    fn rejects_extra_or_non_decimal_syntax() {
        assert_eq!(
            parse_range_header("bytes=-", 10),
            Err(RangeError::Malformed)
        );
        assert_eq!(
            parse_range_header("bytes=+1-2", 10),
            Err(RangeError::Malformed)
        );
        assert_eq!(
            parse_range_header("bytes=1-2-3", 10),
            Err(RangeError::Malformed)
        );
        assert_eq!(
            parse_range_header("bytes=1- 2", 10),
            Err(RangeError::Malformed)
        );
        assert_eq!(
            parse_range_header("bytes=18446744073709551616-", 10),
            Err(RangeError::Malformed)
        );
        assert_eq!(
            parse_range_header("bytes=0-18446744073709551616", 10),
            Err(RangeError::Malformed)
        );
    }
}
