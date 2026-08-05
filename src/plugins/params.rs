//! One parameter vocabulary, for every plugin format.
//!
//! A compressor's threshold is set the same way whether the compressor is a
//! [built-in](super::atome), a VST3, or an Audio Unit:
//!
//! ```
//! use ::atome::plugins::{atome, ParamError, Params};
//!
//! let mut compressor = atome::compressor(48_000);
//! compressor.set_params(&Params::parse(r#"{ "threshold_db": -18, "ratio": 4 }"#)?)?;
//! # Ok::<(), ParamError>(())
//! ```
//!
//! The formats do not agree on what a parameter *is* — a VST3 addresses them by
//! index and normalises to `0.0..=1.0`, an AU uses an address and native units,
//! a built-in has named fields — so [`Params`] is a name-to-value map and each
//! backend translates. Names are how they meet: for the hosted formats the name
//! is what the plugin calls the parameter in its own UI.
//!
//! # The syntax
//!
//! A flat JSON object. Deliberately not all of JSON — no nesting, no arrays,
//! no `null` — because a parameter set is flat, and accepting more would only
//! move the error further from the mistake:
//!
//! ```text
//! { "threshold_db": -18.0, "ratio": 4, "auto_makeup": true, "mode": "peak" }
//! ```
//!
//! Three things are accepted beyond strict JSON, because this is as often
//! typed by hand as generated: the outer braces may be left off, keys need not
//! be quoted, and a trailing comma is allowed.
//!
//! ```text
//! threshold_db: -18, ratio: 4
//! ```
//!
//! Nothing here is on the audio path. Parsing allocates and reports errors by
//! `String`; parameters are applied between blocks, not during one.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

// -----------------------------------------------------------------------------
//  Values
// -----------------------------------------------------------------------------

/// A single parameter value.
///
/// Numbers are `f64` whatever they were written as: `4` and `4.0` are the same
/// value, since a plugin parameter that is conceptually an integer (a mode, a
/// voice count) still arrives through the same `f64` door in every format.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Number(f64),
    Bool(bool),
    Text(String),
}

impl Value {
    /// A number that was stored as an `f32`, without the noise widening adds.
    ///
    /// `1.4f32 as f64` is 1.399999976158142 — true, and not what anyone typed.
    /// Going through the shortest `f32` representation gives back the 1.4 that
    /// was written, which is what a parameter listing should show and what a
    /// saved chain should contain.
    pub fn from_f32(value: f32) -> Self {
        Self::Number(value.to_string().parse().unwrap_or(value as f64))
    }

    /// The value as a number, if it is one.
    ///
    /// A bool counts: `true` is 1.0. Formats that address every parameter as a
    /// number need somewhere for a toggle to go, and this is it.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(number) => Some(*number),
            Self::Bool(true) => Some(1.0),
            Self::Bool(false) => Some(0.0),
            Self::Text(_) => None,
        }
    }

    /// The value as a bool, if it is one.
    ///
    /// A number counts, on the same reasoning as [`as_number`](Self::as_number)
    /// in reverse: anything not equal to zero is true.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(flag) => Some(*flag),
            Self::Number(number) => Some(*number != 0.0),
            Self::Text(_) => None,
        }
    }

    /// The value as text, if it is text. Numbers are not stringified.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// What this value is, for an error message.
    fn kind(&self) -> &'static str {
        match self {
            Self::Number(_) => "a number",
            Self::Bool(_) => "a boolean",
            Self::Text(_) => "text",
        }
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<f32> for Value {
    fn from(value: f32) -> Self {
        Self::from_f32(value)
    }
}

impl From<i32> for Value {
    fn from(value: i32) -> Self {
        Self::Number(value as f64)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // `{}` on an f64 already drops a trailing `.0`, which keeps a
            // round-trip through `Display` and `parse` from growing noise.
            Self::Number(number) => write!(f, "{number}"),
            Self::Bool(flag) => write!(f, "{flag}"),
            Self::Text(text) => write!(f, "\"{}\"", escape(text)),
        }
    }
}

// -----------------------------------------------------------------------------
//  Errors
// -----------------------------------------------------------------------------

