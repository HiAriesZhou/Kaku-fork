//! Registration tokens: who may register a Mac with this relay.
//!
//! A token is `base64url(claims JSON).base64url(Ed25519 signature)`. The
//! relay holds only the public key (`KAKU_RELAY_TOKEN_KEY`) and checks a
//! token offline, with no database: the signature, the expiry, and that it
//! names this Mac's id or any (`*`). Tokens are short-lived and renewed by
//! whoever issued them, so a refund or an ended invite simply stops renewal.
//! `kaku-relay issue` signs one with the private key from the environment
//! (`KAKU_RELAY_SIGNING_KEY`); the key is never written to disk here.

use base64::Engine;
use ring::signature::{self, Ed25519KeyPair, KeyPair};

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .ok()
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Claims {
    /// The Mac's relay id, or `*` for any Mac (an invite).
    pub sub: String,
    /// What it grants: `invite` (beta), later `trial` or `pro`.
    pub kind: String,
    /// Unix seconds after which it no longer registers.
    pub exp: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Invalid {
    Malformed,
    Signature,
    Expired,
    OtherMac,
}

/// Check `token` for the Mac `host_id` at `now`.
pub fn verify(token: &str, public_key: &[u8], host_id: &str, now: u64) -> Result<Claims, Invalid> {
    let (body, sig) = token.split_once('.').ok_or(Invalid::Malformed)?;
    let body = unb64(body).ok_or(Invalid::Malformed)?;
    let sig = unb64(sig).ok_or(Invalid::Malformed)?;
    signature::UnparsedPublicKey::new(&signature::ED25519, public_key)
        .verify(&body, &sig)
        .map_err(|_| Invalid::Signature)?;
    let claims: Claims = serde_json::from_slice(&body).map_err(|_| Invalid::Malformed)?;
    if claims.exp < now {
        return Err(Invalid::Expired);
    }
    if claims.sub != "*" && claims.sub != host_id {
        return Err(Invalid::OtherMac);
    }
    Ok(claims)
}

/// Sign `claims` with a PKCS#8 Ed25519 key.
pub fn sign(claims: &Claims, pkcs8: &[u8]) -> Result<String, String> {
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|_| "not an Ed25519 PKCS#8 key")?;
    let body = serde_json::to_vec(claims).map_err(|e| e.to_string())?;
    let sig = pair.sign(&body);
    Ok(format!("{}.{}", b64(&body), b64(sig.as_ref())))
}

/// A new key pair: (PKCS#8 private, public), both base64url.
pub fn generate() -> Result<(String, String), String> {
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| "key generation failed")?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|_| "key generation failed")?;
    Ok((b64(pkcs8.as_ref()), b64(pair.public_key().as_ref())))
}

pub fn decode_key(text: &str) -> Option<Vec<u8>> {
    unb64(text.trim())
}

/// `kaku-relay keygen` and `kaku-relay issue`; returns None for anything
/// else, which runs the relay.
pub fn run_command(args: &[String]) -> Option<Result<String, String>> {
    match args.first().map(String::as_str) {
        Some("keygen") => Some(generate().map(|(private, public)| {
            format!(
                "KAKU_RELAY_TOKEN_KEY={public}\nKAKU_RELAY_SIGNING_KEY={private}\n\
                 Set the first on the relay (fly secrets set). Keep the second only where tokens are issued."
            )
        })),
        Some("issue") => Some(issue(&args[1..])),
        _ => None,
    }
}

fn issue(args: &[String]) -> Result<String, String> {
    let mut sub = "*".to_string();
    let mut kind = "invite".to_string();
    let mut days = 30u64;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--host" => sub = value.clone(),
            "--kind" => kind = value.clone(),
            "--days" => days = value.parse().map_err(|_| "--days takes a number")?,
            _ => {
                return Err(format!(
                    "unknown option {flag}; use --host, --kind or --days"
                ))
            }
        }
    }
    let key = std::env::var("KAKU_RELAY_SIGNING_KEY")
        .map_err(|_| "set KAKU_RELAY_SIGNING_KEY (from kaku-relay keygen)")?;
    let pkcs8 = decode_key(&key).ok_or("KAKU_RELAY_SIGNING_KEY is not base64url")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    sign(
        &Claims {
            sub,
            kind,
            exp: now + days * 86400,
        },
        &pkcs8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> (Vec<u8>, Vec<u8>) {
        let (private, public) = generate().unwrap();
        (decode_key(&private).unwrap(), decode_key(&public).unwrap())
    }

    fn claims(sub: &str, exp: u64) -> Claims {
        Claims {
            sub: sub.into(),
            kind: "invite".into(),
            exp,
        }
    }

    #[test]
    fn a_signed_token_registers_its_mac_until_it_expires() {
        let (private, public) = keys();
        let token = sign(&claims("abc", 1000), &private).unwrap();
        assert_eq!(verify(&token, &public, "abc", 999).unwrap().kind, "invite");
        assert_eq!(verify(&token, &public, "abc", 1001), Err(Invalid::Expired));
        assert_eq!(verify(&token, &public, "xyz", 999), Err(Invalid::OtherMac));
        let any = sign(&claims("*", 1000), &private).unwrap();
        assert!(verify(&any, &public, "xyz", 999).is_ok());
    }

    #[test]
    fn a_token_from_another_key_or_altered_does_not() {
        let (private, public) = keys();
        let (other, _) = keys();
        let forged = sign(&claims("*", u64::MAX), &other).unwrap();
        assert_eq!(verify(&forged, &public, "abc", 0), Err(Invalid::Signature));
        // Stretching the expiry breaks the signature.
        let token = sign(&claims("*", 10), &private).unwrap();
        let (_, sig) = token.split_once('.').unwrap();
        let longer = b64(&serde_json::to_vec(&claims("*", u64::MAX)).unwrap());
        assert_eq!(
            verify(&format!("{longer}.{sig}"), &public, "abc", 0),
            Err(Invalid::Signature)
        );
        assert_eq!(
            verify("nonsense", &public, "abc", 0),
            Err(Invalid::Malformed)
        );
    }
}
