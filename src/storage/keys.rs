//! Order-preserving key encodings: bytes that sort, compared as plain bytes,
//! exactly as the values they encode.
//!
//! Integers are big-endian. Property values use a tagged encoding whose byte
//! order equals [`Value::total_cmp`]: nulls, then bools, then numbers (Int
//! and Float compared as f64, NaN last), then text, then lists.

use crate::value::Value;

#[inline]
pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[inline]
pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

#[inline]
pub fn get_u64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_be_bytes(a)
}

#[inline]
pub fn get_u32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub fn u64_key(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

const T_NULL: u8 = 0x10;
const T_BOOL: u8 = 0x20;
const T_NUM: u8 = 0x30;
const T_TEXT: u8 = 0x40;
const T_LIST: u8 = 0x50;

/// Append the order-preserving encoding of `v`.
pub fn put_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(T_NULL),
        Value::Bool(b) => {
            out.push(T_BOOL);
            out.push(*b as u8);
        }
        Value::Int(_) | Value::Float(_) => {
            out.push(T_NUM);
            let f = v.as_f64().unwrap_or(f64::NAN);
            let bits = f.to_bits();
            // f64::total_cmp's order, as unsigned bytes.
            let k = if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) };
            out.extend_from_slice(&k.to_be_bytes());
        }
        Value::Text(t) => {
            out.push(T_TEXT);
            // 0x00 is escaped as 00 FF and the string ends with 00 01, so the
            // encoding is prefix-free and sorts like the bytes of the string.
            for &b in t.as_bytes() {
                out.push(b);
                if b == 0 {
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0x00, 0x01]);
        }
        Value::List(items) => {
            out.push(T_LIST);
            for it in items {
                out.push(0x01);
                put_value(out, it);
            }
            out.push(0x00);
        }
    }
}

pub fn value_key(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    put_value(&mut out, v);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn values() -> Vec<Value> {
        let mut v = vec![
            Value::Null,
            Value::Bool(false),
            Value::Bool(true),
            Value::Int(i64::MIN),
            Value::Int(-7),
            Value::Int(0),
            Value::Int(1),
            Value::Int(i64::MAX),
            Value::Float(f64::NEG_INFINITY),
            Value::Float(-2.5),
            Value::Float(-0.0),
            Value::Float(0.0),
            Value::Float(0.5),
            Value::Float(1.0),
            Value::Float(f64::INFINITY),
            Value::Float(f64::NAN),
            Value::Text(String::new()),
            Value::Text("a".into()),
            Value::Text("a\0".into()),
            Value::Text("a\0b".into()),
            Value::Text("ab".into()),
            Value::Text("b".into()),
            Value::Text("é".into()),
            Value::Text("東京".into()),
            Value::List(vec![]),
            Value::List(vec![Value::Null]),
            Value::List(vec![Value::Int(1)]),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::List(vec![Value::Text("a".into())]),
            Value::List(vec![Value::Text("a".into()), Value::Null]),
            Value::List(vec![Value::List(vec![Value::Bool(true)])]),
        ];
        // Pseudo-random extras.
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..300 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.push(match x % 4 {
                0 => Value::Int((x >> 8) as i64 % 1000 - 500),
                1 => Value::Float(((x >> 11) as f64 / (1u64 << 53) as f64) * 200.0 - 100.0),
                2 => Value::Text(format!("{:x}", x % 4096)),
                _ => Value::List(vec![Value::Int((x % 5) as i64), Value::Text(format!("{}", x % 3))]),
            });
        }
        v
    }

    #[test]
    fn byte_order_equals_total_cmp() {
        let vs = values();
        for a in &vs {
            for b in &vs {
                let want = a.total_cmp(b);
                let got = value_key(a).cmp(&value_key(b));
                assert_eq!(got, want, "{a:?} vs {b:?}");
            }
        }
        assert_eq!(
            value_key(&Value::Int(3)).cmp(&value_key(&Value::Float(3.0))),
            Ordering::Equal
        );
    }
}
