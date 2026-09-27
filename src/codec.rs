//! Little binary codec used by the on-disk log. Varints for ids and lengths,
//! CRC32 (IEEE) for record integrity.

use crate::value::Value;

pub fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

pub fn put_ivarint(out: &mut Vec<u8>, v: i64) {
    // Zigzag so small negatives stay small.
    put_varint(out, ((v << 1) ^ (v >> 63)) as u64);
}

pub fn put_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

pub fn put_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => put_u8(out, 0),
        Value::Bool(b) => {
            put_u8(out, 1);
            put_u8(out, *b as u8);
        }
        Value::Int(i) => {
            put_u8(out, 2);
            put_ivarint(out, *i);
        }
        Value::Float(f) => {
            put_u8(out, 3);
            put_f64(out, *f);
        }
        Value::Text(t) => {
            put_u8(out, 4);
            put_str(out, t);
        }
        Value::List(items) => {
            put_u8(out, 5);
            put_varint(out, items.len() as u64);
            for i in items {
                put_value(out, i);
            }
        }
    }
}

pub struct Reader<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn u8(&mut self) -> Result<u8, String> {
        let v = *self.buf.get(self.pos).ok_or("unexpected end of record")?;
        self.pos += 1;
        Ok(v)
    }

    pub fn u32(&mut self) -> Result<u32, String> {
        if self.remaining() < 4 {
            return Err("unexpected end of record".into());
        }
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.buf[self.pos..self.pos + 4]);
        self.pos += 4;
        Ok(u32::from_le_bytes(b))
    }

    pub fn varint(&mut self) -> Result<u64, String> {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift > 63 {
                return Err("varint overflow".into());
            }
        }
    }

    pub fn ivarint(&mut self) -> Result<i64, String> {
        let v = self.varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    pub fn f64(&mut self) -> Result<f64, String> {
        if self.remaining() < 8 {
            return Err("unexpected end of record".into());
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.buf[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(f64::from_le_bytes(b))
    }

    pub fn string(&mut self) -> Result<String, String> {
        let len = self.varint()? as usize;
        if self.remaining() < len {
            return Err("unexpected end of string".into());
        }
        let s = std::str::from_utf8(&self.buf[self.pos..self.pos + len])
            .map_err(|_| "invalid utf-8 in record".to_string())?
            .to_string();
        self.pos += len;
        Ok(s)
    }

    pub fn value(&mut self) -> Result<Value, String> {
        match self.u8()? {
            0 => Ok(Value::Null),
            1 => Ok(Value::Bool(self.u8()? != 0)),
            2 => Ok(Value::Int(self.ivarint()?)),
            3 => Ok(Value::Float(self.f64()?)),
            4 => Ok(Value::Text(self.string()?)),
            5 => {
                let n = self.varint()? as usize;
                if n > self.remaining() + 1 {
                    return Err("list length exceeds record".into());
                }
                let mut items = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    items.push(self.value()?);
                }
                Ok(Value::List(items))
            }
            other => Err(format!("unknown value tag {}", other)),
        }
    }

    fn advance(&mut self, n: usize) -> Result<(), String> {
        if self.remaining() < n {
            return Err("unexpected end of record".into());
        }
        self.pos += n;
        Ok(())
    }

    /// Step over a string without allocating it. Checks UTF-8 so that a
    /// record which validates is guaranteed to decode.
    pub fn skip_str(&mut self) -> Result<(), String> {
        let len = self.varint()? as usize;
        if self.remaining() < len {
            return Err("unexpected end of string".into());
        }
        std::str::from_utf8(&self.buf[self.pos..self.pos + len])
            .map_err(|_| "invalid utf-8 in record".to_string())?;
        self.pos += len;
        Ok(())
    }

    /// Step over a value without checking string contents. For data whose
    /// integrity is already established — a checksummed image — where
    /// re-validating UTF-8 in every skipped text would dominate a lookup.
    pub fn skip_value_trusted(&mut self) -> Result<(), String> {
        match self.u8()? {
            0 => Ok(()),
            1 => self.advance(1),
            2 => self.ivarint().map(|_| ()),
            3 => self.advance(8),
            4 => {
                let len = self.varint()? as usize;
                self.advance(len)
            }
            5 => {
                let n = self.varint()? as usize;
                if n > self.remaining() + 1 {
                    return Err("list length exceeds record".into());
                }
                for _ in 0..n {
                    self.skip_value_trusted()?;
                }
                Ok(())
            }
            other => Err(format!("unknown value tag {}", other)),
        }
    }

    /// Step over a value without allocating it. Accepts exactly what
    /// `value` accepts.
    pub fn skip_value(&mut self) -> Result<(), String> {
        match self.u8()? {
            0 => Ok(()),
            1 => self.advance(1),
            2 => self.ivarint().map(|_| ()),
            3 => self.advance(8),
            4 => self.skip_str(),
            5 => {
                let n = self.varint()? as usize;
                if n > self.remaining() + 1 {
                    return Err("list length exceeds record".into());
                }
                for _ in 0..n {
                    self.skip_value()?;
                }
                Ok(())
            }
            other => Err(format!("unknown value tag {}", other)),
        }
    }
}

// ------------------------------------------------ interned property lists
//
// The snapshot image stores each node's and edge's properties as one run:
// `varint count, then (varint key_id, value)*`, with keys as interned string
// ids. These read and write that run.

pub fn put_prop_ids(out: &mut Vec<u8>, props: &[(u32, Value)]) {
    put_varint(out, props.len() as u64);
    for (k, v) in props {
        put_varint(out, *k as u64);
        put_value(out, v);
    }
}

pub fn read_prop_ids(buf: &[u8]) -> Result<Vec<(u32, Value)>, String> {
    if buf.is_empty() {
        return Ok(Vec::new());
    }
    let mut r = Reader::new(buf);
    let n = r.varint()? as usize;
    if n > r.remaining() + 1 {
        return Err("property count exceeds run".into());
    }
    let mut props = Vec::with_capacity(n);
    for _ in 0..n {
        let k = r.varint()?;
        props.push((k as u32, r.value()?));
    }
    Ok(props)
}

/// One property out of a run, decoding only the value that matches. The
/// others are stepped over without allocating. Runs come from checksummed
/// images, so skipped strings are not re-validated; the matching value is
/// decoded, and so checked, in full.
pub fn find_prop(buf: &[u8], key: u32) -> Result<Option<Value>, String> {
    if buf.is_empty() {
        return Ok(None);
    }
    let mut r = Reader::new(buf);
    let n = r.varint()?;
    for _ in 0..n {
        let k = r.varint()?;
        if k == key as u64 {
            return r.value().map(Some);
        }
        r.skip_value_trusted()?;
    }
    Ok(None)
}

// ------------------------------------------------------------------- CRC32

/// Slicing-by-8 tables: `T[0]` is the classic byte-at-a-time table, `T[k]` is
/// the CRC of a byte followed by `k` zero bytes. Eight lookups per eight input
/// bytes instead of eight dependent ones — about 4x the throughput, which
/// matters now that opening a snapshot image is mostly checksumming.
static CRC_TABLES: [[u32; 256]; 8] = build_crc_tables();

const fn build_crc_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        t[0][i] = crc;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[k - 1][i];
            t[k][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            i += 1;
        }
        k += 1;
    }
    t
}

