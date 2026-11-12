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

/// `arn:aws:sns:<region>:<account id>:<topic name>`.
pub fn is_valid_topic_arn(arn: &str) -> bool {
    let parts: Vec<&str> = arn.split(':').collect();
    parts.len() == 6
        && parts[..3] == ["arn", "aws", "sns"]
        && !parts[3].is_empty()
        && parts[4].len() == 12
        && parts[4].bytes().all(|b| b.is_ascii_digit())
        && !parts[5].is_empty()
        && parts[5].len() <= 256
        && parts[5].bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// How far a message's `Timestamp` may be from now. SNS retries a failed delivery for a while; an
/// older signed message is a replay.
pub const MAX_AGE_MS: i64 = 6 * 3600 * 1000;

impl SnsMessage {
    /// The signed `Timestamp` is within [`MAX_AGE_MS`] of `now_ms`.
    pub fn is_fresh(&self, now_ms: i64) -> bool {
        crate::time::parse_rfc3339_ms(&self.timestamp).is_some_and(|t| (now_ms - t).abs() <= MAX_AGE_MS)
    }

    /// The topic matches the one pinned for the build (`pinned` empty: any topic).
    pub fn is_from(&self, pinned: &str) -> bool {
        pinned.is_empty() || self.topic_arn == pinned
    }

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
    use rsa::RsaPublicKey;

    let der = pem_to_der(cert_pem)?;
    let spki = certificate_spki(&der)?;
    let key = RsaPublicKey::from_public_key_der(spki).map_err(|e| format!("SNS certificate key: {e}"))?;
    let signature = STANDARD.decode(m.signature.trim()).map_err(|_| "SNS signature is not base64".to_string())?;
    let (scheme, hash) = signing_input(m)?;
    key.verify(scheme, &hash, &signature).map_err(|_| "SNS signature does not match".to_string())
}

/// The padding scheme and the digest of the canonical string, by `SignatureVersion`.
fn signing_input(m: &SnsMessage) -> Result<(rsa::Pkcs1v15Sign, Vec<u8>), String> {
    use rsa::Pkcs1v15Sign;
    use sha1::Sha1;
    use sha2::{Digest, Sha256};
    let data = m.string_to_sign();
    match m.signature_version.as_str() {
        "1" => Ok((Pkcs1v15Sign { hash_len: Some(20), prefix: SHA1_DIGEST_INFO.into() }, Sha1::digest(data.as_bytes()).to_vec())),
        "2" => Ok((Pkcs1v15Sign::new::<Sha256>(), Sha256::digest(data.as_bytes()).to_vec())),
        v => Err(format!("unsupported SNS SignatureVersion '{v}'")),
    }
}

/// SHA-1 DigestInfo prefix (RFC 8017 9.2 note 1); `rsa` only builds it for SHA-1 with its `sha1`
/// feature, which would pull a second SHA-1 implementation.
const SHA1_DIGEST_INFO: &[u8] = &[0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14];

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

/// `SubjectPublicKeyInfo` of an X.509 certificate (RFC 5280 4.1), as DER.
pub fn certificate_spki(der: &[u8]) -> Result<&[u8], String> {
    const SEQUENCE: u8 = 0x30;
    let (tag, _, cert, _) = crate::x509::element(der)?;
    let (tbs_tag, _, mut tbs, _) = crate::x509::element(cert)?;
    if tag != SEQUENCE || tbs_tag != SEQUENCE {
        return Err("not an X.509 certificate".into());
    }
    // [0] version (optional), serialNumber, signature, issuer, validity, subject, subjectPublicKeyInfo
    let mut index = 0;
    loop {
        let (t, whole, _, rest) = crate::x509::element(tbs)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::EncodePublicKey;
    use rsa::RsaPrivateKey;

    #[test]
    fn stale_messages_and_other_topics_are_told_apart() {
        let m = SnsMessage { timestamp: "2026-11-10T10:00:00.000Z".into(), topic_arn: "arn:aws:sns:us-east-1:1:rtn".into(), ..Default::default() };
        let at = crate::time::parse_rfc3339_ms("2026-11-10T10:00:00Z").unwrap();
        assert!(m.is_fresh(at + 3600 * 1000) && m.is_fresh(at - 60 * 1000));
        assert!(!m.is_fresh(at + MAX_AGE_MS + 1));
        assert!(!SnsMessage { timestamp: "t".into(), ..m.clone() }.is_fresh(at));
        assert!(m.is_from("") && m.is_from("arn:aws:sns:us-east-1:1:rtn"));
        assert!(!m.is_from("arn:aws:sns:us-east-1:2:rtn"));
        assert!(is_valid_topic_arn("arn:aws:sns:us-east-1:123456789012:rtn-topic"));
        assert!(!is_valid_topic_arn("arn:aws:sns:us-east-1:1:rtn") && !is_valid_topic_arn("arn:aws:sqs:us-east-1:123456789012:q"));
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
        let (scheme, hash) = signing_input(m).unwrap();
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
