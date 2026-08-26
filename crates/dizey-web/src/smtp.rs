//! The `Mailer` the engine talks to when the mail is real.
//!
//! Everything about SMTP stops here. The engine knows a send was accepted or
//! refused, and whether the refusal is worth trying again; it does not know
//! what a 550 is. Keeping the protocol in one file is also what makes the
//! engine testable — the tests hand it a mailer that remembers instead of
//! connecting.
//!
//! The credentials come from the environment through `Config`, never from a
//! literal in here, and the password is not in anything this module logs or
//! puts in an error message: a refusal that quoted the password would put it
//! in the ledger, where an admin screen would later print it.

use dizey_core::config::MailConfig;
use dizey_core::mail::{MailError, Mailer, Outgoing};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

/// A sender holding one connection pool for the life of the process.
pub struct Smtp {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
}

impl Smtp {
    /// Builds the transport. Fails only on a host name TLS cannot be set up
    /// against — a server that is merely unreachable is discovered per send,
    /// as a retryable refusal, which is the honest place for it.
    pub fn new(mail: &MailConfig) -> Result<Self, String> {
        // 465 is submissions, where TLS is up before the greeting. Everything
        // else — 587 above all — is submission, where the session starts in
        // the clear and is upgraded. Both are encrypted before the password
        // moves; a plaintext session is not among the options.
        let builder = if mail.port == 465 {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&mail.host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&mail.host)
        }
        .map_err(|problem| format!("{} cannot be reached over TLS: {problem}", mail.host))?;

        Ok(Self {
            transport: builder
                .port(mail.port)
                .credentials(Credentials::new(
                    mail.username.clone(),
                    mail.password.clone(),
                ))
                .build(),
            from: mail.from.clone(),
        })
    }
}

#[async_trait::async_trait]
impl Mailer for Smtp {
    async fn send(&self, mail: &Outgoing) -> Result<(), MailError> {
        let built =
            Message::builder()
                .from(self.from.parse().map_err(|_| {
                    MailError::permanent(format!("{} is not an address", self.from))
                })?)
                .to(mail
                    .to
                    .parse()
                    .map_err(|_| MailError::permanent(format!("{} is not an address", mail.to)))?)
                .subject(&mail.subject)
                .header(ContentType::TEXT_PLAIN)
                .body(mail.body.clone())
                .map_err(|problem| MailError::permanent(problem.to_string()))?;

        self.transport
            .send(built)
            .await
            .map(|_| ())
            .map_err(|problem| {
                let said = problem.to_string();
                // A permanent SMTP code is the server saying the address will not
                // work today or tomorrow — a mailbox that does not exist, a domain
                // that does not accept us. Everything else, a timeout above all,
                // is worth another attempt: the alternative is losing a mail
                // because a host restarted at the wrong moment.
                if problem.is_permanent() {
                    MailError::permanent(said)
                } else {
                    MailError::retryable(said)
                }
            })
    }
}
