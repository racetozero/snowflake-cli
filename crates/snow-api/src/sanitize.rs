//! `snowflake.cli.api.sanitizers`.

use regex::Regex;
use std::sync::LazyLock;

static ANSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])").expect("valid regex"));

/// Strip ANSI escape sequences.
pub fn for_terminal(text: &str) -> String {
    if !text.contains('\x1b') {
        return text.to_string();
    }
    ANSI.replace_all(text, "").into_owned()
}

/// Strip escapes and prefix `'` to values a spreadsheet would evaluate.
pub fn for_csv(text: &str) -> String {
    let t = for_terminal(text);
    if t.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{t}")
    } else {
        t
    }
}

/// Strip escapes and prompt-unsafe C0 controls (keeps TAB and LF).
pub fn for_prompt(text: &str) -> String {
    for_terminal(text)
        .chars()
        .filter(|c| {
            let u = *c as u32;
            !(u <= 0x08 || (0x0b..=0x1f).contains(&u) || u == 0x7f)
        })
        .collect()
}

/// `snowflake.connector.secret_detector.SecretDetector.mask_secrets`.
pub fn mask_secrets(text: &str) -> String {
    use regex::Regex;
    use std::sync::LazyLock;
    const PUNCT: &str = r##"[a-z0-9!"#\$%&\\'\(\)\*\+,\-\./:;<=>\?@\[\]\^_`\{\|\}~]"##;
    static RES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
        let p = PUNCT;
        vec![
            (Regex::new(r"(?i)(aws_key_id|aws_secret_key|access_key_id|secret_access_key)\s*=\s*'([^']+)'").expect("re"), "${1}='****'"),
            (Regex::new(r"(?i)(sig|signature|AWSAccessKeyId|password|passcode)=(?P<secret>[a-z0-9%/+]{16,})").expect("re"), "${1}=****"),
            (Regex::new(r#"(?i)(accessToken|tempToken|keySecret)"\s*:\s*"([a-z0-9/+]{32,}={0,2})""#).expect("re"), "${1}\":\"XXXX\""),
            (Regex::new(&format!(r#"(?i)(access_token|refresh_token)"\s*:\s*"({p}{{3,}})""#)).expect("re"), "${1}\":\"XXXX\""),
            (Regex::new(r"(?im)-{3,}BEGIN [A-Z ]*PRIVATE KEY-{3,}\n([\s\S]*?)\n-{3,}END [A-Z ]*PRIVATE KEY-{3,}").expect("re"), r"-----BEGIN PRIVATE KEY-----\\nXXXX\\n-----END PRIVATE KEY-----"),
            (Regex::new(r#"(?im)"privateKeyData": "([a-z0-9/+=\\n]{10,})""#).expect("re"), "\"privateKeyData\": \"XXXX\""),
            (Regex::new(r#"(?i)(passcode|otp|pin|otac)(['"\s:=]+)([0-9]{4,6})"#).expect("re"), "${1}${2}****"),
            (Regex::new(&format!(r#"(?i)(oauthClientId|oauthClientSecret|clientSecret)(['"\s:=]+)({p}{{8,}})"#)).expect("re"), "${1}${2}****"),
            (Regex::new(&format!(r#"(?i)(password|pwd)(['"\s:=]+)({p}{{6,}})"#)).expect("re"), "${1}${2}****"),
            (Regex::new(r#"(?i)(token|assertion content)(['"\s:=]+)([a-z0-9=/_\-\+\.:%]{8,})"#).expect("re"), "${1}${2}****"),
        ]
    });
    let mut t = text.to_string();
    for (re, rep) in RES.iter() {
        t = re.replace_all(&t, *rep).into_owned();
    }
    t
}
