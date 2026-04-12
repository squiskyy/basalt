/// RESP2 protocol parser and serializer.
///
/// Uses memchr for SIMD-accelerated CRLF scanning on the hot path.
/// Supports RESP pipelining: multiple commands in a single read are all processed.

use memchr::memchr;

/// Maximum allowed size for a RESP BulkString value (512 MB).
pub const MAX_BULK_STRING_SIZE: usize = 512 * 1024 * 1024;

/// A parsed RESP2 value.
#[derive(Debug, Clone, PartialEq)]
pub enum RespValue {
    SimpleString(String),
    Error(String),
    Integer(i64),
    BulkString(Option<Vec<u8>>),
    Array(Option<Vec<RespValue>>),
}

/// Errors that can occur during RESP2 parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// Not enough data to complete parsing — need more bytes.
    Incomplete,
    /// The input is malformed.
    Invalid(String),
}

/// A parsed command extracted from a RESP Array.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub name: String,
    pub args: Vec<Vec<u8>>,
}

/// Find the position of the next `\r\n` (CRLF) in the input.
/// Uses memchr for SIMD-accelerated `\r` scanning on x86_64 and aarch64.
#[inline]
fn find_crlf(input: &[u8]) -> Option<usize> {
    // memchr uses AVX2/SSE2 on x86_64, NEON on aarch64
    let mut start = 0;
    while start < input.len() {
        let pos = memchr(b'\r', &input[start..])?;
        let abs = start + pos;
        // Check if next byte is \n
        if abs + 1 < input.len() && input[abs + 1] == b'\n' {
            return Some(abs);
        }
        // \r not followed by \n — skip and keep searching
        start = abs + 1;
    }
    None
}

