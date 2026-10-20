//! JSON flattened to (path, scalar) pairs: `{"a": {"b": [1, 2]}}` gives `a.b.0 = 1`, `a.b.1 = 2`.

pub const DEPTH_MAX: usize = 32;
const INPUT_BYTES_MAX: usize = 4 << 20;
const PAIRS_MAX: usize = 65536;
const PATH_BYTES_MAX: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Number(f64),
    Text(String),
    Bool(bool),
    Null,
}

struct Frame {
    is_object: bool,
    key: String,
    index: u32,
}

enum Expect {
    Value,
    ValueOrEnd,
    KeyOrEnd,
    Key,
    CommaOrEnd,
}

pub fn flatten(text: &str) -> Result<Vec<(String, Scalar)>, String> {
    if text.len() > INPUT_BYTES_MAX {
        return Err("JSON exceeds 4 MiB".into());
    }
    let bytes = text.as_bytes();
    let mut pairs = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    let mut expect = Expect::Value;
    let mut at = 0usize;
    for _ in 0..=bytes.len() {
        at = skip_whitespace(bytes, at);
        if at == bytes.len() {
            break;
        }
        let byte = bytes[at];
        match expect {
            Expect::Value | Expect::ValueOrEnd => match byte {
                b'{' | b'[' => {
                    if stack.len() == DEPTH_MAX {
                        return Err(format!("nesting deeper than {DEPTH_MAX}"));
                    }
                    stack.push(Frame {
                        is_object: byte == b'{',
                        key: String::new(),
                        index: 0,
                    });
                    expect = if byte == b'{' {
                        Expect::KeyOrEnd
                    } else {
                        Expect::ValueOrEnd
                    };
                    at += 1;
                }
                b']' => {
                    if !matches!(expect, Expect::ValueOrEnd) {
                        return Err("expected a value".into());
                    }
                    let frame = stack.pop().ok_or("']' without an open array")?;
                    if frame.is_object {
                        return Err("']' closes an object".into());
                    }
                    expect = Expect::CommaOrEnd;
                    at += 1;
                }
                _ => {
                    let (scalar, next) = scalar(bytes, at)?;
                    let path = path(&stack);
                    if pairs.len() == PAIRS_MAX || path.len() > PATH_BYTES_MAX {
                        return Err("too many values or path too long".into());
                    }
                    pairs.push((path, scalar));
                    expect = Expect::CommaOrEnd;
                    at = next;
                }
            },
            Expect::KeyOrEnd | Expect::Key => match byte {
                b'}' => {
                    if !matches!(expect, Expect::KeyOrEnd) {
                        return Err("expected a key".into());
                    }
                    stack.pop().ok_or("'}' without an open object")?;
                    expect = Expect::CommaOrEnd;
                    at += 1;
                }
                b'"' => {
                    let (key, next) = string(bytes, at)?;
                    let colon = skip_whitespace(bytes, next);
                    if bytes.get(colon) != Some(&b':') {
                        return Err(format!("expected ':' at byte {colon}"));
                    }
                    stack.last_mut().ok_or("key outside an object")?.key = key;
                    expect = Expect::Value;
                    at = colon + 1;
                }
                _ => return Err(format!("expected a key at byte {at}")),
            },
            Expect::CommaOrEnd => {
                let frame = stack
                    .last_mut()
                    .ok_or_else(|| format!("unexpected data at byte {at}"))?;
                match byte {
                    b',' => {
                        frame.index += 1;
                        expect = if frame.is_object { Expect::Key } else { Expect::Value };
                    }
                    b'}' | b']' => {
                        if frame.is_object != (byte == b'}') {
                            return Err(format!("mismatched close at byte {at}"));
                        }
                        stack.pop();
                    }
                    _ => return Err(format!("expected ',' or a close at byte {at}")),
                }
                at += 1;
            }
        }
    }
    if !stack.is_empty() {
        return Err("unterminated container".into());
    }
    if !matches!(expect, Expect::CommaOrEnd) {
        return Err("no value".into());
    }
    Ok(pairs)
}

pub fn number(pairs: &[(String, Scalar)], wanted: &str) -> Option<f64> {
    pairs
        .iter()
        .find(|(path, _)| path == wanted)
        .and_then(|(_, scalar)| match scalar {
            Scalar::Number(value) => Some(*value),
            _ => None,
        })
}

pub fn text<'a>(pairs: &'a [(String, Scalar)], wanted: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(path, _)| path == wanted)
        .and_then(|(_, scalar)| match scalar {
            Scalar::Text(value) => Some(value.as_str()),
            _ => None,
        })
}

fn path(stack: &[Frame]) -> String {
    let parts: Vec<String> = stack
        .iter()
        .map(|frame| {
            if frame.is_object {
                frame.key.clone()
            } else {
                frame.index.to_string()
            }
        })
        .collect();
    parts.join(".")
}

fn skip_whitespace(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && matches!(bytes[at], b' ' | b'\t' | b'\r' | b'\n') {
        at += 1;
    }
    at
}

