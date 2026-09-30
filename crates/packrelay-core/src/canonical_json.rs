// Canonical JSON — the bytes a manifest signature covers.
//
// Must stay byte-for-byte identical to the cloud's `canonicalize()`
// in PackRelayCloud's src/lib/canonical-json.ts, or every signature
// check fails. The rules, as that file implements them:
//
//   - object keys sorted with JS's default Array.prototype.sort(),
//     i.e. by UTF-16 code units (NOT Rust's byte / code-point order;
//     the two disagree once keys mix astral chars with U+E000..U+FFFF);
//   - no whitespace between tokens;
//   - strings escaped the way JSON.stringify escapes them: `"`, `\`,
//     \b \f \n \r \t, other control chars as lowercase \u00XX, and
//     everything else (including non-ASCII, `/`, U+2028) written raw;
//   - numbers as JSON.stringify writes them.
//
// Numbers are the one place we're stricter than the cloud: rather
// than re-implement JS's float formatting, we only accept integers JS
// can represent exactly. The manifest schema has no non-integer
// numbers and the cloud stores manifests via JSON.stringify, so a
// served manifest never has anything else. Anything outside that
// fails closed instead of guessing at the signed bytes.
//
// tests/fixtures/signing-fixture.json holds cases produced by the
// cloud's own canonicalize(); tests/signing.rs checks them all.

use std::cmp::Ordering;
use std::fmt::Write as _;

use anyhow::{bail, Result};
use serde_json::{Number, Value};

/// Number.MAX_SAFE_INTEGER — the largest integer a JS number holds exactly.
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// Canonical JSON text of `value`. Encode as UTF-8 (`.as_bytes()`)
/// to get the signing input.
pub fn canonicalize(value: &Value) -> Result<String> {
    let mut out = String::new();
    write_value(&mut out, value)?;
    Ok(out)
}

fn write_value(out: &mut String, value: &Value) -> Result<()> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(out, n)?,
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|(a, _), (b, _)| utf16_cmp(a, b));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, item)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// JS's default sort order for strings: UTF-16 code unit by code unit.
fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

fn write_number(out: &mut String, n: &Number) -> Result<()> {
    if let Some(u) = n.as_u64() {
        if u <= MAX_SAFE_INTEGER {
            let _ = write!(out, "{u}");
            return Ok(());
        }
    } else if let Some(i) = n.as_i64() {
        if i.unsigned_abs() <= MAX_SAFE_INTEGER {
            let _ = write!(out, "{i}");
            return Ok(());
        }
    } else if let Some(f) = n.as_f64() {
        // `5.0` parses as a float here but is the number 5 in JS, which
        // JSON.stringify writes as "5" (and -0 as "0").
        if f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64 {
            let _ = write!(out, "{}", f as i64);
            return Ok(());
        }
    }
    bail!(
        "manifest contains the number {n}, which isn't a whole number JavaScript \
         represents exactly; its signed bytes can't be reproduced"
    )
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c as u32 == 0x08 => out.push_str("\\b"),
            c if c as u32 == 0x0c => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn canon(text: &str) -> String {
        canonicalize(&serde_json::from_str(text).unwrap()).unwrap()
    }

    #[test]
    fn sorts_keys_and_drops_whitespace() {
        assert_eq!(
            canon(r#"{ "b": [1, 2], "a": { "d": null, "c": true } }"#),
            r#"{"a":{"c":true,"d":null},"b":[1,2]}"#
        );
    }

    #[test]
    fn whole_floats_write_like_js() {
        assert_eq!(canon("[5.0, -0.0, 1e3]"), "[5,0,1000]");
    }

    #[test]
    fn rejects_numbers_js_would_format_differently() {
        for text in ["1.5", "1e300", "9007199254740992", "-9007199254740992"] {
            let value: Value = serde_json::from_str(text).unwrap();
            assert!(canonicalize(&value).is_err(), "{text} should be rejected");
        }
        assert!(canonicalize(&json!(9_007_199_254_740_991_u64)).is_ok());
    }

    #[test]
    fn escapes_control_chars_as_lowercase_hex() {
        let s: String = [0x01u8, 0x08, 0x0c, 0x1f]
            .iter()
            .map(|b| *b as char)
            .collect();
        assert_eq!(canonicalize(&json!(s)).unwrap(), r#""\u0001\b\f\u001f""#);
    }
}