/// What went wrong setting or parsing parameters.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamError {
    /// The text is not a parameter object.
    Syntax { at: usize, message: String },
    /// The plugin has no parameter by that name.
    Unknown { key: String, known: Vec<String> },
    /// The parameter exists but does not take a value of that kind.
    Type {
        key: String,
        wanted: &'static str,
        got: &'static str,
    },
    /// The value is outside what the parameter accepts.
    Range {
        key: String,
        value: f64,
        min: f64,
        max: f64,
    },
    /// The plugin has parameters but this build cannot reach them.
    Unsupported { message: String },
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { at, message } => {
                write!(f, "parameter syntax error at byte {at}: {message}")
            }
            Self::Unknown { key, known } => {
                write!(f, "no parameter called '{key}'")?;
                // A misspelling is the usual cause, and the fix is almost
                // always visible in the list — so print it, up to the point
                // where it stops being readable.
                if !known.is_empty() {
                    let shown = known.len().min(12);
                    write!(f, " (has: {}", known[..shown].join(", "))?;
                    if known.len() > shown {
                        write!(f, ", and {} more", known.len() - shown)?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            Self::Type { key, wanted, got } => {
                write!(f, "'{key}' wants {wanted}, got {got}")
            }
            Self::Range {
                key,
                value,
                min,
                max,
            } => write!(f, "'{key}' is {value}, outside {min}..={max}"),
            Self::Unsupported { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ParamError {}

// -----------------------------------------------------------------------------
//  Schema
// -----------------------------------------------------------------------------

/// Whether a parameter is a quantity or a switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamKind {
    Number,
    Toggle,
}

/// What a plugin says about one of its parameters.
///
/// Enough to build a UI, validate a value, or print a help listing — see
/// [`Plugin::param_schema`](super::Plugin::param_schema).
#[derive(Clone, Debug, PartialEq)]
pub struct ParamSpec {
    pub name: String,
    /// `"dB"`, `"Hz"`, `"ms"`, or empty for a ratio or a switch.
    pub unit: String,
    pub kind: ParamKind,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    /// One line on what the parameter does.
    pub about: String,
}

impl ParamSpec {
    /// A numeric parameter.
    pub fn number(
        name: &str,
        unit: &str,
        min: f64,
        max: f64,
        default: f64,
        about: &str,
    ) -> Self {
        Self {
            name: name.to_string(),
            unit: unit.to_string(),
            kind: ParamKind::Number,
            min,
            max,
            default,
            about: about.to_string(),
        }
    }

    /// A switch.
    pub fn toggle(name: &str, default: bool, about: &str) -> Self {
        Self {
            name: name.to_string(),
            unit: String::new(),
            kind: ParamKind::Toggle,
            min: 0.0,
            max: 1.0,
            default: if default { 1.0 } else { 0.0 },
            about: about.to_string(),
        }
    }

    /// Checks a value against this parameter, returning it as a number.
    ///
    /// Out-of-range is an error rather than a clamp. A threshold of `-180 dB`
    /// asked for by mistake is silence, and silence that was quietly corrected
    /// to the nearest legal value is harder to find than one that refused.
    pub fn check(&self, value: &Value) -> Result<f64, ParamError> {
        let number = match self.kind {
            ParamKind::Toggle => value
                .as_bool()
                .map(|flag| if flag { 1.0 } else { 0.0 })
                .ok_or_else(|| ParamError::Type {
                    key: self.name.clone(),
                    wanted: "a boolean",
                    got: value.kind(),
                })?,
            ParamKind::Number => value.as_number().ok_or_else(|| ParamError::Type {
                key: self.name.clone(),
                wanted: "a number",
                got: value.kind(),
            })?,
        };

        if number < self.min || number > self.max {
            return Err(ParamError::Range {
                key: self.name.clone(),
                value: number,
                min: self.min,
                max: self.max,
            });
        }

        Ok(number)
    }
}

impl fmt::Display for ParamSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ParamKind::Toggle => write!(
                f,
                "{:<16} {:<22} {}",
                self.name,
                format!("(default {})", self.default != 0.0),
                self.about
            ),
            ParamKind::Number => write!(
                f,
                "{:<16} {:<22} {}",
                self.name,
                format!(
                    "{}..={}{}{}",
                    self.min,
                    self.max,
                    if self.unit.is_empty() { "" } else { " " },
                    self.unit
                ),
                self.about
            ),
        }
    }
}

