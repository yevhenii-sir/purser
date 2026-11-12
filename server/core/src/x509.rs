//! Just enough X.509 / DER to check a certificate chain: the parts of a certificate, its validity,
//! its extensions and its ECDSA signature. Used for the App Store's signed notifications (x5c) and
//! the public key of Amazon SNS signing certificates.

use sha2::{Digest, Sha256, Sha384};

const SEQUENCE: u8 = 0x30;
const BIT_STRING: u8 = 0x03;
const OID: u8 = 0x06;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;

/// `ecdsa-with-SHA256` (1.2.840.10045.4.3.2) and `ecdsa-with-SHA384` (1.2.840.10045.4.3.3).
const ECDSA_SHA256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];
const ECDSA_SHA384: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03];

/// One DER element: (tag, whole element, content, rest).
pub fn element(input: &[u8]) -> Result<(u8, &[u8], &[u8], &[u8]), String> {
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

/// The parts of a certificate a chain check needs (all borrowed from the DER).
#[derive(Debug)]
pub struct Certificate<'a> {
    /// `tbsCertificate`, the signed part (whole element).
    pub tbs: &'a [u8],
    /// `SubjectPublicKeyInfo` (whole element).
    pub spki: &'a [u8],
    pub not_before_ms: i64,
    pub not_after_ms: i64,
    /// `extensions` (content of `[3]`), empty when there are none.
    extensions: &'a [u8],
    /// OID content of `signatureAlgorithm`.
    signature_algorithm: &'a [u8],
    /// DER `ECDSA-Sig-Value` (the BIT STRING without its unused-bits byte).
    signature: &'a [u8],
}

impl<'a> Certificate<'a> {
    pub fn parse(der: &'a [u8]) -> Result<Self, String> {
        let (tag, _, cert, _) = element(der)?;
        let (tbs_tag, tbs, mut fields, rest) = element(cert)?;
        if tag != SEQUENCE || tbs_tag != SEQUENCE {
            return Err("not an X.509 certificate".into());
        }
        let (_, _, alg, rest) = element(rest)?;
        let (_, _, signature_algorithm, _) = element(alg)?;
        let (sig_tag, _, sig, _) = element(rest)?;
        if sig_tag != BIT_STRING || sig.first() != Some(&0) {
            return Err("certificate signature is not a BIT STRING".into());
        }
        // [0] version (optional), serialNumber, signature, issuer, validity, subject,
        // subjectPublicKeyInfo, [1] / [2] unique ids (optional), [3] extensions (optional).
        if fields.first() == Some(&0xa0) {
            fields = element(fields)?.3;
        }
        let mut parts = Vec::new();
        while !fields.is_empty() {
            let (t, whole, content, rest) = element(fields)?;
            parts.push((t, whole, content));
            fields = rest;
        }
        if parts.len() < 6 {
            return Err("certificate without a public key".into());
        }
        let (validity_tag, _, validity) = parts[3];
        let (spki_tag, spki, _) = parts[5];
        if validity_tag != SEQUENCE || spki_tag != SEQUENCE {
            return Err("malformed certificate".into());
        }
        let (t1, _, before, rest) = element(validity)?;
        let (t2, _, after, _) = element(rest)?;
        let extensions = parts[6..].iter().find(|p| p.0 == 0xa3).map_or(&[][..], |p| p.2);
        Ok(Certificate {
            tbs,
            spki,
            not_before_ms: parse_time(t1, before)?,
            not_after_ms: parse_time(t2, after)?,
            extensions,
            signature_algorithm,
            signature: &sig[1..],
        })
    }

    pub fn is_valid_at(&self, now_ms: i64) -> bool {
        (self.not_before_ms..=self.not_after_ms).contains(&now_ms)
    }

    /// The certificate has an extension with this OID (DER content of the OID).
    pub fn has_extension(&self, oid: &[u8]) -> bool {
        let Ok((_, _, mut list, _)) = element(self.extensions) else { return false };
        while let Ok((_, _, ext, rest)) = element(list) {
            if element(ext).is_ok_and(|(t, _, id, _)| t == OID && id == oid) {
                return true;
            }
            list = rest;
        }
        false
    }

    /// The certificate is signed (ECDSA, SHA-256 or SHA-384) by the key in `issuer_spki`.
    pub fn verify_signed_by(&self, issuer_spki: &[u8]) -> Result<(), String> {
        let digest = match self.signature_algorithm {
            ECDSA_SHA256 => Sha256::digest(self.tbs).to_vec(),
            ECDSA_SHA384 => Sha384::digest(self.tbs).to_vec(),
            _ => return Err("certificate signature is not ECDSA with SHA-256 / SHA-384".into()),
        };
        verify_ecdsa_prehash(issuer_spki, &digest, self.signature, SignatureEncoding::Der)
    }
}

/// How an ECDSA signature is encoded.
#[derive(Clone, Copy)]
pub enum SignatureEncoding {
    /// `ECDSA-Sig-Value` (certificates).
    Der,
    /// `r || s` (JWS).
    Fixed,
}

/// Verifies an ECDSA signature over a digest with a P-256 or P-384 key given as SPKI DER.
pub fn verify_ecdsa_prehash(spki: &[u8], digest: &[u8], sig: &[u8], encoding: SignatureEncoding) -> Result<(), String> {
    use ecdsa::signature::hazmat::PrehashVerifier;
    use p256::pkcs8::DecodePublicKey;
    let bad = || "signature does not match".to_string();
    if let Ok(key) = p256::ecdsa::VerifyingKey::from_public_key_der(spki) {
        let sig = match encoding {
            SignatureEncoding::Der => p256::ecdsa::Signature::from_der(sig),
            SignatureEncoding::Fixed => p256::ecdsa::Signature::from_slice(sig),
        }
        .map_err(|_| bad())?;
        return key.verify_prehash(digest, &sig).map_err(|_| bad());
    }
    if let Ok(key) = p384::ecdsa::VerifyingKey::from_public_key_der(spki) {
        let sig = match encoding {
            SignatureEncoding::Der => p384::ecdsa::Signature::from_der(sig),
            SignatureEncoding::Fixed => p384::ecdsa::Signature::from_slice(sig),
        }
        .map_err(|_| bad())?;
        return key.verify_prehash(digest, &sig).map_err(|_| bad());
    }
    Err("not a P-256 or P-384 public key".into())
}

/// `UTCTime` (`YYMMDDHHMMSSZ`, years 1950-2049) or `GeneralizedTime` (`YYYYMMDDHHMMSSZ`) → Unix ms.
fn parse_time(tag: u8, v: &[u8]) -> Result<i64, String> {
    let s = std::str::from_utf8(v).map_err(|_| "bad certificate time".to_string())?;
    let full = match (tag, s.len()) {
        (UTC_TIME, 13) => {
            let yy: i32 = s[..2].parse().map_err(|_| "bad certificate time".to_string())?;
            format!("{}{}", if yy < 50 { 20 } else { 19 }, s)
        }
        (GENERALIZED_TIME, 15) => s.to_string(),
        _ => return Err("unsupported certificate time".into()),
    };
    let iso = format!("{}-{}-{}T{}:{}:{}Z", &full[..4], &full[4..6], &full[6..8], &full[8..10], &full[10..12], &full[12..14]);
    crate::time::parse_rfc3339_ms(&iso).ok_or_else(|| "bad certificate time".to_string())
}
