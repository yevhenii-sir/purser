//! Amazon SNS HTTP(S) deliveries: the envelope and its signature.
//!
//! <https://docs.aws.amazon.com/sns/latest/dg/sns-verify-signature-of-message.html>: the message is
//! signed with RSA over a canonical "Key\nValue\n" string, SHA1 (`SignatureVersion` 1) or SHA256
//! (2), with the key in an X.509 certificate at `SigningCertURL` (an `sns.<region>.amazonaws.com`
//! https URL). The module downloads (and caches) the certificate; everything else is here.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnsMessage {
    /// `Notification` | `SubscriptionConfirmation` | `UnsubscribeConfirmation`.
    pub kind: String,
    pub message_id: String,
    pub topic_arn: String,
    pub subject: Option<String>,
    pub message: String,
    pub timestamp: String,
    pub signature_version: String,
    pub signature: String,
    pub signing_cert_url: String,
    /// Confirmation messages only.
    pub token: String,
    pub subscribe_url: String,
}

pub const NOTIFICATION: &str = "Notification";
pub const SUBSCRIPTION_CONFIRMATION: &str = "SubscriptionConfirmation";
pub const UNSUBSCRIBE_CONFIRMATION: &str = "UnsubscribeConfirmation";

pub fn parse(body: &str) -> Result<SnsMessage, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("bad SNS message: {e}"))?;
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let m = SnsMessage {
        kind: s("Type"),
        message_id: s("MessageId"),
        topic_arn: s("TopicArn"),
        subject: v["Subject"].as_str().map(str::to_string),
        message: s("Message"),
        timestamp: s("Timestamp"),
        signature_version: s("SignatureVersion"),
        signature: s("Signature"),
        signing_cert_url: s("SigningCertURL"),
        token: s("Token"),
        subscribe_url: s("SubscribeURL"),
    };
    if !matches!(m.kind.as_str(), NOTIFICATION | SUBSCRIPTION_CONFIRMATION | UNSUBSCRIBE_CONFIRMATION) {
        return Err(format!("unknown SNS message type '{}'", m.kind));
    }
    Ok(m)
}

impl SnsMessage {
    /// The canonical string the signature is computed over.
    pub fn string_to_sign(&self) -> String {
        let mut fields: Vec<(&str, &str)> = vec![("Message", &self.message), ("MessageId", &self.message_id)];
        if self.kind == NOTIFICATION {
            if let Some(subject) = &self.subject {
                fields.push(("Subject", subject));
            }
            fields.extend([("Timestamp", self.timestamp.as_str()), ("TopicArn", &self.topic_arn), ("Type", &self.kind)]);
        } else {
            fields.extend([
                ("SubscribeURL", self.subscribe_url.as_str()),
                ("Timestamp", &self.timestamp),
                ("Token", &self.token),
                ("TopicArn", &self.topic_arn),
                ("Type", &self.kind),
            ]);
        }
        fields.iter().map(|(k, v)| format!("{k}\n{v}\n")).collect()
    }
}

/// `https://sns.<region>.amazonaws.com[.cn]/<path>.pem` only: anything else could serve a key
/// the sender controls.
pub fn is_valid_cert_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let region = host
        .strip_prefix("sns.")
        .and_then(|h| h.strip_suffix(".amazonaws.com").or_else(|| h.strip_suffix(".amazonaws.com.cn")));
    let region_ok = region.is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    region_ok && path.ends_with(".pem") && !path.contains("..") && !path.contains(['?', '#', '@'])
}

/// Verifies the signature with the PEM certificate downloaded from `signing_cert_url`.
pub fn verify(m: &SnsMessage, cert_pem: &str) -> Result<(), String> {
    use rsa::pkcs8::DecodePublicKey;
    use rsa::{Pkcs1v15Sign, RsaPublicKey};
    use sha2::{Digest, Sha256};

    let der = pem_to_der(cert_pem)?;
    let spki = certificate_spki(&der)?;
    let key = RsaPublicKey::from_public_key_der(spki).map_err(|e| format!("SNS certificate key: {e}"))?;
    let signature = STANDARD.decode(m.signature.trim()).map_err(|_| "SNS signature is not base64".to_string())?;
    let data = m.string_to_sign();
    let (scheme, hash) = match m.signature_version.as_str() {
        "1" => (Pkcs1v15Sign { hash_len: Some(20), prefix: SHA1_PREFIX.into() }, sha1(data.as_bytes()).to_vec()),
        "2" => (Pkcs1v15Sign { hash_len: Some(32), prefix: SHA256_PREFIX.into() }, Sha256::digest(data.as_bytes()).to_vec()),
        v => return Err(format!("unsupported SNS SignatureVersion '{v}'")),
    };
    key.verify(scheme, &hash, &signature).map_err(|_| "SNS signature does not match".to_string())
}