// -----------------------------------------------------------------------------
//  Params
// -----------------------------------------------------------------------------

/// A set of parameter values, by name.
///
/// Ordered, because the order a set prints in should not change between runs —
/// a chain's parameters end up in configuration files and diffs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Params {
    values: BTreeMap<String, Value>,
}

impl Params {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses the [syntax described on this module](self).
    ///
    /// # Errors
    ///
    /// [`ParamError::Syntax`], with the byte offset of whatever stopped it.
    pub fn parse(text: &str) -> Result<Self, ParamError> {
        Parser::new(text).parse()
    }

    /// Sets one value, returning `self` — for building a set inline.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.set(key, value);
        self
    }

    /// Sets one value, replacing any already under that name.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.values.insert(key.into(), value.into());
    }

    /// The value under `key`.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// The value under `key` as a number.
    pub fn number(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_number()
    }

    /// The value under `key` as a bool.
    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    /// The value under `key` as text.
    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_text()
    }

    /// Removes a value.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.values.remove(key)
    }

    /// Every name and value, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.values.iter().map(|(key, value)| (key.as_str(), value))
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl FromStr for Params {
    type Err = ParamError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl<K: Into<String>, V: Into<Value>> FromIterator<(K, V)> for Params {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(pairs: I) -> Self {
        Self {
            values: pairs
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }
}

/// Prints as a JSON object, which [`Params::parse`] reads back.
impl fmt::Display for Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (index, (key, value)) in self.values.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            write!(f, " \"{}\": {value}", escape(key))?;
        }
        f.write_str(if self.values.is_empty() { "}" } else { " }" })
    }
}

/// Escapes the two characters that would otherwise end a JSON string early.
///
/// Control characters are not escaped, and not expected: these are parameter
/// names and mode words, not arbitrary text.
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

// -----------------------------------------------------------------------------
//  Parser
// -----------------------------------------------------------------------------

