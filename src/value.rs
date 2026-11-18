//! The property value type, plus a small self-contained JSON codec.
//!
//! A list may hold lists, and a statement, a parameter, an import or a
//! damaged page can say how deeply. Everything that walks a value — compare,
//! clone, drop, render, encode, decode, the BEAM binding — follows that
//! nesting down by recursion, so the depth is bounded once for all of them:
//! [`MAX_DEPTH`] levels, checked wherever values come in and refused past
//! it, never truncated. The readers that see input first (the JSON parser
//! here, the record codec) hold what they are still filling on an explicit
//! stack rather than recursing, so a document nested a million deep costs
//! them a vector, not the thread's stack, and they say no at the limit.

use std::cmp::Ordering;
use std::fmt;

/// How deeply anything glider reads may nest: lists within lists, and in a
/// statement, expressions within expressions (`EXPLAIN` within `EXPLAIN`
/// too), and the hops of one pattern chain. One limit, the same in the
/// parser, the evaluator, the codecs and the bindings, so a value that one
/// of them accepts the others can carry.
///
/// It exists because each of those walks a nested structure by recursion,
/// and a thread's stack is not something an embedded database gets to
/// choose: a BEAM dirty scheduler, an iOS secondary thread and wasm all
/// give it a few hundred kilobytes. Sixty-four levels of the deepest of
/// them fit in a small part of that even in a debug build. Structures that
/// are long rather than deep — `AND` chains, long lists, many patterns —
/// do not nest and are bounded by size and memory instead.
pub const MAX_DEPTH: usize = 64;

/// How deeply a JSON document may nest. Deeper than [`MAX_DEPTH`] because a
/// document wraps its values: an export line holds a 64-deep list two
/// objects down, and nested objects become text, which has no depth. The
/// parser is iterative, so this bounds only what the parsed tree costs to
/// walk (and drop) afterwards.
pub const MAX_JSON_DEPTH: usize = 200;

/// The error for something nested past [`MAX_DEPTH`], naming the construct:
/// `list nested deeper than 64 levels`.
pub fn too_deep(what: &str) -> String {
    format!("{what} nested deeper than {MAX_DEPTH} levels")
}

/// How many lists deep `v` is: 0 for a scalar, 1 for `[1]`. Iterative, so
/// it can be asked of a value however deep. Stops counting past
/// [`MAX_DEPTH`] + 1, which is all any caller needs to know.
pub fn depth(v: &Value) -> usize {
    let Value::List(items) = v else {
        return 0;
    };
    // The lists open around the current position, innermost last.
    let mut open = vec![items.iter()];
    let mut deepest = 1;
    while let Some(items) = open.last_mut() {
        match items.next() {
            Some(Value::List(inner)) => {
                if open.len() > MAX_DEPTH {
                    return MAX_DEPTH + 1;
                }
                open.push(inner.iter());
                deepest = deepest.max(open.len());
            }
            Some(_) => {}
            None => {
                open.pop();
            }
        }
    }
    deepest
}