/// DigestInfo prefixes (RFC 8017 9.2 note 1).
const SHA1_PREFIX: &[u8] = &[0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14];
const SHA256_PREFIX: &[u8] =
    &[0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20];

fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let b64: String = pem
        .lines()
        .map(str::trim)
        .skip_while(|l| !l.starts_with("-----BEGIN CERTIFICATE-----"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    if b64.is_empty() {
        return Err("SNS certificate is not a PEM certificate".into());
    }
    STANDARD.decode(b64).map_err(|_| "SNS certificate is not valid base64".into())
}

/// One DER element: (tag, whole element, content, rest).
fn der_element(input: &[u8]) -> Result<(u8, &[u8], &[u8], &[u8]), String> {
    let err = || "malformed DER".to_string();
    let (&tag, rest) = input.split_first().ok_or_else(err)?;
    let (&first, rest) = rest.split_first().ok_or_else(err)?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return Err(err());
        }
        (rest[..n].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize), &rest[n..])
    };
    if rest.len() < len {
        return Err(err());
    }
    let header = input.len() - rest.len();
    Ok((tag, &input[..header + len], &rest[..len], &rest[len..]))
}

/// `SubjectPublicKeyInfo` of an X.509 certificate (RFC 5280 4.1), as DER.
pub fn certificate_spki(der: &[u8]) -> Result<&[u8], String> {
    const SEQUENCE: u8 = 0x30;
    let (tag, _, cert, _) = der_element(der)?;
    let (tbs_tag, _, mut tbs, _) = der_element(cert)?;
    if tag != SEQUENCE || tbs_tag != SEQUENCE {
        return Err("not an X.509 certificate".into());
    }
    // [0] version (optional), serialNumber, signature, issuer, validity, subject, subjectPublicKeyInfo
    let mut index = 0;
    loop {
        let (t, whole, _, rest) = der_element(tbs)?;
        if index == 0 && t == 0xa0 {
            tbs = rest;
            continue;
        }
        if index == 5 {
            return if t == SEQUENCE { Ok(whole) } else { Err("certificate without a public key".into()) };
        }
        index += 1;
        tbs = rest;
    }
}

