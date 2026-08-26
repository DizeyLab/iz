//! The `Mailer` the engine talks to when the mail is real.
//!
//! Everything about SMTP stops here. The engine knows a send was accepted or
//! refused, and whether the refusal is worth trying again; it does not know
//! what a 550 is. Keeping the protocol in one file is also what makes the
//! engine testable — the tests hand it a mailer that remembers instead of
//! connecting.
//!
//! The credentials come from the workspace's own settings, written by an admin
//! on the Settings screen. They are never in a literal in here, and the
//! password is not in anything this module logs or puts in an error message: a
//! refusal that quoted the password would put it in the ledger, where an admin
//! screen would later print it.
//!
//! Because the sender is editable while the process runs, the transport cannot
//! be built once at boot. [`WorkspaceSmtp`] holds the last one it built and the
//! settings it was built from; a send whose settings still match reuses the
//! connection pool, and a send after an edit builds a new one and drops the old.

use std::sync::Arc;

use izlek_core::store::Store;
use izlek_core::mail::{MailError, Mailer, Outgoing};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

/// The sender as the mailer needs it: what the admin typed, password included.
/// It exists only inside this module and inside the store; nothing that can
/// reach a response body holds one.
#[derive(Clone, PartialEq, Eq)]
pub struct Sending {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// `Name <address>` if a name was given, the bare address otherwise.
    pub from: String,
}

/// A sender holding one connection pool.
pub struct Smtp {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: String,
}

impl Smtp {
    /// Builds the transport. Fails only on a host name TLS cannot be set up
    /// against — a server that is merely unreachable is discovered per send,
    /// as a retryable refusal, which is the honest place for it.
    pub fn new(mail: &Sending) -> Result<Self, String> {
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

/// The mailer the engine actually holds: it reads the workspace's sender at
/// send time, so an admin who fixes a typo in the host does not have to restart
/// anything for the next mail to go out.
pub struct WorkspaceSmtp {
    store: Arc<dyn Store>,
    /// The live transport and the settings it was built from. Rebuilt only when
    /// those settings change — building one per send would throw away the
    /// connection pool that makes a backlog drain quickly.
    current: tokio::sync::Mutex<Option<(Sending, Arc<Smtp>)>>,
}

impl WorkspaceSmtp {
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self {
            store,
            current: tokio::sync::Mutex::new(None),
        }
    }

    /// The sender as it stands right now, or `None` while the panel is empty.
    ///
    /// A half-filled sender counts as none. The screen will not save one, but
    /// the check is here too, because this is the side that would otherwise
    /// hand lettre an empty host and turn a missing setting into a stack of
    /// refusals in the ledger.
    async fn sending(&self) -> Result<Option<Sending>, MailError> {
        let store = &self.store;
        let workspace = store
            .workspace()
            .await
            .map_err(|e| MailError::retryable(e.to_string()))?;
        let Some(ws) = workspace else {
            return Ok(None);
        };
        let (Some(host), Some(port), Some(username), Some(address)) = (
            ws.smtp_host.clone(),
            ws.smtp_port,
            ws.smtp_username.clone(),
            ws.smtp_from_address.clone(),
        ) else {
            return Ok(None);
        };
        let password = store
            .smtp_password(&ws.id)
            .await
            .map_err(|e| MailError::retryable(e.to_string()))?;
        let (Some(password), Ok(port)) = (password, u16::try_from(port)) else {
            return Ok(None);
        };
        if host.trim().is_empty() || username.trim().is_empty() || password.is_empty() {
            return Ok(None);
        }
        let from = match ws.smtp_from_name.as_deref().map(str::trim) {
            Some(name) if !name.is_empty() => format!("{name} <{address}>"),
            _ => address,
        };
        Ok(Some(Sending {
            host,
            port,
            username,
            password,
            from,
        }))
    }

    /// The transport for the current settings, building a new one only when
    /// they have changed since the last send.
    async fn transport(&self, want: &Sending) -> Result<Arc<Smtp>, MailError> {
        let mut current = self.current.lock().await;
        if let Some((held, smtp)) = current.as_ref()
            && held == want
        {
            return Ok(smtp.clone());
        }
        let built = Arc::new(Smtp::new(want).map_err(MailError::retryable)?);
        *current = Some((want.clone(), built.clone()));
        Ok(built)
    }
}

#[async_trait::async_trait]
impl Mailer for WorkspaceSmtp {
    async fn send(&self, mail: &Outgoing) -> Result<(), MailError> {
        let Some(want) = self.sending().await? else {
            // Not a failure, and pointedly not an attempt: the mail is owed
            // until somebody fills the sender in.
            return Err(MailError::unsent(
                "no sender is configured, so this is waiting rather than failing",
            ));
        };
        self.transport(&want).await?.send(mail).await
    }
}
