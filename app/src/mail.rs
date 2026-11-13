//! Outgoing mail: Resend, any SMTP server, or the log (`MAIL_LOG=true`, single-user self-host).
use std::time::Duration;

use lettre::{
    message::{Mailbox, MultiPart},
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};

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
    /// The plain text is also sent as a branded HTML version (`html`), for mail we write.
    pub async fn send(&self, to: &str, subject: &str, body: String) -> Result<(), String> {
        let page = html(subject, &body);
        self.deliver(to, None, subject, body, Some(page)).await
    }

    /// Plain text only, with a `Reply-To` address: a contact form message, whose text a
    /// stranger wrote.
    pub async fn send_reply_to(
        &self,
        to: &str,
        reply_to: Option<&str>,
        subject: &str,
        body: String,
    ) -> Result<(), String> {
        self.deliver(to, reply_to, subject, body, None).await
    }

    async fn deliver(
        &self,
        to: &str,
        reply_to: Option<&str>,
        subject: &str,
        body: String,
        html: Option<String>,
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
                if let Some(page) = html {
                    mail["html"] = page.into();
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
        let message = match html {
            Some(page) => message.multipart(MultiPart::alternative_plain_html(body, page)),
            None => message.body(body),
        }
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

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The HTML version of a plain-text mail: the subject as a heading, each paragraph, and every
/// line that is only a link as a button. A line ending in `:` right before a link is the
/// button's label ("Confirm my email:"). Everything is escaped; only `http(s)` links are linked.
/// Tables and inline styles, because mail clients ignore most CSS.
fn html(subject: &str, text: &str) -> String {
    const INK: &str = "#121212";
    const SOFT: &str = "#57534e";
    const SIGNAL: &str = "#c2330a";
    let is_link = |l: &str| {
        (l.starts_with("https://") || l.starts_with("http://")) && !l.contains(char::is_whitespace)
    };
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let mut body = String::new();
    for (i, line) in lines.iter().enumerate() {
        if is_link(line) {
            let label = match i.checked_sub(1).map(|p| lines[p]) {
                Some(prev) if prev.ends_with(':') && !is_link(prev) => prev.trim_end_matches(':'),
                _ => "Open link",
            };
            let url = escape(line);
            // The colour and padding sit on the cell: Outlook ignores them on a link.
            body.push_str(&format!(
                "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" \
                 style=\"margin:24px 0 8px\"><tr><td bgcolor=\"{INK}\" style=\"background:{INK};\
                 border-radius:999px;padding:12px 22px\"><a href=\"{url}\" style=\"color:#ffffff;\
                 font-weight:700;text-decoration:none\">{}</a></td></tr></table>\
                 <p style=\"margin:0 0 20px;font-size:13px;line-height:1.5;color:{SOFT}\">Or copy \
                 this link: <a href=\"{url}\" style=\"color:{SIGNAL};word-break:break-all\">{url}</a></p>",
                escape(label)
            ));
        } else if line.ends_with(':') && lines.get(i + 1).is_some_and(|n| is_link(n)) {
            // Shown on the button instead.
        } else {
            body.push_str(&format!(
                "<p style=\"margin:0 0 16px\">{}</p>",
                escape(line)
            ));
        }
    }
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta name=\"color-scheme\" content=\"light\">\
         <meta name=\"supported-color-schemes\" content=\"light\"><title>{title}</title></head>\
         <body style=\"margin:0;padding:0;background:#faf8f6\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" \
         style=\"background:#faf8f6\"><tr><td align=\"center\" style=\"padding:32px 16px\">\
         <table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" \
         style=\"max-width:560px;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,\
         Helvetica,Arial,sans-serif;color:{INK}\">\
         <tr><td style=\"padding:0 4px 16px;font-size:20px;font-weight:800;letter-spacing:-0.02em\">\
         <span style=\"color:{SIGNAL}\">&#9679;</span> SocialReins</td></tr>\
         <tr><td style=\"background:#ffffff;border:1px solid #e7e5e4;border-radius:16px;\
         padding:32px 28px;font-size:16px;line-height:1.55\">\
         <h1 style=\"margin:0 0 20px;font-size:24px;line-height:1.25;font-weight:800;\
         letter-spacing:-0.02em\">{title}</h1>{body}</td></tr>\
         <tr><td style=\"padding:16px 4px 0;font-size:13px;line-height:1.5;color:{SOFT}\">\
         SocialReins, the open-source social media manager.</td></tr>\
         </table></td></tr></table></body></html>",
        title = escape(subject)
    )
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
        assert_eq!(body["from"], "Brain <login@example.com>");
        assert_eq!(body["to"], json!(["a@example.com"]));
        assert_eq!(body["subject"], "Hi");
        assert_eq!(body["text"], "Body");
        assert!(body["html"]
            .as_str()
            .unwrap()
            .contains("<p style=\"margin:0 0 16px\">Body</p>"));
    }

    #[test]
    fn html_turns_a_labelled_link_into_a_button_and_escapes_everything() {
        let text =
            "Thanks, <b>Ann & co</b>.\n\nConfirm my email:\nhttps://x.test/v?token=a&b=\"c\"\n\n\
                    Not a link: javascript:alert(1)\nhttps://x.test/plain\n";
        let page = super::html("Hi <you>", text);
        assert!(page.contains("<h1 style=") && page.contains(">Hi &lt;you&gt;</h1>"));
        assert!(page.contains("Thanks, &lt;b&gt;Ann &amp; co&lt;/b&gt;."));
        // The label moves onto the button and is not repeated as a paragraph.
        let href = "href=\"https://x.test/v?token=a&amp;b=&quot;c&quot;\"";
        assert_eq!(page.matches(href).count(), 2);
        assert!(page.contains(">Confirm my email</a>"));
        assert!(!page.contains("Confirm my email:"));
        // A line that is not an http(s) link stays text; a bare link gets a default label.
        assert!(page.contains("<p style=\"margin:0 0 16px\">Not a link: javascript:alert(1)</p>"));
        assert!(!page.contains("href=\"javascript"));
        assert!(page.contains(">Open link</a>"));
    }

    #[tokio::test]
    async fn a_contact_message_stays_plain_text() {
        let (url, seen) = mock(StatusCode::OK).await;
        let mailer = Mailer::resend_at(&url, "re_test", "login@example.com").unwrap();
        let send = mailer.send_reply_to("a@example.com", None, "Hi", "B".into());
        send.await.unwrap();
        let (_, body) = seen.lock().unwrap().pop().unwrap();
        assert!(body.get("html").is_none());
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