/// A recursive-descent parser for the flat object described on the module.
///
/// Byte-oriented: every value in the grammar is ASCII-delimited, so the only
/// place a multi-byte character can appear is inside a quoted string, where it
/// is copied through without being looked at.
struct Parser<'a> {
    text: &'a [u8],
    source: &'a str,
    at: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            text: source.as_bytes(),
            source,
            at: 0,
        }
    }

    fn parse(mut self) -> Result<Params, ParamError> {
        let mut values = BTreeMap::new();

        self.space();

        // The braces are optional, but they have to match: an opening brace
        // with no closing one is a truncated object, not a brace-less list
        // that happens to start with `{`.
        let braced = self.eat(b'{');
        self.space();

        while self.at < self.text.len() {
            if braced && self.peek() == Some(b'}') {
                break;
            }

            let key = self.key()?;
            self.space();

            if !self.eat(b':') && !self.eat(b'=') {
                return Err(self.error("expected ':' after the parameter name"));
            }

            self.space();
            let value = self.value()?;
            // Last one wins. Refusing a duplicate would be defensible, but a
            // set built by appending an override to a default is a reasonable
            // thing to write, and this is what makes it work.
            values.insert(key, value);

            self.space();
            if !self.eat(b',') {
                break;
            }
            self.space();
        }

        self.space();

        if braced && !self.eat(b'}') {
            return Err(self.error("expected '}' to close the parameter object"));
        }

        self.space();
        if self.at < self.text.len() {
            return Err(self.error("unexpected trailing text"));
        }

        Ok(Params { values })
    }

    fn key(&mut self) -> Result<String, ParamError> {
        if self.peek() == Some(b'"') {
            return self.string();
        }

        let start = self.at;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.' {
                self.at += 1;
            } else {
                break;
            }
        }

        if self.at == start {
            return Err(self.error("expected a parameter name"));
        }

        Ok(self.source[start..self.at].to_string())
    }

    fn value(&mut self) -> Result<Value, ParamError> {
        match self.peek() {
            Some(b'"') => self.string().map(Value::Text),
            Some(b't') if self.word("true") => Ok(Value::Bool(true)),
            Some(b'f') if self.word("false") => Ok(Value::Bool(false)),
            Some(byte) if byte == b'-' || byte == b'+' || byte.is_ascii_digit() || byte == b'.' => {
                self.number()
            }
            Some(_) => Err(self.error("expected a number, a boolean, or a quoted string")),
            None => Err(self.error("expected a value")),
        }
    }

    fn number(&mut self) -> Result<Value, ParamError> {
        let start = self.at;

        if matches!(self.peek(), Some(b'-') | Some(b'+')) {
            self.at += 1;
        }
        while matches!(self.peek(), Some(byte) if byte.is_ascii_digit() || byte == b'.') {
            self.at += 1;
        }
        // Exponent, so a value pasted from something that prints in scientific
        // notation reads back.
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'-') | Some(b'+')) {
                self.at += 1;
            }
            while matches!(self.peek(), Some(byte) if byte.is_ascii_digit()) {
                self.at += 1;
            }
        }

        self.source[start..self.at]
            .parse::<f64>()
            .map(Value::Number)
            .map_err(|_| {
                let at = start;
                ParamError::Syntax {
                    at,
                    message: format!("'{}' is not a number", &self.source[start..self.at]),
                }
            })
    }

    fn string(&mut self) -> Result<String, ParamError> {
        // Caller has already seen the opening quote.
        self.at += 1;
        let mut out = String::new();

        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    match self.peek() {
                        Some(b'"') => out.push('"'),
                        Some(b'\\') => out.push('\\'),
                        Some(b'n') => out.push('\n'),
                        Some(b't') => out.push('\t'),
                        Some(_) => return Err(self.error("unknown escape")),
                        None => return Err(self.error("unterminated escape")),
                    }
                    self.at += 1;
                }
                Some(_) => {
                    // Step a whole character, not a byte: the rest of the
                    // grammar is ASCII but a string's contents need not be.
                    let rest = &self.source[self.at..];
                    let character = rest.chars().next().expect("non-empty by the match");
                    out.push(character);
                    self.at += character.len_utf8();
                }
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    /// Matches a bare word, but only where a delimiter follows it — so `truex`
    /// is not read as `true` with trailing rubbish.
    fn word(&mut self, word: &str) -> bool {
        let end = self.at + word.len();
        if self.source.get(self.at..end) != Some(word) {
            return false;
        }
        if matches!(self.text.get(end), Some(byte) if byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return false;
        }

        self.at = end;
        true
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(byte) if byte.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    fn error(&self, message: &str) -> ParamError {
        ParamError::Syntax {
            at: self.at,
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_json_object() {
        let params =
            Params::parse(r#"{ "threshold_db": -18.5, "ratio": 4, "auto": true, "mode": "peak" }"#)
                .expect("valid");

        assert_eq!(params.number("threshold_db"), Some(-18.5));
        assert_eq!(params.number("ratio"), Some(4.0));
        assert_eq!(params.bool("auto"), Some(true));
        assert_eq!(params.text("mode"), Some("peak"));
    }

    #[test]
    fn braces_and_quotes_are_optional() {
        let params = Params::parse("threshold_db: -18, ratio: 4").expect("valid");
        assert_eq!(params.number("threshold_db"), Some(-18.0));
        assert_eq!(params.number("ratio"), Some(4.0));
    }

    #[test]
    fn equals_reads_as_a_colon() {
        let params = Params::parse("gain_db = -6").expect("valid");
        assert_eq!(params.number("gain_db"), Some(-6.0));
    }

    #[test]
    fn a_trailing_comma_is_allowed() {
        let params = Params::parse("{ a: 1, b: 2, }").expect("valid");
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn an_empty_object_is_an_empty_set() {
        assert!(Params::parse("{}").expect("valid").is_empty());
        assert!(Params::parse("").expect("valid").is_empty());
        assert!(Params::parse("   ").expect("valid").is_empty());
    }

    #[test]
    fn reads_exponents_and_signs() {
        let params = Params::parse("a: 1e3, b: -2.5e-2, c: +7").expect("valid");
        assert_eq!(params.number("a"), Some(1000.0));
        assert_eq!(params.number("b"), Some(-0.025));
        assert_eq!(params.number("c"), Some(7.0));
    }

    #[test]
    fn the_last_value_for_a_name_wins() {
        let params = Params::parse("gain_db: 0, gain_db: -6").expect("valid");
        assert_eq!(params.number("gain_db"), Some(-6.0));
    }

    #[test]
    fn round_trips_through_display() {
        let original =
            Params::parse(r#"{"a": 1.5, "b": true, "c": "text", "d": -2}"#).expect("valid");
        let printed = original.to_string();
        let reparsed = Params::parse(&printed).expect("its own output");

        assert_eq!(original, reparsed, "printed as {printed}");
    }

    #[test]
    fn an_empty_set_prints_as_an_empty_object() {
        assert_eq!(Params::new().to_string(), "{}");
        assert_eq!(Params::parse("{}").unwrap(), Params::new());
    }

    #[test]
    fn quotes_in_a_value_survive_a_round_trip() {
        let params = Params::new().with("mode", "a \"quoted\" word");
        let reparsed = Params::parse(&params.to_string()).expect("its own output");
        assert_eq!(reparsed.text("mode"), Some("a \"quoted\" word"));
    }

    #[test]
    fn rejects_what_is_not_a_parameter_object() {
        for bad in [
            "{",
            "{ a: 1",
            "a:",
            "a: ,",
            ": 1",
            "a: 1 b: 2",
            "{ a: 1 } trailing",
            "a: [1, 2]",
            "a: null",
            "a: \"unterminated",
            "a: truex",
        ] {
            assert!(
                Params::parse(bad).is_err(),
                "{bad:?} should not have parsed"
            );
        }
    }

    #[test]
    fn a_syntax_error_says_where() {
        let Err(ParamError::Syntax { at, .. }) = Params::parse("ratio: 4, : 2") else {
            panic!("expected a syntax error");
        };
        assert_eq!(at, 10, "should point at the missing name");
    }

    #[test]
    fn an_f32_does_not_widen_into_noise() {
        // The whole point: `1.4f32 as f64` is 1.399999976158142.
        assert_eq!(Value::from_f32(1.4), Value::Number(1.4));
        assert_eq!(Value::from_f32(0.3), Value::Number(0.3));
        assert_eq!(Value::from_f32(-24.0), Value::Number(-24.0));
        assert_eq!(Value::from_f32(1.4).to_string(), "1.4");
    }

    #[test]
    fn numbers_and_booleans_convert_between_each_other() {
        assert_eq!(Value::Bool(true).as_number(), Some(1.0));
        assert_eq!(Value::Number(0.0).as_bool(), Some(false));
        assert_eq!(Value::Number(0.5).as_bool(), Some(true));
        // Text does not, in either direction: a mode word is not a quantity.
        assert_eq!(Value::Text("4".into()).as_number(), None);
        assert_eq!(Value::Number(1.0).as_text(), None);
    }

    #[test]
    fn a_spec_checks_range_and_kind() {
        let spec = ParamSpec::number("ratio", "", 1.0, 20.0, 4.0, "");

        assert_eq!(spec.check(&Value::Number(4.0)), Ok(4.0));
        assert!(matches!(
            spec.check(&Value::Number(40.0)),
            Err(ParamError::Range { .. })
        ));
        assert!(matches!(
            spec.check(&Value::Text("loud".into())),
            Err(ParamError::Type { .. })
        ));

        // The bounds themselves are inside the range.
        assert_eq!(spec.check(&Value::Number(1.0)), Ok(1.0));
        assert_eq!(spec.check(&Value::Number(20.0)), Ok(20.0));
    }

    #[test]
    fn a_toggle_takes_a_bool_or_a_number() {
        let spec = ParamSpec::toggle("bypass", false, "");

        assert_eq!(spec.check(&Value::Bool(true)), Ok(1.0));
        assert_eq!(spec.check(&Value::Number(0.0)), Ok(0.0));
        assert!(matches!(
            spec.check(&Value::Text("on".into())),
            Err(ParamError::Type { .. })
        ));
    }

    #[test]
    fn an_unknown_parameter_lists_what_there_is() {
        let error = ParamError::Unknown {
            key: "ratioo".into(),
            known: vec!["ratio".into(), "threshold_db".into()],
        };

        let message = error.to_string();
        assert!(message.contains("ratioo"), "{message}");
        assert!(message.contains("ratio, threshold_db"), "{message}");
    }
}
