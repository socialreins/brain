//! Outgoing mail: Resend, any SMTP server, or the log (`MAIL_LOG=true`, single-user self-host).
use std::time::Duration;

use lettre::{message::Mailbox, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

const RESEND_API: &str = "https://api.resend.com/emails";

#[derive(Clone)]
pub enum Mailer {
    /// `MAIL_LOG=true`: each message, sign-in link included, is written to the log at info level.
    Log,
    Smtp {
        transport: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
    },
    /// Resend's HTTPS API. Not its SMTP relay: hosts such as Railway block outbound SMTP.
    Resend {
        client: reqwest::Client,
        endpoint: String,
        api_key: String,
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

    pub fn resend(api_key: &str, from: &str) -> Result<Self, String> {
        Self::resend_at(RESEND_API, api_key, from)
    }

    /// `resend` against another endpoint, for tests.
    pub fn resend_at(endpoint: &str, api_key: &str, from: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self::Resend {
            client,
            endpoint: endpoint.into(),
            api_key: api_key.into(),
            from: parse_from(from)?,
        })
    }

    /// Errors name the failure kind and status code only, never the recipient, body or key.
    pub async fn send(&self, to: &str, subject: &str, body: String) -> Result<(), String> {
        self.send_reply_to(to, None, subject, body).await
    }

    /// `send` with a `Reply-To` address, e.g. the sender of a contact form message.
    pub async fn send_reply_to(
        &self,
        to: &str,
        reply_to: Option<&str>,
        subject: &str,
        body: String,
    ) -> Result<(), String> {
        let (transport, from) = match self {
            Self::Log => {
                tracing::info!(
                    to,
                    reply_to,
                    subject,
                    body,
                    "MAIL_LOG: logging mail instead of sending"
                );
                return Ok(());
            }
            Self::Smtp { transport, from } => (transport, from),
            Self::Resend {
                client,
                endpoint,
                api_key,
                from,
            } => {
                let mut mail = serde_json::json!({
                    "from": from.to_string(),
                    "to": [to],
                    "subject": subject,
                    "text": body,
                });
                if let Some(r) = reply_to {
                    mail["reply_to"] = r.into();
                }
                let res = client
                    .post(endpoint)
                    .bearer_auth(api_key)
                    .json(&mail)
                    .send()
                    .await
                    .map_err(|e| {
                        let kind = if e.is_timeout() {
                            "timeout"
                        } else {
                            "connection"
                        };
                        format!("resend {kind}")
                    })?;
                let status = res.status();
                return if status.is_success() {
                    Ok(())
                } else {
                    Err(format!("resend status {}", status.as_u16()))
                };
            }
        };
        let to = to.parse().map_err(|_| "recipient is not a mailbox")?;
        let mut message = Message::builder()
            .from(from.clone())
            .to(to)
            .subject(subject);
        if let Some(r) = reply_to {
            message = message.reply_to(r.parse().map_err(|_| "reply-to is not a mailbox")?);
        }
        let message = message.body(body).map_err(|_| "message did not build")?;
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::{http::HeaderMap, http::StatusCode, routing::post, Json, Router};
    use serde_json::{json, Value};

    use super::Mailer;

    type Seen = Arc<Mutex<Vec<(Option<String>, Value)>>>;

    /// A local stand-in for Resend that records each request and answers `status`.
    async fn mock(status: StatusCode) -> (String, Seen) {
        let seen = Seen::default();
        let log = seen.clone();
        let app = Router::new().route(
            "/emails",
            post(
                move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    let auth = headers
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().into());
                    log.lock().unwrap().push((auth, body));
                    (status, Json(json!({"id": "x"})))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/emails", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, seen)
    }

    #[tokio::test]
    async fn resend_posts_json_with_the_bearer_key() {
        let (url, seen) = mock(StatusCode::OK).await;
        let mailer = Mailer::resend_at(&url, "re_test", "Brain <login@example.com>").unwrap();
        mailer
            .send("a@example.com", "Hi", "Body".into())
            .await
            .unwrap();
        let (auth, body) = seen.lock().unwrap().pop().unwrap();
        assert_eq!(auth.as_deref(), Some("Bearer re_test"));
        let want = json!({"from": "Brain <login@example.com>", "to": ["a@example.com"],
            "subject": "Hi", "text": "Body"});
        assert_eq!(body, want);
    }

    #[tokio::test]
    async fn resend_sends_reply_to_when_given() {
        let (url, seen) = mock(StatusCode::OK).await;
        let mailer = Mailer::resend_at(&url, "re_test", "login@example.com").unwrap();
        let send = mailer.send_reply_to("a@example.com", Some("b@example.com"), "Hi", "B".into());
        send.await.unwrap();
        let (_, body) = seen.lock().unwrap().pop().unwrap();
        assert_eq!(body["reply_to"], "b@example.com");
    }

    #[tokio::test]
    async fn resend_errors_carry_the_status_only() {
        let (url, _) = mock(StatusCode::UNPROCESSABLE_ENTITY).await;
        let mailer = Mailer::resend_at(&url, "re_secret", "login@example.com").unwrap();
        let err = mailer.send("a@example.com", "Hi", "Body".into()).await;
        assert_eq!(err.unwrap_err(), "resend status 422");
    }
}
