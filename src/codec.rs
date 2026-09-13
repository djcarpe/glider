//! Little binary codec used by the on-disk log. Varints for ids and lengths,
//! CRC32 (IEEE) for record integrity.

use crate::value::Value;

pub fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
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
}

// ------------------------------------------------------------------- CRC32

static CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
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
        table[i] = crc;
        i += 1;
    }
    table
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC_TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}
