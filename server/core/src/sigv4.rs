//! AWS Signature Version 4 (header-based) for S3-compatible object storage: AWS S3, Cloudflare R2,
//! MinIO and others. Pure functions: the caller supplies the time and the payload hash.
//!
//! <https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_sigv-create-signed-request.html>

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// Who signs, and for which region and service (`s3` for object storage).
#[derive(Debug, Clone, Copy)]
pub struct Signer<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
}

/// The parts of a request that are signed. `host` and `x-amz-date` are added to `headers`.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub method: &'a str,
    /// `host[:port]` exactly as the request sends it.
    pub host: &'a str,
    /// The path, not encoded (`/bucket/key`); [`canonical_uri`] encodes it.
    pub path: &'a str,
    /// Query parameters, not encoded.
    pub query: &'a [(&'a str, &'a str)],
    /// Other headers to sign (name, value), e.g. `content-type`, `x-amz-content-sha256`.
    pub headers: &'a [(&'a str, &'a str)],
    /// Lowercase hex SHA-256 of the body ([`payload_hash`]).
    pub payload_hash: &'a str,
}

/// The result of signing, with the intermediate texts (for tests and debugging; none holds the key).
#[derive(Debug, Clone, PartialEq)]
pub struct Signed {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
    /// The `Authorization` header value.
    pub authorization: String,
}

/// Lowercase hex SHA-256 of a body (the empty body: `e3b0c442...`).
pub fn payload_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// `YYYYMMDD'T'HHMMSS'Z'` of a Unix time in milliseconds (the `x-amz-date` header).
pub fn amz_date(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = crate::time::civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", sod / 3600, sod % 3600 / 60, sod % 60)
}

/// AWS URI encoding: unreserved characters (`A-Z a-z 0-9 - _ . ~`) stay, every other byte is `%XX`
/// (uppercase hex); `/` stays when `keep_slash` (paths).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || (keep_slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The path as signed and sent: every byte but `/` URI-encoded once (S3 does not normalize or
/// double-encode paths); an empty path is `/`.
pub fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        "/".into()
    } else {
        uri_encode(path, true)
    }
}

/// Encoded `name=value` pairs sorted by name, then value.
pub fn canonical_query(query: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// Lowercase names, values trimmed with inner runs of spaces collapsed, sorted by name.
fn canonical_headers(req: &Request, amz_date: &str) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = vec![("host".into(), req.host.to_ascii_lowercase()), ("x-amz-date".into(), amz_date.into())];
    for (k, v) in req.headers {
        all.push((k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")));
    }
    all.sort();
    all
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// `kSigning = HMAC(HMAC(HMAC(HMAC("AWS4" + secret, date), region), service), "aws4_request")`.
pub fn signing_key(secret_access_key: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret_access_key}").as_bytes(), date);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    hmac(&k_service, "aws4_request")
}

/// Signs `req` at `amz_date` ([`amz_date`]).
pub fn sign(signer: &Signer, req: &Request, amz_date: &str) -> Signed {
    let headers = canonical_headers(req, amz_date);
    let signed_headers = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let header_block: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let canonical_request = format!(
        "{}\n{}\n{}\n{header_block}\n{signed_headers}\n{}",
        req.method,
        canonical_uri(req.path),
        canonical_query(req.query),
        req.payload_hash
    );
    let date = &amz_date[..amz_date.len().min(8)];
    let scope = format!("{date}/{}/{}/aws4_request", signer.region, signer.service);
    let string_to_sign = format!("{ALGORITHM}\n{amz_date}\n{scope}\n{}", payload_hash(canonical_request.as_bytes()));
    let key = signing_key(signer.secret_access_key, date, signer.region, signer.service);
    let signature = hex::encode(hmac(&key, &string_to_sign));
    let authorization =
        format!("{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}", signer.access_key_id);
    Signed { canonical_request, string_to_sign, signature, authorization }
}

#[cfg(test)]
mod tests {
    use super::*;

    // AWS's published SigV4 test suite (awslabs/aws-c-auth, tests/aws-signing-test-suite/v4): the
    // same credentials, region, service and time in every case.
    const SIGNER: Signer = Signer {
        access_key_id: "AKIDEXAMPLE",
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        region: "us-east-1",
        service: "service",
    };
    const DATE: &str = "20150830T123600Z";
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn get(path: &'static str, query: &'static [(&'static str, &'static str)]) -> Request<'static> {
        Request { method: "GET", host: "example.amazonaws.com", path, query, headers: &[], payload_hash: EMPTY }
    }

    #[test]
    fn get_vanilla() {
        let s = sign(&SIGNER, &get("/", &[]), DATE);
        assert_eq!(s.canonical_request, "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\n".to_string() + EMPTY);
        assert_eq!(
            s.string_to_sign,
            "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/service/aws4_request\nbb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );
        assert_eq!(s.signature, "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
        assert_eq!(
            s.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn get_vanilla_query_order_key_case() {
        let s = sign(&SIGNER, &get("/", &[("Param2", "value2"), ("Param1", "value1")]), DATE);
        assert!(s.canonical_request.starts_with("GET\n/\nParam1=value1&Param2=value2\n"), "{}", s.canonical_request);
        assert_eq!(s.signature, "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500");
    }

    #[test]
    fn get_utf8_path() {
        let s = sign(&SIGNER, &get("/\u{1234}", &[]), DATE);
        assert!(s.canonical_request.starts_with("GET\n/%E1%88%B4\n\n"), "{}", s.canonical_request);
        assert_eq!(s.signature, "8318018e0b0f223aa2bbf98705b62bb787dc9c0e678f255a891fd03141be5d85");
    }

    #[test]
    fn post_x_www_form_urlencoded_with_a_signed_body() {
        let body = "Param1=value1";
        let hash = payload_hash(body.as_bytes());
        assert_eq!(hash, "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e");
        let headers = [("Content-Type", "application/x-www-form-urlencoded"), ("Content-Length", "13"), ("x-amz-content-sha256", hash.as_str())];
        let req = Request { method: "POST", host: "example.amazonaws.com", path: "/", query: &[], headers: &headers, payload_hash: &hash };
        let s = sign(&SIGNER, &req, DATE);
        assert_eq!(
            s.canonical_request,
            format!(
                "POST\n/\n\ncontent-length:13\ncontent-type:application/x-www-form-urlencoded\nhost:example.amazonaws.com\nx-amz-content-sha256:{hash}\nx-amz-date:20150830T123600Z\n\ncontent-length;content-type;host;x-amz-content-sha256;x-amz-date\n{hash}"
            )
        );
        assert_eq!(s.string_to_sign.lines().last(), Some("b1edd1d03544c25390e32085d55b57acc9a3961bb59415ff86c45c3d89d16cfb"));
        assert_eq!(s.signature, "d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b");
        assert!(s.authorization.contains("SignedHeaders=content-length;content-type;host;x-amz-content-sha256;x-amz-date, "));
    }

    #[test]
    fn dates_and_encodings() {
        assert_eq!(amz_date(1_440_938_160_000), DATE);
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(canonical_uri("/bucket/dt=2026-10-05/a b~c.json"), "/bucket/dt%3D2026-10-05/a%20b~c.json");
        assert_eq!(canonical_uri(""), "/");
        assert_eq!(canonical_query(&[("b", "1/2"), ("a", "x y")]), "a=x%20y&b=1%2F2");
    }
}