/// Parse a RESP2 value from the beginning of `input`.
/// Returns the parsed value and the number of bytes consumed.
fn parse_one(input: &[u8]) -> Result<(RespValue, usize), ParseError> {
    if input.is_empty() {
        return Err(ParseError::Incomplete);
    }

    match input[0] {
        b'+' => {
            // SimpleString: +<string>\r\n
            let crlf = find_crlf(&input[1..]).ok_or(ParseError::Incomplete)?;
            let end = 1 + crlf;
            let s = String::from_utf8(input[1..end].to_vec())
                .map_err(|e| ParseError::Invalid(format!("invalid UTF-8 in SimpleString: {e}")))?;
            Ok((RespValue::SimpleString(s), end + 2)) // +2 for \r\n
        }
        b'-' => {
            // Error: -<string>\r\n
            let crlf = find_crlf(&input[1..]).ok_or(ParseError::Incomplete)?;
            let end = 1 + crlf;
            let s = String::from_utf8(input[1..end].to_vec())
                .map_err(|e| ParseError::Invalid(format!("invalid UTF-8 in Error: {e}")))?;
            Ok((RespValue::Error(s), end + 2))
        }
        b':' => {
            // Integer: :<number>\r\n
            let crlf = find_crlf(&input[1..]).ok_or(ParseError::Incomplete)?;
            let end = 1 + crlf;
            let s = std::str::from_utf8(&input[1..end])
                .map_err(|e| ParseError::Invalid(format!("invalid UTF-8 in Integer: {e}")))?;
            let n: i64 = s
                .parse()
                .map_err(|e| ParseError::Invalid(format!("invalid integer: {e}")))?;
            Ok((RespValue::Integer(n), end + 2))
        }
        b'$' => {
            // BulkString: $<len>\r\n<data>\r\n  or  $-1\r\n (null)
            let crlf = find_crlf(&input[1..]).ok_or(ParseError::Incomplete)?;
            let end = 1 + crlf;
            let len_str = std::str::from_utf8(&input[1..end])
                .map_err(|e| ParseError::Invalid(format!("invalid UTF-8 in BulkString length: {e}")))?;
            let len: i64 = len_str
                .parse()
                .map_err(|e| ParseError::Invalid(format!("invalid BulkString length: {e}")))?;

            if len == -1 {
                return Ok((RespValue::BulkString(None), end + 2));
            }

            if len < 0 {
                return Err(ParseError::Invalid(format!(
                    "negative BulkString length (not -1): {len}"
                )));
            }

            let len = len as usize;

            if len > MAX_BULK_STRING_SIZE {
                return Err(ParseError::Invalid(format!(
                    "BulkString length {len} exceeds maximum allowed size of {MAX_BULK_STRING_SIZE}"
                )));
            }
            let data_start = end + 2;
            let data_end = data_start + len;

            // Need len bytes of data + 2 bytes trailing CRLF
            if input.len() < data_end + 2 {
                return Err(ParseError::Incomplete);
            }

            // Verify trailing CRLF
            if input[data_end] != b'\r' || input[data_end + 1] != b'\n' {
                return Err(ParseError::Invalid(
                    "missing CRLF after BulkString data".to_string(),
                ));
            }

            let data = input[data_start..data_end].to_vec();
            Ok((RespValue::BulkString(Some(data)), data_end + 2))
        }
        b'*' => {
            // Array: *<count>\r\n<elements>  or  *-1\r\n (null array)
            let crlf = find_crlf(&input[1..]).ok_or(ParseError::Incomplete)?;
            let end = 1 + crlf;
            let count_str = std::str::from_utf8(&input[1..end])
                .map_err(|e| ParseError::Invalid(format!("invalid UTF-8 in Array count: {e}")))?;
            let count: i64 = count_str
                .parse()
                .map_err(|e| ParseError::Invalid(format!("invalid Array count: {e}")))?;

            if count == -1 {
                return Ok((RespValue::Array(None), end + 2));
            }

            if count < 0 {
                return Err(ParseError::Invalid(format!(
                    "negative Array count (not -1): {count}"
                )));
            }

            let count = count as usize;
            let mut offset = end + 2;
            let mut elements = Vec::with_capacity(count);

            for _ in 0..count {
                let (elem, consumed) = parse_one(&input[offset..])?;
                elements.push(elem);
                offset += consumed;
            }

            Ok((RespValue::Array(Some(elements)), offset))
        }
        _ => Err(ParseError::Invalid(format!(
            "unknown RESP type byte: {:?}",
            input[0] as char
        ))),
    }
}

/// Parse a single RESP2 value from the beginning of `input`.
pub fn parse(input: &[u8]) -> Result<RespValue, ParseError> {
    let (value, _consumed) = parse_one(input)?;
    Ok(value)
}

/// Parse as many complete RESP2 values as possible from `input`.
/// Returns a vector of parsed values and the number of bytes consumed.
/// If the input ends with incomplete data, those bytes are not consumed.
pub fn parse_pipeline(input: &[u8]) -> (Vec<RespValue>, usize) {
    let mut results = Vec::new();
    let mut offset = 0;

    while offset < input.len() {
        match parse_one(&input[offset..]) {
            Ok((value, consumed)) => {
                results.push(value);
                offset += consumed;
            }
            Err(ParseError::Incomplete) => break,
            Err(_) => break, // On invalid data, stop processing
        }
    }

    (results, offset)
}

