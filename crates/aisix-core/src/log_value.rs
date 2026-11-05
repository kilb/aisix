//! A caller-chosen value written into a text log line.

use std::fmt;

/// Renders a value the caller chose, such as an adopted request id, as a log
/// field: bare when it cannot be read as log syntax, quoted and escaped when
/// it could.
///
/// A text log line is `key=value` pairs, and a bare value that opens with `"`
/// or carries `key="value"` of its own reads, to anything parsing the line, as
/// fields this gateway never wrote: a caller could put another key's
/// `api_key_id` on a line. Quoting every value would change how every
/// ordinary id renders, so only the ones that need it are quoted.
pub struct LogValue<'a>(pub &'a str);

impl fmt::Display for LogValue<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plain = !self.0.is_empty()
            && self
                .0
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
        if plain {
            f.write_str(self.0)
        } else {
            write!(f, "{:?}", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_id_is_written_as_it_is() {
        for id in ["3f1c9a2e-7b4d-4c1e-9a0b-1d2e3f4a5b6c", "req_01HX.2", "abc"] {
            assert_eq!(LogValue(id).to_string(), id);
        }
    }

    #[test]
    fn a_value_that_could_read_as_fields_is_quoted_and_escaped() {
        let forged = r#""x"api_key_id="VICTIM"total_tokens="999999""#;
        let shown = LogValue(forged).to_string();
        assert!(shown.starts_with('"') && shown.ends_with('"'), "{shown}");
        assert!(!shown.contains(r#"api_key_id="VICTIM""#), "{shown}");
        assert_eq!(LogValue("a=b").to_string(), r#""a=b""#);
        assert_eq!(LogValue("").to_string(), r#""""#);
    }
}
