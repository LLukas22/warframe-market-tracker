//! The admin token (`WFMT_ADMIN_TOKEN`) guarding everything that changes what the server does or
//! holds: starting or cancelling a scrape, exporting and uploading. Without it these stay off.

use std::time::Duration;

use dioxus::fullstack::axum::http::StatusCode;
use tokio::sync::Mutex;

/// Wrong tokens are answered one at a time, this slowly, which limits guessing.
const WRONG_TOKEN_DELAY: Duration = Duration::from_secs(1);
static WRONG_TOKENS: Mutex<()> = Mutex::const_new(());

#[derive(Debug, PartialEq)]
pub enum Denied {
    NoTokenSet,
    WrongToken,
}

impl Denied {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NoTokenSet => StatusCode::FORBIDDEN,
            Self::WrongToken => StatusCode::UNAUTHORIZED,
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Self::NoTokenSet => "Set an admin token on the server to allow this.",
            Self::WrongToken => "Wrong admin token.",
        }
    }
}

fn admin_token() -> Option<String> {
    std::env::var("WFMT_ADMIN_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
}

pub fn enabled() -> bool {
    admin_token().is_some()
}

fn token_matches(expected: &str, given: &str) -> bool {
    expected.len() == given.len()
        && expected
            .bytes()
            .zip(given.bytes())
            .fold(0, |diff, (a, b)| diff | (a ^ b))
            == 0
}

pub async fn check(given: &str) -> Result<(), Denied> {
    let expected = admin_token().ok_or(Denied::NoTokenSet)?;
    if token_matches(&expected, given) {
        return Ok(());
    }
    let _one_at_a_time = WRONG_TOKENS.lock().await;
    tokio::time::sleep(WRONG_TOKEN_DELAY).await;
    Err(Denied::WrongToken)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_must_match_exactly() {
        assert!(token_matches("secret", "secret"));
        assert!(!token_matches("secret", "secrex"));
        assert!(!token_matches("secret", "secre"));
        assert!(!token_matches("secret", ""));
    }
}
