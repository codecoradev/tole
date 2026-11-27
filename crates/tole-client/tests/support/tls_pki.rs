//! Throwaway certificates for the HTTPS tests, made in the test itself: no key
//! or certificate is committed and nothing is read from the system. A tiny DER
//! writer builds an X.509 v3 certificate (ECDSA P-256, SHA-256) and `ring`
//! signs it. It is not a general X.509 library: just the fields the verifier
//! of the client looks at (names, validity, the CA flag and the subject
//! alternative names).
#![allow(dead_code)]

use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

/// DER `tag length body`.
fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    match body.len() {
        n if n < 0x80 => out.push(n as u8),
        n if n < 0x100 => out.extend([0x81, n as u8]),
        n => out.extend([0x82, (n >> 8) as u8, n as u8]),
    }
    out.extend_from_slice(body);
    out
}

fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    tlv(0x30, &parts.concat())
}

const OID_ECDSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
const OID_P256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
const OID_COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1D, 0x13];
const OID_SAN: &[u8] = &[0x55, 0x1D, 0x11];

fn name(cn: &str) -> Vec<u8> {
    let attr = seq(&[tlv(0x06, OID_COMMON_NAME), tlv(0x0C, cn.as_bytes())]);
    seq(&[tlv(0x31, &attr)])
}

/// The window a certificate is valid in (`YYMMDDHHMMSSZ`, two-digit years
/// below 50 are 20xx).
#[derive(Clone, Copy)]
pub enum Validity {
    Current,
    Expired,
}

/// One key pair with its certificate.
pub struct Identity {
    pub cert_der: Vec<u8>,
    pub key_pkcs8: Vec<u8>,
    subject: Vec<u8>,
    key: EcdsaKeyPair,
}

/// What goes into a certificate.
pub struct Spec<'a> {
    pub cn: &'a str,
    pub is_ca: bool,
    pub dns: &'a [&'a str],
    pub ips: &'a [[u8; 4]],
    pub validity: Validity,
}

impl<'a> Spec<'a> {
    /// A server certificate for these names, valid now.
    pub fn server(dns: &'a [&'a str], ips: &'a [[u8; 4]]) -> Self {
        Spec {
            cn: "tole test server",
            is_ca: false,
            dns,
            ips,
            validity: Validity::Current,
        }
    }
}

fn generate() -> (Vec<u8>, EcdsaKeyPair) {
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng)
        .expect("generate a key")
        .as_ref()
        .to_vec();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &pkcs8, &rng)
        .expect("load the key");
    (pkcs8, key)
}

/// Issue a certificate for a fresh key. `issuer` signs it; `None` makes it
/// self-signed.
pub fn issue(spec: &Spec<'_>, issuer: Option<&Identity>) -> Identity {
    let (key_pkcs8, key) = generate();
    let subject = name(spec.cn);
    let spki = seq(&[
        seq(&[tlv(0x06, OID_EC_PUBLIC_KEY), tlv(0x06, OID_P256)]),
        tlv(0x03, &[&[0u8][..], key.public_key().as_ref()].concat()),
    ]);
    let (not_before, not_after) = match spec.validity {
        Validity::Current => ("250101000000Z", "490101000000Z"),
        Validity::Expired => ("200101000000Z", "210101000000Z"),
    };
    let mut extensions = Vec::new();
    if spec.is_ca {
        let value = tlv(0x04, &seq(&[tlv(0x01, &[0xFF])]));
        extensions.push(seq(&[
            tlv(0x06, OID_BASIC_CONSTRAINTS),
            tlv(0x01, &[0xFF]),
            value,
        ]));
    }
    if !spec.dns.is_empty() || !spec.ips.is_empty() {
        let mut names: Vec<Vec<u8>> = spec.dns.iter().map(|d| tlv(0x82, d.as_bytes())).collect();
        names.extend(spec.ips.iter().map(|ip| tlv(0x87, ip)));
        extensions.push(seq(&[tlv(0x06, OID_SAN), tlv(0x04, &seq(&names))]));
    }
    let signer = issuer.map_or(&key, |i| &i.key);
    let issuer_name = issuer.map_or(&subject, |i| &i.subject).clone();
    let sig_alg = seq(&[tlv(0x06, OID_ECDSA_SHA256)]);
    let tbs = seq(&[
        tlv(0xA0, &tlv(0x02, &[2])), // version 3
        tlv(0x02, &[1]),             // serial
        sig_alg.clone(),
        issuer_name,
        seq(&[
            tlv(0x17, not_before.as_bytes()),
            tlv(0x17, not_after.as_bytes()),
        ]),
        subject.clone(),
        spki,
        tlv(0xA3, &seq(&extensions)),
    ]);
    let signature = signer
        .sign(&SystemRandom::new(), &tbs)
        .expect("sign the certificate");
    let cert_der = seq(&[
        tbs,
        sig_alg,
        tlv(0x03, &[&[0u8][..], signature.as_ref()].concat()),
    ]);
    Identity {
        cert_der,
        key_pkcs8,
        subject,
        key,
    }
}

/// A CA certificate (self-signed, `CA:TRUE`).
pub fn ca(cn: &str) -> Identity {
    issue(
        &Spec {
            cn,
            is_ca: true,
            dns: &[],
            ips: &[],
            validity: Validity::Current,
        },
        None,
    )
}

/// PEM-encode one certificate.
pub fn pem(der: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::new();
    for chunk in der.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, b| a << 8 | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                b64.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                b64.push('=');
            }
        }
    }
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap());
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out.into_bytes()
}