fn scalar(bytes: &[u8], at: usize) -> Result<(Scalar, usize), String> {
    if bytes[at] == b'"' {
        let (value, next) = string(bytes, at)?;
        return Ok((Scalar::Text(value), next));
    }
    let mut end = at;
    while end < bytes.len() && !matches!(bytes[end], b',' | b'}' | b']') && !bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    let word = std::str::from_utf8(&bytes[at..end]).map_err(|error| error.to_string())?;
    let value = match word {
        "true" => Scalar::Bool(true),
        "false" => Scalar::Bool(false),
        "null" => Scalar::Null,
        // Python's json writes these for non-finite floats.
        "NaN" => Scalar::Number(f64::NAN),
        "Infinity" => Scalar::Number(f64::INFINITY),
        "-Infinity" => Scalar::Number(f64::NEG_INFINITY),
        _ => {
            let digits = word.strip_prefix('-').unwrap_or(word);
            let integer_end = digits.find(['.', 'e', 'E']).unwrap_or(digits.len());
            let integer = &digits[..integer_end];
            if integer.is_empty() || !integer.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(format!("bad number at byte {at}"));
            }
            if integer.len() > 1 && integer.starts_with('0') {
                return Err(format!("leading zero at byte {at}"));
            }
            let rest = &digits[integer_end..];
            if let Some(fraction) = rest.strip_prefix('.') {
                if !fraction.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                    return Err(format!("empty fraction at byte {at}"));
                }
            }
            let value = word.parse::<f64>().map_err(|_| format!("bad number at byte {at}"))?;
            if !value.is_finite() {
                return Err(format!("number overflow at byte {at}"));
            }
            Scalar::Number(value)
        }
    };
    Ok((value, end))
}

fn string(bytes: &[u8], at: usize) -> Result<(String, usize), String> {
    assert_eq!(bytes[at], b'"');
    let mut out = Vec::new();
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Ok((String::from_utf8(out).map_err(|error| error.to_string())?, i + 1)),
            b'\\' => {
                let escape = *bytes.get(i + 1).ok_or("unterminated escape")?;
                i += 2;
                match escape {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'u' => {
                        let hex = bytes.get(i..i + 4).ok_or("short \\u escape")?;
                        if !hex.iter().all(u8::is_ascii_hexdigit) {
                            return Err("invalid Unicode escape".into());
                        }
                        let mut code =
                            u32::from_str_radix(&String::from_utf8_lossy(hex), 16).map_err(|e| e.to_string())?;
                        if (0xd800..=0xdbff).contains(&code) {
                            if bytes.get(i + 4..i + 6) != Some(b"\\u") {
                                return Err("missing low surrogate".into());
                            }
                            let low_hex = bytes.get(i + 6..i + 10).ok_or("short low surrogate")?;
                            if !low_hex.iter().all(u8::is_ascii_hexdigit) {
                                return Err("invalid low surrogate escape".into());
                            }
                            let low = u32::from_str_radix(&String::from_utf8_lossy(low_hex), 16)
                                .map_err(|error| error.to_string())?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return Err("invalid low surrogate".into());
                            }
                            code = 0x10000 + ((code - 0xd800) << 10) + low - 0xdc00;
                            i += 6;
                        }
                        let character = char::from_u32(code).ok_or("invalid Unicode scalar")?;
                        out.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
                        i += 4;
                    }
                    b'"' | b'\\' | b'/' => out.push(escape),
                    _ => return Err("invalid escape".into()),
                }
            }
            0..=31 => return Err("unescaped control character".into()),
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    Err("unterminated string".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_nested_values() {
        let pairs = flatten(r#"{"a": {"b": [1, 2.5, "x"]}, "c": true, "d": null, "e/f": -3e2}"#).unwrap();
        assert_eq!(number(&pairs, "a.b.0"), Some(1.0));
        assert_eq!(number(&pairs, "a.b.1"), Some(2.5));
        assert_eq!(text(&pairs, "a.b.2"), Some("x"));
        assert_eq!(number(&pairs, "e/f"), Some(-300.0));
        assert_eq!(pairs.len(), 6);
        assert_eq!(number(&pairs, "c"), None);
    }

    #[test]
    fn handles_empty_containers_and_escapes() {
        let pairs = flatten(r#"{"a": [], "b": {}, "c": "q\"é\n", "d": [[1], [2]]}"#).unwrap();
        assert_eq!(text(&pairs, "c"), Some("q\"é\n"));
        assert_eq!(number(&pairs, "d.1.0"), Some(2.0));
        assert_eq!(pairs.len(), 3);
    }

    #[test]
    fn rejects_broken_input() {
        assert!(flatten(r#"{"a": 1"#).is_err());
        assert!(flatten(r#"{"a" 1}"#).is_err());
        assert!(flatten(r#"{"a": 1]"#).is_err());
        assert!(flatten("").is_err());
        assert!(flatten(&"[".repeat(DEPTH_MAX + 1)).is_err());
    }

    #[test]
    fn reads_non_finite_numbers() {
        let pairs = flatten(r#"{"a": NaN, "b": Infinity}"#).unwrap();
        assert!(number(&pairs, "a").unwrap().is_nan());
        assert_eq!(number(&pairs, "b"), Some(f64::INFINITY));
    }

    #[test]
    fn rejects_malformed_json_and_decodes_surrogate_pairs() {
        for input in [
            "[1,]",
            "{\"a\":1,}",
            "{\"a\":]",
            "01",
            "+1",
            "1.",
            "1e999",
            "1 2",
            r#""\q""#,
            r#""\uD800""#,
            r#""\u+001""#,
            "\"a\nb\"",
        ] {
            assert!(flatten(input).is_err(), "accepted {input:?}");
        }
        let pairs = flatten(r#"{"pet":"\uD83D\uDC08"}"#).unwrap();
        assert_eq!(text(&pairs, "pet"), Some("🐈"));
        assert!(flatten(&format!("[{}0]", "0,".repeat(PAIRS_MAX))).is_err());
        assert!(flatten(&" ".repeat(INPUT_BYTES_MAX + 1)).is_err());
    }
}
