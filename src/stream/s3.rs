//! An S3 client: SigV4 signing, PUT/GET/DELETE, and paginated ListObjectsV2.
//!
//! Written against the SigV4 spec rather than an SDK, which is a smaller
//! surface than it sounds: a canonical request, a string to sign, four nested
//! HMACs, one header. Works with anything S3-compatible — MinIO, Ceph RGW,
//! SeaweedFS, Garage, LocalStack — over a plaintext endpoint.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::hash::{hex, hmac_sha256, sha256};
use super::http::{self, uri_encode, Request};

#[derive(Clone, Debug)]
pub struct S3 {
    pub bucket: String,
    /// Key prefix inside the bucket, no leading or trailing slash.
    pub prefix: String,
    pub region: String,
    pub host: String,
    pub port: u16,
    pub access_key: String,
    pub secret_key: String,
    /// MinIO and friends use `bucket/key`; AWS uses `bucket.s3.amazonaws.com`.
    pub path_style: bool,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Object {
    pub key: String,
    pub size: u64,
    pub modified: String,
}

impl S3 {
    fn key_path(&self, key: &str) -> String {
        let full = if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.prefix, key)
        };
        if self.path_style {
            format!("/{}/{}", self.bucket, full)
        } else {
            format!("/{full}")
        }
    }

    pub fn put(&self, key: &str, body: Vec<u8>) -> std::io::Result<()> {
        let resp = self.call("PUT", &self.key_path(key), &[], body)?;
        self.check(resp, "PUT", key)
    }

    pub fn get(&self, key: &str) -> std::io::Result<Vec<u8>> {
        let resp = self.call("GET", &self.key_path(key), &[], Vec::new())?;
        if resp.status == 404 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no such object: {key}"),
            ));
        }
        if !resp.ok() {
            return Err(std::io::Error::other(format!(
                "GET {key} failed: {} {}",
                resp.status,
                resp.text()
            )));
        }
        Ok(resp.body)
    }

    pub fn delete(&self, key: &str) -> std::io::Result<()> {
        let resp = self.call("DELETE", &self.key_path(key), &[], Vec::new())?;
        if resp.status == 404 {
            return Ok(());
        }
        self.check(resp, "DELETE", key)
    }

    /// Every object under `prefix`, following continuation tokens.
    pub fn list(&self, prefix: &str) -> std::io::Result<Vec<Object>> {
        let full_prefix = if self.prefix.is_empty() {
            prefix.to_string()
        } else {
            format!("{}/{}", self.prefix, prefix)
        };
        let path = if self.path_style {
            format!("/{}", self.bucket)
        } else {
            "/".to_string()
        };

        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query: Vec<(String, String)> = vec![
                ("list-type".into(), "2".into()),
                ("prefix".into(), full_prefix.clone()),
                ("max-keys".into(), "1000".into()),
            ];
            if let Some(t) = &token {
                query.push(("continuation-token".into(), t.clone()));
            }
            query.sort();

            let resp = self.call("GET", &path, &query, Vec::new())?;
            if !resp.ok() {
                return Err(std::io::Error::other(format!(
                    "LIST failed: {} {}",
                    resp.status,
                    resp.text()
                )));
            }
            let xml = resp.text();
            for chunk in xml.split("<Contents>").skip(1) {
                let key = tag(chunk, "Key").unwrap_or_default();
                let size = tag(chunk, "Size").and_then(|s| s.parse().ok()).unwrap_or(0);
                let modified = tag(chunk, "LastModified").unwrap_or_default();
                let stripped = if self.prefix.is_empty() {
                    key.clone()
                } else {
                    key.strip_prefix(&format!("{}/", self.prefix))
                        .unwrap_or(&key)
                        .to_string()
                };
                out.push(Object {
                    key: stripped,
                    size,
                    modified,
                });
            }
            if tag(&xml, "IsTruncated").as_deref() == Some("true") {
                token = tag(&xml, "NextContinuationToken");
                if token.is_some() {
                    continue;
                }
            }
            break;
        }
        Ok(out)
    }

    fn check(&self, resp: http::Response, what: &str, key: &str) -> std::io::Result<()> {
        if resp.ok() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "{what} {key} failed: {} {}",
                resp.status,
                resp.text()
            )))
        }
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Vec<u8>,
    ) -> std::io::Result<http::Response> {
        let (date, datetime) = timestamps();
        let payload_hash = hex(&sha256(&body));

        let canonical_query = query
            .iter()
            .map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true)))
            .collect::<Vec<_>>()
            .join("&");

        let canonical_path = uri_encode(path, false);

        // Headers in the signature must be lowercase, trimmed, sorted.
        let host_header = if self.port == 80 || self.port == 443 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        };
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";
        let canonical_headers = format!(
            "host:{host_header}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{datetime}\n"
        );

        let canonical_request = format!(
            "{method}\n{canonical_path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );

        let scope = format!("{date}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{}",
            hex(&sha256(canonical_request.as_bytes()))
        );

        let k_date = hmac_sha256(
            format!("AWS4{}", self.secret_key).as_bytes(),
            date.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, self.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"s3");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hex(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );

        let target = if canonical_query.is_empty() {
            canonical_path
        } else {
            format!("{canonical_path}?{canonical_query}")
        };

        http::send(&Request {
            method,
            host: &self.host,
            port: self.port,
            target,
            headers: vec![
                ("x-amz-date".into(), datetime),
                ("x-amz-content-sha256".into(), payload_hash),
                ("Authorization".into(), authorization),
            ],
            body,
            timeout: self.timeout,
        })
    }
}

fn tag(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// `20260912`, `20260912T205713Z`.
fn timestamps() -> (String, String) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = crate::wal::fmt_unix(secs); // "2026-09-12 20:57:13Z"
    let date = format!("{}{}{}", &stamp[0..4], &stamp[5..7], &stamp[8..10]);
    let time = format!("{}{}{}", &stamp[11..13], &stamp[14..16], &stamp[17..19]);
    (date.clone(), format!("{date}T{time}Z"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from the AWS SigV4 documentation: the signing key
    /// derivation for a known date, region and secret.
    #[test]
    fn signing_key_matches_the_aws_worked_example() {
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), b"20150830");
        let k_region = hmac_sha256(&k_date, b"us-east-1");
        let k_service = hmac_sha256(&k_region, b"iam");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        assert_eq!(
            hex(&k_signing),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn uri_encoding_follows_the_signing_rules() {
        assert_eq!(uri_encode("/a b/c", false), "/a%20b/c");
        assert_eq!(uri_encode("/a b/c", true), "%2Fa%20b%2Fc");
        assert_eq!(uri_encode("a~b_c-d.e", false), "a~b_c-d.e");
        assert_eq!(uri_encode("k=v&x", true), "k%3Dv%26x");
    }

    #[test]
    fn timestamps_are_the_shape_sigv4_wants() {
        let (date, datetime) = timestamps();
        assert_eq!(date.len(), 8);
        assert_eq!(datetime.len(), 16);
        assert!(datetime.ends_with('Z'));
        assert!(datetime.contains('T'));
        assert!(date.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn xml_extraction() {
        let body = "<Contents><Key>a/b.seg</Key><Size>42</Size></Contents>";
        assert_eq!(tag(body, "Key").as_deref(), Some("a/b.seg"));
        assert_eq!(tag(body, "Size").as_deref(), Some("42"));
        assert_eq!(tag(body, "Nope"), None);
    }
}
