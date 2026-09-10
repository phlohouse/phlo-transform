//! A small parser for the Python-literal subset used by dbt `config()` calls
//! and test definitions: strings, numbers, booleans, `None`, lists, dicts,
//! bare identifiers and embedded Jinja expressions (kept opaque).

/// A parsed literal.
#[derive(Clone, Debug, PartialEq)]
pub enum Lit {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    None,
    List(Vec<Lit>),
    /// Ordered key/value pairs; keys are rendered strings.
    Dict(Vec<(String, Lit)>),
    /// A bare identifier such as `true_var` — rare in dbt config.
    Ident(String),
    /// An embedded `{{ ... }}`/`{% ... %}` construct kept verbatim.
    Jinja(String),
}

impl Lit {
    /// The value as a string when it is one (or a bare identifier).
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Lit::Str(value) | Lit::Ident(value) => Some(value),
            _ => None,
        }
    }

    /// A list of strings, when every element is a string/identifier.
    pub fn as_str_list(&self) -> Option<Vec<String>> {
        match self {
            Lit::Str(value) | Lit::Ident(value) => Some(vec![value.clone()]),
            Lit::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(item.as_str()?.to_string());
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// A scalar rendered to a SQL-friendly literal, e.g. `'x'`, `42`, `true`.
    pub fn to_sql_literal(&self) -> Option<String> {
        match self {
            Lit::Str(value) => Some(format!("'{}'", value.replace('\'', "''"))),
            Lit::Ident(value) => Some(format!("'{}'", value.replace('\'', "''"))),
            Lit::Int(value) => Some(value.to_string()),
            Lit::Float(value) => Some(value.to_string()),
            Lit::Bool(value) => Some(value.to_string()),
            Lit::None | Lit::List(_) | Lit::Dict(_) | Lit::Jinja(_) => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LitError(pub String);

/// Parse a comma-separated `key = value` argument list, as in
/// `config(materialized = 'table')` or a test's `- relationships: {to: ...}`.
/// Bare positional arguments are also allowed (their key is `None`).
/// `(positional, keyword)` arguments of a call.
pub type Args = (Vec<Lit>, Vec<(String, Lit)>);

/// Parse a call's argument list.
pub fn parse_args(input: &str) -> Result<Args, LitError> {
    let mut parser = Parser::new(input);
    let mut positional = Vec::new();
    let mut keyword = Vec::new();
    loop {
        parser.skip_ws();
        if parser.eof() {
            break;
        }
        // `name = value` when a bare identifier is followed by `=` (not `==`).
        let checkpoint = parser.pos;
        if let Some(name) = parser.parse_ident() {
            parser.skip_ws();
            if parser.peek() == Some(b'=') && parser.peek2() != Some(b'=') {
                parser.pos += 1;
                keyword.push((name, parser.parse_value()?));
            } else {
                parser.pos = checkpoint;
                positional.push(parser.parse_value()?);
            }
        } else {
            positional.push(parser.parse_value()?);
        }
        parser.skip_ws();
        match parser.peek() {
            Some(b',') => parser.pos += 1,
            None => break,
            Some(other) => {
                return Err(LitError(format!(
                    "unexpected `{}` in arguments",
                    char::from(other)
                )))
            }
        }
    }
    Ok((positional, keyword))
}

/// Parse a single literal value.
pub fn parse_value(input: &str) -> Result<Lit, LitError> {
    let mut parser = Parser::new(input);
    let value = parser.parse_value()?;
    parser.skip_ws();
    if !parser.eof() {
        return Err(LitError(format!("trailing characters in `{input}`")));
    }
    Ok(value)
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input: input.as_bytes(),
            pos: 0,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.input.get(self.pos + 1).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self) -> Result<Lit, LitError> {
        self.skip_ws();
        match self.peek() {
            None => Err(LitError("unexpected end of input".into())),
            Some(b'\'') | Some(b'"') => self.parse_string(),
            Some(b'[') => self.parse_list(),
            Some(b'{') if self.peek2() == Some(b'{') || self.peek2() == Some(b'%') => {
                self.parse_jinja()
            }
            Some(b'{') => self.parse_dict(),
            Some(b'(') => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_ws();
                    if self.peek() == Some(b')') {
                        self.pos += 1;
                        break;
                    }
                    items.push(self.parse_value()?);
                    self.skip_ws();
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b')') => {
                            self.pos += 1;
                            break;
                        }
                        other => {
                            return Err(LitError(format!(
                                "unexpected `{}` in tuple",
                                other.map(char::from).unwrap_or('?')
                            )))
                        }
                    }
                }
                Ok(Lit::List(items))
            }
            Some(b'-' | b'+') | Some(b'0'..=b'9') => self.parse_number(),
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {
                let ident = self.parse_ident().expect("checked first byte");
                match ident.as_str() {
                    "True" | "true" => Ok(Lit::Bool(true)),
                    "False" | "false" => Ok(Lit::Bool(false)),
                    "None" | "none" | "null" => Ok(Lit::None),
                    _ => {
                        // A function call inside config (rare) — keep the name.
                        if self.peek() == Some(b'(') {
                            let mut depth = 0usize;
                            while let Some(c) = self.peek() {
                                self.pos += 1;
                                match c {
                                    b'(' => depth += 1,
                                    b')' => {
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Ok(Lit::Ident(format!("{ident}(...)")))
                        } else {
                            Ok(Lit::Ident(ident))
                        }
                    }
                }
            }
            Some(other) => Err(LitError(format!(
                "unexpected character `{}`",
                char::from(other)
            ))),
        }
    }

    fn parse_string(&mut self) -> Result<Lit, LitError> {
        let quote = self.input[self.pos];
        self.pos += 1;
        // Triple-quoted strings.
        let triple = self.peek() == Some(quote) && self.peek2() == Some(quote);
        if triple {
            self.pos += 2;
        }
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(LitError("unterminated string".into())),
                Some(b'\\') if !triple => {
                    self.pos += 1;
                    let escaped = self
                        .peek()
                        .ok_or_else(|| LitError("unterminated escape".into()))?;
                    out.push(char::from(escaped));
                    self.pos += 1;
                }
                Some(c) if c == quote => {
                    if triple {
                        if self.peek2() == Some(quote)
                            && self.input.get(self.pos + 2) == Some(&quote)
                        {
                            self.pos += 3;
                            break;
                        }
                        out.push(char::from(c));
                        self.pos += 1;
                    } else {
                        self.pos += 1;
                        break;
                    }
                }
                Some(c) => {
                    // dbt config strings are ASCII in practice; keep raw bytes
                    // lossless via char conversion.
                    out.push(char::from(c));
                    self.pos += 1;
                }
            }
        }
        Ok(Lit::Str(out))
    }

    fn parse_list(&mut self) -> Result<Lit, LitError> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(LitError("unterminated list".into())),
                _ => {}
            }
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                other => {
                    return Err(LitError(format!(
                        "unexpected `{}` in list",
                        other.map(char::from).unwrap_or('?')
                    )))
                }
            }
        }
        Ok(Lit::List(items))
    }

    fn parse_dict(&mut self) -> Result<Lit, LitError> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                None => return Err(LitError("unterminated dict".into())),
                _ => {}
            }
            let key = match self.parse_value()? {
                Lit::Str(key) | Lit::Ident(key) => key,
                other => return Err(LitError(format!("invalid dict key {other:?}"))),
            };
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(LitError("expected `:` in dict".into()));
            }
            self.pos += 1;
            items.push((key, self.parse_value()?));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                other => {
                    return Err(LitError(format!(
                        "unexpected `{}` in dict",
                        other.map(char::from).unwrap_or('?')
                    )))
                }
            }
        }
        Ok(Lit::Dict(items))
    }

    /// Consume a `{{ ... }}`/`{% ... %}` block verbatim.
    fn parse_jinja(&mut self) -> Result<Lit, LitError> {
        let start = self.pos;
        let close: &[u8] = if self.peek2() == Some(b'%') {
            b"%}"
        } else {
            b"}}"
        };
        self.pos += 2;
        while self.pos + 1 < self.input.len() {
            if &self.input[self.pos..self.pos + 2] == close {
                self.pos += 2;
                let text = std::str::from_utf8(&self.input[start..self.pos])
                    .map_err(|error| LitError(error.to_string()))?;
                return Ok(Lit::Jinja(text.to_string()));
            }
            self.pos += 1;
        }
        Err(LitError("unterminated Jinja block in literal".into()))
    }

    fn parse_number(&mut self) -> Result<Lit, LitError> {
        let start = self.pos;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.pos += 1;
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'-' | b'+' => {
                    is_float = true;
                    self.pos += 1;
                }
                b'_' => self.pos += 1,
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.input[start..self.pos])
            .map_err(|error| LitError(error.to_string()))?;
        if text.is_empty() || text == "-" || text == "+" {
            return Err(LitError(format!("invalid number `{text}`")));
        }
        if is_float {
            text.parse::<f64>()
                .map(Lit::Float)
                .map_err(|error| LitError(format!("invalid number `{text}`: {error}")))
        } else {
            text.parse::<i64>()
                .map(Lit::Int)
                .map_err(|error| LitError(format!("invalid number `{text}`: {error}")))
        }
    }

    fn parse_ident(&mut self) -> Option<String> {
        match self.peek() {
            Some(c) if c.is_ascii_alphabetic() || c == b'_' => {}
            _ => return None,
        }
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                self.pos += 1;
            } else {
                break;
            }
        }
        Some(
            std::str::from_utf8(&self.input[start..self.pos])
                .unwrap_or_default()
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_config_kwargs() {
        let (positional, kwargs) =
            parse_args("materialized = 'incremental', unique_key = 'id', tags = [\"a\", 'b']")
                .unwrap();
        assert!(positional.is_empty());
        assert_eq!(kwargs.len(), 3);
        assert_eq!(kwargs[0].0, "materialized");
        assert_eq!(kwargs[0].1, Lit::Str("incremental".into()));
        assert_eq!(
            kwargs[2].1.as_str_list(),
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn parses_positional_and_mixed() {
        let (positional, kwargs) = parse_args("'samples', field = 'id'").unwrap();
        assert_eq!(positional, vec![Lit::Str("samples".into())]);
        assert_eq!(kwargs[0], ("field".into(), Lit::Str("id".into())));
    }

    #[test]
    fn parses_dict_values() {
        let value = parse_value("{to: ref('x'), 'field': 'id', n: 3}").unwrap();
        match value {
            Lit::Dict(items) => {
                assert_eq!(items[0].0, "to");
                assert_eq!(items[2].1, Lit::Int(3));
            }
            other => panic!("expected dict, got {other:?}"),
        }
    }

    #[test]
    fn keeps_jinja_opaque() {
        let value = parse_value("{{ var('x') }}").unwrap();
        assert!(matches!(value, Lit::Jinja(_)));
    }
}
