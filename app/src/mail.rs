//! Outgoing mail: Resend or any SMTP server, or the log when neither is configured (self-host).
use lettre::{
    message::Mailbox, transport::smtp::authentication::Credentials, AsyncSmtpTransport,
    AsyncTransport, Message, Tokio1Executor,
};

#[derive(Clone)]
pub enum Mailer {
    /// No mail configured: each message is written to the log at info level.
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

    pub async fn send(&self, to: &str, subject: &str, body: String) -> Result<(), String> {
        let (transport, from) = match self {
            Self::Log => {
                tracing::info!(to, subject, body, "mail not configured, logging it instead");
                return Ok(());
            }
            Self::Smtp { transport, from } => (transport, from),
        };
        let message = Message::builder()
            .from(from.clone())
            .to(to.parse().map_err(|e| format!("recipient: {e}"))?)
            .subject(subject)
            .body(body)
            .map_err(|e| e.to_string())?;
        transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn parse_from(from: &str) -> Result<Mailbox, String> {
    from.parse()
        .map_err(|_| format!("MAIL_FROM: not a mailbox: {from}"))
}