/// A running CRC32 (IEEE), for checksumming data that arrives in pieces.
#[derive(Clone, Copy)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Crc32::new()
    }
}

impl Crc32 {
    pub fn new() -> Crc32 {
        Crc32(0xFFFF_FFFF)
    }

    pub fn update(&mut self, data: &[u8]) {
        let t = &CRC_TABLES;
        let mut crc = self.0;
        let mut chunks = data.chunks_exact(8);
        for c in &mut chunks {
            let lo = crc ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            crc = t[7][(lo & 0xff) as usize]
                ^ t[6][((lo >> 8) & 0xff) as usize]
                ^ t[5][((lo >> 16) & 0xff) as usize]
                ^ t[4][(lo >> 24) as usize]
                ^ t[3][c[4] as usize]
                ^ t[2][c[5] as usize]
                ^ t[1][c[6] as usize]
                ^ t[0][c[7] as usize];
        }
        for &b in chunks.remainder() {
            crc = t[0][((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        self.0 = crc;
    }

    pub fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte-at-a-time definition, kept as the reference the fast path
    /// must agree with.
    fn crc32_slow(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc = CRC_TABLES[0][((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }

    #[test]
    fn slicing_by_8_matches_the_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        let mut x = 0x1234_5678u32;
        let data: Vec<u8> = (0..4099)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        for len in [0, 1, 7, 8, 9, 63, 64, 65, 1000, 4099] {
            assert_eq!(crc32(&data[..len]), crc32_slow(&data[..len]), "len {len}");
        }
        // Streaming in odd pieces gives the same answer as one call.
        let mut c = Crc32::new();
        for piece in data.chunks(13) {
            c.update(piece);
        }
        assert_eq!(c.finish(), crc32(&data));
    }

    #[test]
    fn find_prop_and_skip_agree_with_decode() {
        let props = vec![
            (3u32, Value::Text("x".into())),
            (
                7,
                Value::List(vec![Value::Int(-1), Value::Float(2.5), Value::Null]),
            ),
            (9, Value::Bool(true)),
        ];
        let mut buf = Vec::new();
        put_prop_ids(&mut buf, &props);
        assert_eq!(find_prop(&buf, 7).unwrap(), Some(props[1].1.clone()));
        assert_eq!(find_prop(&buf, 9).unwrap(), Some(Value::Bool(true)));
        assert_eq!(find_prop(&buf, 4).unwrap(), None);
        let back = read_prop_ids(&buf).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[1].1, props[1].1);
    }
}