/// SHA-1 (FIPS 180-4). Only for verifying `SignatureVersion` 1 SNS messages, which AWS still signs
/// with SHA1 unless the topic owner chose version 2.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::RsaPrivateKey;

    #[test]
    fn sha1_vectors() {
        assert_eq!(hex::encode(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(hex::encode(sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(hex::encode(sha1(long)), "84983e441c3bd26ebaae4aa1f95129e5e54670f1");
        assert_eq!(hex::encode(sha1(&vec![b'a'; 1000])), "291e9a6c66994949b57ba5e650361e98fc36b1ba");
    }

    #[test]
    fn certificate_urls() {
        assert!(is_valid_cert_url("https://sns.us-east-1.amazonaws.com/SimpleNotificationService-abc.pem"));
        assert!(is_valid_cert_url("https://sns.cn-north-1.amazonaws.com.cn/SimpleNotificationService-abc.pem"));
        for bad in [
            "http://sns.us-east-1.amazonaws.com/x.pem",
            "https://sns.us-east-1.amazonaws.com.evil.example/x.pem",
            "https://evil.example/sns.us-east-1.amazonaws.com/x.pem",
            "https://sns.us-east-1.amazonaws.com@evil.example/x.pem",
            "https://sns..amazonaws.com/x.pem",
            "https://sns.us-east-1.amazonaws.com/x.txt",
            "https://sns.us-east-1.amazonaws.com:8443/x.pem",
        ] {
            assert!(!is_valid_cert_url(bad), "{bad}");
        }
    }

    fn der(tag: u8, content: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if content.len() < 0x80 {
            out.push(content.len() as u8);
        } else {
            let len = (content.len() as u32).to_be_bytes();
            let skip = len.iter().take_while(|b| **b == 0).count();
            out.push(0x80 | (4 - skip) as u8);
            out.extend_from_slice(&len[skip..]);
        }
        out.extend_from_slice(content);
        out
    }

    /// A structurally valid certificate around `spki` (enough for `certificate_spki`).
    fn fake_certificate_pem(spki: &[u8]) -> String {
        let seq = |parts: &[Vec<u8>]| der(0x30, &parts.concat());
        let tbs = seq(&[der(0xa0, &der(0x02, &[2])), der(0x02, &[1]), seq(&[]), seq(&[]), seq(&[]), seq(&[]), spki.to_vec()]);
        let cert = seq(&[tbs, seq(&[]), der(0x03, &[0])]);
        let b64 = STANDARD.encode(cert);
        let lines: Vec<&str> = b64.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
        format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", lines.join("\n"))
    }

    fn sign(key: &RsaPrivateKey, m: &mut SnsMessage) {
        use sha2::{Digest, Sha256};
        let data = m.string_to_sign();
        let (scheme, hash) = if m.signature_version == "1" {
            (rsa::Pkcs1v15Sign { hash_len: Some(20), prefix: SHA1_PREFIX.into() }, sha1(data.as_bytes()).to_vec())
        } else {
            (rsa::Pkcs1v15Sign { hash_len: Some(32), prefix: SHA256_PREFIX.into() }, Sha256::digest(data.as_bytes()).to_vec())
        };
        m.signature = STANDARD.encode(key.sign(scheme, &hash).unwrap());
    }

    #[test]
    fn signatures_are_verified() {
        let mut rng = rand::thread_rng();
        let key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let spki = key.to_public_key().to_public_key_der().unwrap();
        let pem = fake_certificate_pem(spki.as_bytes());
        assert_eq!(certificate_spki(&pem_to_der(&pem).unwrap()).unwrap(), spki.as_bytes());

        let body = r#"{"Type":"Notification","MessageId":"m-1","TopicArn":"arn:aws:sns:us-east-1:123456789012:rtn",
            "Message":"{\"notificationType\":\"CONSUMABLE_PURCHASED\"}","Timestamp":"2026-11-10T10:00:00.000Z",
            "SignatureVersion":"1","Signature":"","SigningCertURL":"https://sns.us-east-1.amazonaws.com/SimpleNotificationService-x.pem"}"#;
        let mut m = parse(body).unwrap();
        assert_eq!(
            m.string_to_sign(),
            "Message\n{\"notificationType\":\"CONSUMABLE_PURCHASED\"}\nMessageId\nm-1\nTimestamp\n2026-11-10T10:00:00.000Z\nTopicArn\narn:aws:sns:us-east-1:123456789012:rtn\nType\nNotification\n"
        );
        sign(&key, &mut m);
        assert_eq!(verify(&m, &pem), Ok(()));

        let mut tampered = m.clone();
        tampered.message = "{\"notificationType\":\"ENTITLEMENT_PURCHASED\"}".into();
        assert!(verify(&tampered, &pem).is_err());

        let other = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let other_pem = fake_certificate_pem(other.to_public_key().to_public_key_der().unwrap().as_bytes());
        assert!(verify(&m, &other_pem).is_err());

        let mut v2 = m.clone();
        v2.signature_version = "2".into();
        sign(&key, &mut v2);
        assert_eq!(verify(&v2, &pem), Ok(()));
        v2.signature_version = "3".into();
        assert!(verify(&v2, &pem).is_err());
    }

    #[test]
    fn confirmation_string_to_sign() {
        let body = r#"{"Type":"SubscriptionConfirmation","MessageId":"m-2","Token":"tok","TopicArn":"arn",
            "Message":"You have chosen to subscribe","SubscribeURL":"https://sns.us-east-1.amazonaws.com/?Action=ConfirmSubscription",
            "Timestamp":"t","SignatureVersion":"1","Signature":"x","SigningCertURL":"u"}"#;
        let m = parse(body).unwrap();
        assert_eq!(m.token, "tok");
        assert_eq!(
            m.string_to_sign(),
            "Message\nYou have chosen to subscribe\nMessageId\nm-2\nSubscribeURL\nhttps://sns.us-east-1.amazonaws.com/?Action=ConfirmSubscription\nTimestamp\nt\nToken\ntok\nTopicArn\narn\nType\nSubscriptionConfirmation\n"
        );
        assert!(parse(r#"{"Type":"Other"}"#).is_err());
    }
}
