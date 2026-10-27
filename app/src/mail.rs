//! Outgoing mail: Resend, any SMTP server, or the log (`MAIL_LOG=true`, single-user self-host).
use lettre::{
    message::Mailbox, transport::smtp::authentication::Credentials, AsyncSmtpTransport,
    AsyncTransport, Message, Tokio1Executor,
};

#[derive(Clone)]
pub enum Mailer {
    /// `MAIL_LOG=true`: each message, sign-in link included, is written to the log at info level.
    Log,
    Smtp {
        transport: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
    },
}

impl Mailer {
    /// `SMTP_URL`, e.g. `smtps://user:pass@smtp.example.com` (see lettre's `from_url`).
    pub fn smtp(url: &str, from: &str) -> Result<Self, String> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
            .map_err(|_| "SMTP_URL: not a valid smtp:// or smtps:// URL")?
            .build();
        Ok(Self::Smtp {
            transport,
            from: parse_from(from)?,
        })
    }

    /// Resend through its SMTP relay, so one transport serves both.
    pub fn resend(api_key: &str, from: &str) -> Result<Self, String> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::relay("smtp.resend.com")
            .map_err(|e| format!("resend relay: {e}"))?
            .credentials(Credentials::new("resend".into(), api_key.into()))
            .build();
        Ok(Self::Smtp {
            transport,
            from: parse_from(from)?,
        })
    }

    /// Errors name the failure kind and SMTP code only, never the recipient or the body.
    pub async fn send(&self, to: &str, subject: &str, body: String) -> Result<(), String> {
        let (transport, from) = match self {
            Self::Log => {
                tracing::info!(
                    to,
                    subject,
                    body,
                    "MAIL_LOG: logging mail instead of sending"
                );
                return Ok(());
            }
            Self::Smtp { transport, from } => (transport, from),
        };
        let to = to.parse().map_err(|_| "recipient is not a mailbox")?;
        let message = Message::builder()
            .from(from.clone())
            .to(to)
            .subject(subject)
            .body(body)
            .map_err(|_| "message did not build")?;
        transport.send(message).await.map(|_| ()).map_err(|e| {
            let kind = if e.is_permanent() {
                "permanent"
            } else if e.is_transient() {
                "transient"
            } else if e.is_timeout() {
                "timeout"
            } else if e.is_tls() {
                "tls"
            } else {
                "connection"
            };
            let code = e.status().map(|c| c.to_string()).unwrap_or_default();
            format!("smtp {kind} {code}").trim_end().to_owned()
        })
    }
}

fn parse_from(from: &str) -> Result<Mailbox, String> {
    from.parse()
        .map_err(|_| format!("MAIL_FROM: not a mailbox: {from}"))
}