impl RespValue {
    /// Extract a Command from a RESP Array.
    ///
    /// The first element must be a BulkString (the command name),
    /// and the remaining elements are the arguments (also BulkStrings).
    /// Returns None if this is not an Array or the format is wrong.
    pub fn to_command(&self) -> Option<Command> {
        match self {
            RespValue::Array(Some(elements)) if !elements.is_empty() => {
                let name = match &elements[0] {
                    RespValue::BulkString(Some(data)) => {
                        String::from_utf8(data.clone()).ok()?
                    }
                    RespValue::SimpleString(s) => s.clone(),
                    _ => return None,
                };

                // Convert command name to uppercase for case-insensitive matching
                let name = name.to_uppercase();

                let args = elements[1..]
                    .iter()
                    .map(|elem| match elem {
                        RespValue::BulkString(Some(data)) => Some(data.clone()),
                        RespValue::BulkString(None) => Some(Vec::new()),
                        RespValue::SimpleString(s) => Some(s.as_bytes().to_vec()),
                        RespValue::Integer(n) => Some(n.to_string().into_bytes()),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;

                Some(Command { name, args })
            }
            _ => None,
        }
    }
}

/// Serialize a RespValue back into RESP2 wire format.
pub fn serialize(value: &RespValue) -> Vec<u8> {
    match value {
        RespValue::SimpleString(s) => {
            let mut buf = Vec::with_capacity(1 + s.len() + 2);
            buf.push(b'+');
            buf.extend_from_slice(s.as_bytes());
            buf.extend_from_slice(b"\r\n");
            buf
        }
        RespValue::Error(s) => {
            let mut buf = Vec::with_capacity(1 + s.len() + 2);
            buf.push(b'-');
            buf.extend_from_slice(s.as_bytes());
            buf.extend_from_slice(b"\r\n");
            buf
        }
        RespValue::Integer(n) => {
            let s = n.to_string();
            let mut buf = Vec::with_capacity(1 + s.len() + 2);
            buf.push(b':');
            buf.extend_from_slice(s.as_bytes());
            buf.extend_from_slice(b"\r\n");
            buf
        }
        RespValue::BulkString(None) => b"$-1\r\n".to_vec(),
        RespValue::BulkString(Some(data)) => {
            let len_str = data.len().to_string();
            let mut buf = Vec::with_capacity(1 + len_str.len() + 2 + data.len() + 2);
            buf.push(b'$');
            buf.extend_from_slice(len_str.as_bytes());
            buf.extend_from_slice(b"\r\n");
            buf.extend_from_slice(data);
            buf.extend_from_slice(b"\r\n");
            buf
        }
        RespValue::Array(None) => b"*-1\r\n".to_vec(),
        RespValue::Array(Some(elements)) => {
            let count_str = elements.len().to_string();
            let mut buf = Vec::with_capacity(1 + count_str.len() + 2);
            buf.push(b'*');
            buf.extend_from_slice(count_str.as_bytes());
            buf.extend_from_slice(b"\r\n");
            for elem in elements {
                buf.extend(serialize(elem));
            }
            buf
        }
    }
}

/// Serialize multiple RespValues into a single buffer (pipelined response).
/// More efficient than serializing individually and concatenating.
pub fn serialize_pipeline(values: &[RespValue]) -> Vec<u8> {
    let mut buf = Vec::new();
    for value in values {
        buf.extend(serialize(value));
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_string() {
        let input = b"+OK\r\n";
        let (val, consumed) = parse_one(input).unwrap();
        assert_eq!(val, RespValue::SimpleString("OK".to_string()));
        assert_eq!(consumed, 5);
    }

    #[test]
    fn test_error() {
        let input = b"-ERR unknown command\r\n";
        let (val, consumed) = parse_one(input).unwrap();
        assert_eq!(val, RespValue::Error("ERR unknown command".to_string()));
        assert_eq!(consumed, 22);
    }

    #[test]
    fn test_integer() {
        let input = b":1000\r\n";
        let (val, consumed) = parse_one(input).unwrap();
        assert_eq!(val, RespValue::Integer(1000));
        assert_eq!(consumed, 7);
    }

    #[test]
    fn test_negative_integer() {
        let input = b":-42\r\n";
        let (val, _) = parse_one(input).unwrap();
        assert_eq!(val, RespValue::Integer(-42));
    }

    #[test]
    fn test_bulk_string() {
        let input = b"$6\r\nfoobar\r\n";
        let (val, consumed) = parse_one(input).unwrap();
        assert_eq!(val, RespValue::BulkString(Some(b"foobar".to_vec())));
        assert_eq!(consumed, 12);
    }

    #[test]
    fn test_null_bulk_string() {
        let input = b"$-1\r\n";
        let (val, _) = parse_one(input).unwrap();
        assert_eq!(val, RespValue::BulkString(None));
    }

    #[test]
    fn test_empty_bulk_string() {
        let input = b"$0\r\n\r\n";
        let (val, _) = parse_one(input).unwrap();
        assert_eq!(val, RespValue::BulkString(Some(Vec::new())));
    }

    #[test]
    fn test_array() {
        let input = b"*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        let (val, _) = parse_one(input).unwrap();
        assert_eq!(
            val,
            RespValue::Array(Some(vec![
                RespValue::BulkString(Some(b"foo".to_vec())),
                RespValue::BulkString(Some(b"bar".to_vec())),
            ]))
        );
    }

    #[test]
    fn test_null_array() {
        let input = b"*-1\r\n";
        let (val, _) = parse_one(input).unwrap();
        assert_eq!(val, RespValue::Array(None));
    }

    #[test]
    fn test_incomplete() {
        let input = b"+OK";
        assert_eq!(parse_one(input), Err(ParseError::Incomplete));
    }

    #[test]
    fn test_to_command() {
        let input = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n";
        let val = parse(input).unwrap();
        let cmd = val.to_command().unwrap();
        assert_eq!(cmd.name, "SET");
        assert_eq!(cmd.args, vec![b"key".to_vec(), b"value".to_vec()]);
    }

    #[test]
    fn test_to_command_case_insensitive() {
        let input = b"*2\r\n$3\r\nget\r\n$3\r\nkey\r\n";
        let val = parse(input).unwrap();
        let cmd = val.to_command().unwrap();
        assert_eq!(cmd.name, "GET");
    }

    #[test]
    fn test_serialize_roundtrip() {
        let original = RespValue::Array(Some(vec![
            RespValue::SimpleString("OK".to_string()),
            RespValue::Integer(42),
            RespValue::BulkString(Some(b"hello".to_vec())),
            RespValue::BulkString(None),
        ]));
        let bytes = serialize(&original);
        let (parsed, consumed) = parse_one(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, original);
    }

    #[test]
    fn test_pipeline() {
        let input = b"+OK\r\n:42\r\n";
        let (vals, consumed) = parse_pipeline(input);
        assert_eq!(vals.len(), 2);
        assert_eq!(consumed, input.len());
        assert_eq!(vals[0], RespValue::SimpleString("OK".to_string()));
        assert_eq!(vals[1], RespValue::Integer(42));
    }

    #[test]
    fn test_pipeline_incomplete() {
        let input = b"+OK\r\n:42";
        let (vals, consumed) = parse_pipeline(input);
        assert_eq!(vals.len(), 1);
        assert_eq!(consumed, 5); // only consumed the first complete message
    }

    #[test]
    fn test_serialize_error() {
        let val = RespValue::Error("ERR something".to_string());
        let bytes = serialize(&val);
        assert_eq!(&bytes, b"-ERR something\r\n");
    }

    #[test]
    fn test_pipeline_serialization() {
        let values = vec![
            RespValue::SimpleString("OK".to_string()),
            RespValue::Integer(42),
        ];
        let bytes = serialize_pipeline(&values);
        assert_eq!(&bytes, b"+OK\r\n:42\r\n");
    }

    #[test]
    fn test_crlf_not_just_cr() {
        // \r without \n should not match
        let input = b"+OK\r";
        assert_eq!(find_crlf(input), None);
    }

    #[test]
    fn test_crlf_multiple_candidates() {
        // \r at multiple positions, only \r\n should match
        // Position 3: \r followed by X (not \n) — skip
        // Position 5: \r followed by \n — match
        let input = b"+OK\rX\r\n";
        assert_eq!(find_crlf(input), Some(5));
    }
}
