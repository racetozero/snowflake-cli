//! AWS Signature Version 4 (`botocore.auth.SigV4Auth`).

use crate::hex;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    m.update(msg.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// The `Authorization` value for a canonical request whose signed header names
/// are `signed`; each caller builds its own canonical request.
pub(crate) fn authorization(
    canonical: &str,
    signed: &str,
    amzdate: &str,
    region: &str,
    service: &str,
    key_id: &str,
    secret: &str,
) -> String {
    let short = &amzdate[..8];
    let scope = format!("{short}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amzdate}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let k = hmac(
        &hmac(
            &hmac(&hmac(format!("AWS4{secret}").as_bytes(), short), region),
            service,
        ),
        "aws4_request",
    );
    let sig = hex(&hmac(&k, &to_sign));
    format!("AWS4-HMAC-SHA256 Credential={key_id}/{scope}, SignedHeaders={signed}, Signature={sig}")
}
