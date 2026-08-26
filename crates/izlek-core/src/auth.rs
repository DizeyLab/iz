//! Passwords, tokens and the rules around them.
//!
//! Two different jobs, deliberately kept apart:
//!
//! * **Passwords** are low-entropy and chosen by a person, so they go through
//!   Argon2id with deliberately expensive parameters.
//! * **Tokens** — sign-in links, session cookies, read-only links, the calendar
//!   feed — are 128 bits from a CSPRNG. There is nothing to brute-force, so
//!   they are stored as a plain SHA-256 digest. Running Argon2 over them would
//!   buy nothing and cost a page load.

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::Rng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Argon2id at the OWASP Password Storage Cheat Sheet's recommended second
/// configuration: 19 MiB of memory, two iterations, one lane.
pub const ARGON2_MEMORY_KIB: u32 = 19 * 1024;
pub const ARGON2_ITERATIONS: u32 = 2;
pub const ARGON2_PARALLELISM: u32 = 1;

/// How many bytes of randomness every token carries.
pub const TOKEN_BYTES: usize = 16;

/// A password rule the person's choice broke. The wording is the wording on the
/// first-sign-in screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordProblem {
    #[error("at least 10 characters")]
    TooShort,
    #[error("not your address or your name")]
    LooksLikeYou,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("password hashing failed: {0}")]
    Hashing(String),
}

fn argon2() -> Argon2<'static> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        None,
    )
    .expect("argon2 parameters are constants and are in range");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hashes a password into a PHC string, salt included.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    argon2()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hashing(e.to_string()))
}

/// Checks a password against a stored PHC string.
///
/// The parameters come from the stored hash, not from [`argon2`], so raising
/// the cost later does not lock anyone out.
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => argon2()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A PHC string for a password nobody knows, used to keep the miss path as
/// expensive as the hit path.
///
/// Without this, "no such address" answers before Argon2 would have finished
/// and the response time tells an attacker what the wording refuses to.
pub fn dummy_password_hash() -> &'static str {
    // Minted by the ignored `print_a_dummy_hash` test over 256 bits of
    // randomness that was never written down.
    "$argon2id$v=19$m=19456,t=2,p=1$0JjUMrLBpJG7lzg5bxZhMQ$iGpGXBNDAaHV9jqDxDcCyuIEIV33kJ1IAPt0XCh753Q"
}

/// Burns the same work a real verify would, and always fails.
///
/// Call this on every path where a lookup missed, before answering.
pub fn dummy_verify(password: &str) {
    let _ = verify_password(password, dummy_password_hash());
}

/// A freshly minted secret. The plaintext exists only here and in the one
/// place that shows it; the database gets [`Token::hash`].
#[derive(Clone)]
pub struct Token {
    plaintext: String,
}

impl Token {
    /// 128 bits from the thread CSPRNG, hex encoded.
    pub fn mint() -> Self {
        let mut bytes = [0u8; TOKEN_BYTES];
        rand::rng().fill_bytes(&mut bytes);
        Self {
            plaintext: hex(&bytes),
        }
    }

    /// The value that goes in the link or the cookie. Shown exactly once.
    pub fn expose(&self) -> &str {
        &self.plaintext
    }

    /// The value that goes in the database.
    pub fn hash(&self) -> String {
        hash_token(&self.plaintext)
    }
}

impl std::fmt::Debug for Token {
    /// A token must not reach a log through a stray `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(redacted)")
    }
}

/// SHA-256 of a token, hex encoded. Tokens are full-entropy, so a fast digest
/// is the right primitive here.
pub fn hash_token(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex(&hasher.finalize())
}

