//! The property value type, plus a small self-contained JSON codec.

use std::cmp::Ordering;
use std::fmt;

#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    List(Vec<Value>),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Text(_) => "text",
            Value::List(_) => "list",
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            Value::Text(t) => t.parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(t) => Some(t.as_str()),
            _ => None,
        }
    }

    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Text(t) => !t.is_empty(),
            Value::List(l) => !l.is_empty(),
        }
    }

    /// Rank used to order values of different types deterministically.
    fn rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Float(_) => 2,
            Value::Text(_) => 3,
            Value::List(_) => 4,
        }
    }

    /// A total order over every value, so values can key a BTreeMap index.
    /// Numbers compare numerically across Int/Float; NaN sorts last.
    pub fn total_cmp(&self, other: &Value) -> Ordering {
        let (a, b) = (self.rank(), other.rank());
        if a != b {
            return a.cmp(&b);
        }
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            (Value::Text(x), Value::Text(y)) => x.cmp(y),
            (Value::List(x), Value::List(y)) => {
                for (i, j) in x.iter().zip(y.iter()) {
                    let c = i.total_cmp(j);
                    if c != Ordering::Equal {
                        return c;
                    }
                }
                x.len().cmp(&y.len())
            }
            _ => {
                let x = self.as_f64().unwrap_or(f64::NAN);
                let y = other.as_f64().unwrap_or(f64::NAN);
                x.total_cmp(&y)
            }
        }
    }

    pub fn to_json(&self) -> String {
        let mut s = String::new();
        self.write_json(&mut s);
        s
    }

    pub fn write_json(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Float(f) => {
                if f.is_finite() {
                    out.push_str(&format!("{}", f));
                } else {
                    out.push_str("null");
                }
            }
            Value::Text(t) => write_json_string(t, out),
            Value::List(l) => {
                out.push('[');
                for (i, v) in l.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_json(out);
                }
                out.push(']');
            }
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Text(a), Value::Text(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Int(_), Value::Float(_))
            | (Value::Float(_), Value::Int(_))
            | (Value::Float(_), Value::Float(_)) => self.as_f64() == other.as_f64(),
            _ => false,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Int(i) => write!(f, "{}", i),
            Value::Float(x) => write!(f, "{}", x),
            Value::Text(t) => write!(f, "{}", t),
            Value::List(_) => write!(f, "{}", self.to_json()),
        }
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Value::Int(v as i64)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

pub fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------- JSON input

/// A parsed JSON document. Objects keep insertion order, which keeps
/// round-tripping stable and makes import/export diffs readable.
#[derive(Clone, Debug)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(a) => Some(a.as_slice()),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(i) if *i >= 0 => Some(*i as u64),
            Json::Float(f) if *f >= 0.0 => Some(*f as u64),
            _ => None,
        }
    }

    /// Flatten to a property value. Objects become their JSON text, since the
    /// property model is deliberately flat.
    pub fn to_value(&self) -> Value {
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Int(i) => Value::Int(*i),
            Json::Float(f) => Value::Float(*f),
            Json::Text(t) => Value::Text(t.clone()),
            Json::Array(a) => Value::List(a.iter().map(|j| j.to_value()).collect()),
            Json::Object(_) => Value::Text(json_to_string(self)),
        }
    }
}

pub fn json_to_string(j: &Json) -> String {
    let mut out = String::new();
    write_json(j, &mut out);
    out
}

fn write_json(j: &Json, out: &mut String) {
    match j {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(i) => out.push_str(&i.to_string()),
        Json::Float(f) => out.push_str(&f.to_string()),
        Json::Text(t) => write_json_string(t, out),
        Json::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(v, out);
            }
            out.push(']');
        }
        Json::Object(fields) => {
            out.push('{');
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(k, out);
                out.push(':');
                write_json(v, out);
            }
            out.push('}');
        }
    }
}

pub fn parse_json(src: &str) -> Result<Json, String> {
    let b = src.as_bytes();
    let mut p = JsonParser {
        b,
        pos: 0,
        depth: 0,
    };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.pos != b.len() {
        return Err(format!("trailing input at byte {}", p.pos));
    }
    Ok(v)
}

struct JsonParser<'a> {
    b: &'a [u8],
    pos: usize,
    depth: u32,
}

impl<'a> JsonParser<'a> {
    fn ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> Result<(), String> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at byte {}", c as char, self.pos))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        if self.depth > 200 {
            return Err("json nested too deeply".into());
        }
        match self.peek() {
            None => Err("unexpected end of json".into()),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Text(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(_) => self.number(),
        }
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(format!("bad literal at byte {}", self.pos))
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.eat(b'{')?;
        self.depth += 1;
        let mut fields = Vec::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Object(fields));
        }
        loop {
            self.ws();
            let k = self.string()?;
            self.ws();
            self.eat(b':')?;
            self.ws();
            let v = self.value()?;
            fields.push((k, v));
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.pos)),
            }
        }
        self.depth -= 1;
        Ok(Json::Object(fields))
    }

    fn array(&mut self) -> Result<Json, String> {
        self.eat(b'[')?;
        self.depth += 1;
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.pos)),
            }
        }
        self.depth -= 1;
        Ok(Json::Array(items))
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat(b'"')?;
        let mut s = String::new();
        loop {
            let c = self.peek().ok_or("unterminated string")?;
            self.pos += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'b' => s.push('\u{8}'),
                        b'f' => s.push('\u{c}'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            // Surrogate pair handling.
                            if (0xD800..0xDC00).contains(&cp) {
                                if self.peek() == Some(b'\\') {
                                    self.pos += 1;
                                    self.eat(b'u')?;
                                    let lo = self.hex4()?;
                                    let combined = 0x10000
                                        + (((cp - 0xD800) as u32) << 10)
                                        + (lo - 0xDC00) as u32;
                                    s.push(char::from_u32(combined).unwrap_or('\u{fffd}'));
                                } else {
                                    s.push('\u{fffd}');
                                }
                            } else {
                                s.push(char::from_u32(cp as u32).unwrap_or('\u{fffd}'));
                            }
                        }
                        _ => return Err(format!("bad escape at byte {}", self.pos)),
                    }
                }
                _ => {
                    // Collect raw UTF-8 bytes for this codepoint.
                    let start = self.pos - 1;
                    let len = utf8_len(c);
                    self.pos = start + len;
                    if self.pos > self.b.len() {
                        return Err("truncated utf-8".into());
                    }
                    s.push_str(
                        std::str::from_utf8(&self.b[start..self.pos]).map_err(|e| e.to_string())?,
                    );
                }
            }
        }
        Ok(s)
    }

    fn hex4(&mut self) -> Result<u16, String> {
        if self.pos + 4 > self.b.len() {
            return Err("truncated \\u escape".into());
        }
        let hex =
            std::str::from_utf8(&self.b[self.pos..self.pos + 4]).map_err(|e| e.to_string())?;
        self.pos += 4;
        u16::from_str_radix(hex, 16).map_err(|e| e.to_string())
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') || self.peek() == Some(b'+') {
            self.pos += 1;
        }
        let mut float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'-' | b'+' => {
                    float = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.b[start..self.pos]).map_err(|e| e.to_string())?;
        if text.is_empty() {
            return Err(format!("expected a value at byte {}", start));
        }
        if float {
            text.parse::<f64>()
                .map(Json::Float)
                .map_err(|e| e.to_string())
        } else {
            match text.parse::<i64>() {
                Ok(i) => Ok(Json::Int(i)),
                Err(_) => text
                    .parse::<f64>()
                    .map(Json::Float)
                    .map_err(|e| e.to_string()),
            }
        }
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}
