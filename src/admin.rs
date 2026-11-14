//! The admin token (`WFMT_ADMIN_TOKEN`) guarding everything that changes what the server does or
//! holds: starting or cancelling a scrape, exporting and uploading. Without it these stay off.

use std::time::Duration;

use dioxus::fullstack::axum::http::StatusCode;
use tokio::sync::Mutex;

/// Wrong tokens are answered one at a time, this slowly, which limits guessing.
const WRONG_TOKEN_DELAY: Duration = Duration::from_secs(1);

/// Checks admin tokens against the server's.
pub struct Admin {
    token: Option<String>,
    /// Held while a wrong token is answered.
    wrong_tokens: Mutex<()>,
}

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

impl From<Denied> for dioxus::prelude::ServerFnError {
    fn from(denied: Denied) -> Self {
        Self::ServerError {
            message: denied.message().into(),
            code: denied.status().as_u16(),
            details: None,
        }
    }
}

/// Takes the same time wherever the tokens differ, so timing doesn't give the token away.
fn token_matches(expected: &str, given: &str) -> bool {
    use subtle::ConstantTimeEq;
    expected.as_bytes().ct_eq(given.as_bytes()).into()
}

impl Admin {
    /// `token` is `WFMT_ADMIN_TOKEN`, see `Config`.
    pub fn new(token: Option<String>) -> Self {
        Self {
            token,
            wrong_tokens: Mutex::new(()),
        }
    }

    /// The server has a token, so the actions it guards are possible at all.
    pub fn enabled(&self) -> bool {
        self.token.is_some()
    }

    pub async fn check(&self, given: &str) -> Result<(), Denied> {
        let expected = self.token.as_deref().ok_or(Denied::NoTokenSet)?;
        if token_matches(expected, given) {
            return Ok(());
        }
        let _one_at_a_time = self.wrong_tokens.lock().await;
        tokio::time::sleep(WRONG_TOKEN_DELAY).await;
        Err(Denied::WrongToken)
    }
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