/// Compares two token digests without leaking where they diverge.
pub fn digests_match(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The rules the first-sign-in screen states, checked server-side.
pub fn check_password(
    password: &str,
    email: &str,
    display_name: &str,
) -> Result<(), PasswordProblem> {
    // Counted in characters, not bytes: a ten-character password is ten
    // characters whatever alphabet it is in.
    if password.chars().count() < 10 {
        return Err(PasswordProblem::TooShort);
    }

    let folded = password.to_lowercase();
    let local_part = email.split('@').next().unwrap_or(email);
    let mut forbidden = vec![email.to_lowercase(), local_part.to_lowercase()];
    forbidden.extend(
        display_name
            .split_whitespace()
            .map(|word| word.to_lowercase()),
    );
    for needle in forbidden {
        // Short words ("de", "van") would forbid too much to be useful.
        if needle.chars().count() >= 3 && folded.contains(&needle) {
            return Err(PasswordProblem::LooksLikeYou);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_round_trips_and_a_wrong_one_does_not() {
        let phc = hash_password("correct horse battery").unwrap();
        assert!(phc.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{phc}");
        assert!(verify_password("correct horse battery", &phc));
        assert!(!verify_password("correct horse batter", &phc));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        let a = hash_password("correct horse battery").unwrap();
        let b = hash_password("correct horse battery").unwrap();
        assert_ne!(a, b, "salts must differ");
    }

    #[test]
    fn a_malformed_stored_hash_is_a_failed_verify_not_a_panic() {
        assert!(!verify_password("anything", ""));
        assert!(!verify_password("anything", "not-a-phc-string"));
    }

    #[test]
    fn the_dummy_hash_is_usable_and_never_matches() {
        // If this ever stops parsing, the miss path silently gets cheap again.
        assert!(PasswordHash::new(dummy_password_hash()).is_ok());
        assert!(!verify_password("", dummy_password_hash()));
        assert!(!verify_password("password", dummy_password_hash()));
        dummy_verify("password");
    }

    #[test]
    fn tokens_are_full_length_and_never_repeat() {
        let a = Token::mint();
        let b = Token::mint();
        assert_eq!(a.expose().len(), TOKEN_BYTES * 2);
        assert!(a.expose().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.expose(), b.expose());
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn a_token_hashes_to_sha256_and_hides_itself_in_debug() {
        let token = Token::mint();
        assert_eq!(token.hash(), hash_token(token.expose()));
        assert_eq!(token.hash().len(), 64);
        assert_ne!(token.hash(), token.expose());
        assert_eq!(format!("{token:?}"), "Token(redacted)");
    }

    #[test]
    fn known_answer_for_the_token_digest() {
        // SHA-256("izlek"), so a refactor cannot quietly change the algorithm
        // under stored hashes.
        assert_eq!(
            hash_token("izlek"),
            "d81eac046551d3591888f0c7012b8faece2cc3501f63398bc040118bfd835d8e"
        );
    }

    #[test]
    fn digest_comparison_is_by_value() {
        let token = Token::mint();
        assert!(digests_match(&token.hash(), &hash_token(token.expose())));
        assert!(!digests_match(&token.hash(), &hash_token("something else")));
        assert!(!digests_match("short", "longer"));
    }

    #[test]
    fn password_rules_are_the_ones_on_the_screen() {
        let email = "grace@izlek.sh";
        let name = "Grace Hopper";
        assert_eq!(
            check_password("short", email, name),
            Err(PasswordProblem::TooShort)
        );
        assert_eq!(
            check_password("grace-and-more", email, name),
            Err(PasswordProblem::LooksLikeYou)
        );
        assert_eq!(
            check_password("HOPPERhopper", email, name),
            Err(PasswordProblem::LooksLikeYou),
            "case must not be an escape hatch"
        );
        assert_eq!(
            check_password("grace@izlek.sh!!", email, name),
            Err(PasswordProblem::LooksLikeYou)
        );
        assert!(check_password("tide-tables-1892", email, name).is_ok());
        // Ten characters exactly is allowed; nine is not.
        assert!(check_password("abcdefghij", email, name).is_ok());
        assert_eq!(
            check_password("abcdefghi", email, name),
            Err(PasswordProblem::TooShort)
        );
    }
}

#[cfg(test)]
mod dummy_hash_generator {
    //! Run with `cargo test -p izlek-core --lib print_a_dummy_hash -- --ignored
    //! --nocapture` to mint a replacement for [`super::dummy_password_hash`].
    #[test]
    #[ignore = "generator, not a check"]
    fn print_a_dummy_hash() {
        let token = format!(
            "{}{}",
            super::Token::mint().expose(),
            super::Token::mint().expose()
        );
        println!("{}", super::hash_password(&token).unwrap());
    }
}