/// `Err(too_deep("list"))` when `v` holds lists nested past [`MAX_DEPTH`].
/// It is what stands between a parameter, a value written through the API
/// or a result handed out, and everything below that recurses.
pub fn check_depth(v: &Value) -> Result<(), String> {
    if depth(v) > MAX_DEPTH {
        return Err(too_deep("list"));
    }
    Ok(())
}

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
    /// Conservative retained heap cost, including spare capacity and headers.
    /// Iterative: it is asked of parameters before their depth is known.
    pub fn memory_bytes(&self) -> u64 {
        let size = std::mem::size_of::<Value>() as u64;
        let items = match self {
            Value::Text(s) => return size + s.capacity() as u64,
            Value::List(v) => v,
            _ => return size,
        };
        let mut total = size;
        let mut open = vec![items];
        while let Some(items) = open.pop() {
            total += items.capacity() as u64 * size;
            for v in items {
                match v {
                    Value::Text(s) => total += s.capacity() as u64,
                    Value::List(inner) => open.push(inner),
                    _ => {}
                }
            }
        }
        total
    }

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

    /// The value as JSON. Iterative: the lists still open are a stack of
    /// iterators, so rendering costs no thread stack however deep the
    /// value is.
    pub fn write_json(&self, out: &mut String) {
        let mut open: Vec<std::slice::Iter<'_, Value>> = Vec::new();
        let mut next = Some(self);
        loop {
            if let Some(v) = next.take() {
                match v {
                    Value::Null => out.push_str("null"),
                    Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                    Value::Int(i) => out.push_str(&i.to_string()),
                    Value::Float(f) if f.is_finite() => write_float(*f, out),
                    Value::Float(_) => out.push_str("null"),
                    Value::Text(t) => write_json_string(t, out),
                    Value::List(l) => {
                        out.push('[');
                        let mut items = l.iter();
                        next = items.next();
                        open.push(items);
                        continue;
                    }
                }
            }
            // A value is done: the next item of the innermost open list, or
            // close it.
            let Some(items) = open.last_mut() else {
                return;
            };
            match items.next() {
                Some(v) => {
                    out.push(',');
                    next = Some(v);
                }
                None => {
                    out.push(']');
                    open.pop();
                }
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

/// A float as JSON that reads back as a float: a whole one keeps its `.0`
/// (`3.0`, not `3`), or it would come back an integer.
fn write_float(f: f64, out: &mut String) {
    let text = f.to_string();
    out.push_str(&text);
    if !text.contains(['.', 'e', 'E']) {
        out.push_str(".0");
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
    /// property model is deliberately flat, and so does a list nested past
    /// [`MAX_DEPTH`]: this never fails. Input that is going into a database
    /// uses [`Json::try_value`], which refuses such a list instead.
    pub fn to_value(&self) -> Value {
        self.value_at(1, false).unwrap_or(Value::Null)
    }

    /// As [`Json::to_value`], but a list nested deeper than [`MAX_DEPTH`]
    /// is an error (`list nested deeper than 64 levels`), not text.
    pub fn try_value(&self) -> Result<Value, String> {
        self.value_at(1, true)
    }

    /// `depth` is the list depth this value would sit at. Recursion stops at
    /// the limit (past it a list is text or an error), so it is bounded
    /// however deep the document.
    fn value_at(&self, depth: usize, strict: bool) -> Result<Value, String> {
        Ok(match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Int(i) => Value::Int(*i),
            Json::Float(f) => Value::Float(*f),
            Json::Text(t) => Value::Text(t.clone()),
            Json::Array(_) if depth > MAX_DEPTH && strict => return Err(too_deep("list")),
            Json::Array(_) if depth > MAX_DEPTH => Value::Text(json_to_string(self)),
            Json::Array(a) => {
                let mut items = Vec::with_capacity(a.len());
                for j in a {
                    items.push(j.value_at(depth + 1, strict)?);
                }
                Value::List(items)
            }
            Json::Object(_) => Value::Text(json_to_string(self)),
        })
    }
}

pub fn json_to_string(j: &Json) -> String {
    let mut out = String::new();
    write_json(j, &mut out);
    out
}

/// A container being written: the rest of its items.
enum Writing<'a> {
    Array(std::slice::Iter<'a, Json>),
    Object(std::slice::Iter<'a, (String, Json)>),
}

/// The document as JSON, iteratively, as [`Value::write_json`].
fn write_json(j: &Json, out: &mut String) {
    let mut open: Vec<Writing<'_>> = Vec::new();
    let mut next = Some(j);
    loop {
        if let Some(j) = next.take() {
            match j {
                Json::Null => out.push_str("null"),
                Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                Json::Int(i) => out.push_str(&i.to_string()),
                Json::Float(f) if f.is_finite() => write_float(*f, out),
                Json::Float(_) => out.push_str("null"),
                Json::Text(t) => write_json_string(t, out),
                Json::Array(a) => {
                    out.push('[');
                    let mut items = a.iter();
                    next = items.next();
                    open.push(Writing::Array(items));
                    continue;
                }
                Json::Object(fields) => {
                    out.push('{');
                    let mut items = fields.iter();
                    if let Some((k, v)) = items.next() {
                        write_json_string(k, out);
                        out.push(':');
                        next = Some(v);
                    }
                    open.push(Writing::Object(items));
                    continue;
                }
            }
        }
        match open.last_mut() {
            None => return,
            Some(Writing::Array(items)) => match items.next() {
                Some(v) => {
                    out.push(',');
                    next = Some(v);
                }
                None => {
                    out.push(']');
                    open.pop();
                }
            },
            Some(Writing::Object(items)) => match items.next() {
                Some((k, v)) => {
                    out.push(',');
                    write_json_string(k, out);
                    out.push(':');
                    next = Some(v);
                }
                None => {
                    out.push('}');
                    open.pop();
                }
            },
        }
    }
}

pub fn parse_json(src: &str) -> Result<Json, String> {
    let b = src.as_bytes();
    let mut p = JsonParser { b, pos: 0 };
    p.ws();
    let v = p.document()?;
    p.ws();
    if p.pos != b.len() {
        return Err(format!("trailing input at byte {}", p.pos));
    }
    Ok(v)
}

struct JsonParser<'a> {
    b: &'a [u8],
    pos: usize,
}

/// A container still being filled: an array's items so far, or an
/// object's fields and the key whose value comes next.
enum Filling {
    Array(Vec<Json>),
    Object(Vec<(String, Json)>, String),
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

    /// One whole value. Containers are opened onto `open` and closed off
    /// it, so nesting costs heap, not stack; past [`MAX_JSON_DEPTH`] open
    /// containers the document is refused.
    fn document(&mut self) -> Result<Json, String> {
        let mut open: Vec<Filling> = Vec::new();
        loop {
            // Here a value starts: a scalar completes at once, a container
            // opens (or, if empty, completes).
            let mut done = match self.peek() {
                None => return Err("unexpected end of json".into()),
                Some(c @ (b'[' | b'{')) => {
                    if open.len() >= MAX_JSON_DEPTH {
                        return Err(format!("json nested deeper than {MAX_JSON_DEPTH} levels"));
                    }
                    self.pos += 1;
                    self.ws();
                    if c == b'[' {
                        if self.peek() == Some(b']') {
                            self.pos += 1;
                            Json::Array(Vec::new())
                        } else {
                            open.push(Filling::Array(Vec::new()));
                            continue;
                        }
                    } else if self.peek() == Some(b'}') {
                        self.pos += 1;
                        Json::Object(Vec::new())
                    } else {
                        let key = self.key()?;
                        open.push(Filling::Object(Vec::new(), key));
                        continue;
                    }
                }
                Some(b'"') => Json::Text(self.string()?),
                Some(b't') => self.lit("true", Json::Bool(true))?,
                Some(b'f') => self.lit("false", Json::Bool(false))?,
                Some(b'n') => self.lit("null", Json::Null)?,
                Some(_) => self.number()?,
            };
            // A value is complete: it goes into the innermost open
            // container, which then takes its next item or closes, and a
            // closed container is itself a completed value.
            loop {
                let Some(mut top) = open.pop() else {
                    return Ok(done);
                };
                self.ws();
                let closed = match &mut top {
                    Filling::Array(items) => {
                        items.push(done);
                        match self.peek() {
                            Some(b',') => {
                                self.pos += 1;
                                self.ws();
                                false
                            }
                            Some(b']') => {
                                self.pos += 1;
                                true
                            }
                            _ => return Err(format!("expected ',' or ']' at byte {}", self.pos)),
                        }
                    }
                    Filling::Object(fields, key) => {
                        fields.push((std::mem::take(key), done));
                        match self.peek() {
                            Some(b',') => {
                                self.pos += 1;
                                *key = self.key()?;
                                false
                            }
                            Some(b'}') => {
                                self.pos += 1;
                                true
                            }
                            _ => return Err(format!("expected ',' or '}}' at byte {}", self.pos)),
                        }
                    }
                };
                if !closed {
                    open.push(top);
                    break;
                }
                done = match top {
                    Filling::Array(items) => Json::Array(items),
                    Filling::Object(fields, _) => Json::Object(fields),
                };
            }
        }
    }

    /// An object key and its colon, leaving the position at its value.
    fn key(&mut self) -> Result<String, String> {
        self.ws();
        let k = self.string()?;
        self.ws();
        self.eat(b':')?;
        self.ws();
        Ok(k)
    }

    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(format!("bad literal at byte {}", self.pos))
        }
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
