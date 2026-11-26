//! Outgoing mail: Resend, any SMTP server, or the log (`MAIL_LOG=true`, single-user self-host).
use std::time::Duration;

use lettre::{
    message::{
        header::{HeaderName, HeaderValue},
        Mailbox, MultiPart,
    },
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};

const RESEND_API: &str = "https://api.resend.com/emails";
const ONE_CLICK: &str = "List-Unsubscribe=One-Click";

/// What makes a mail a newsletter: the name it comes from (the address stays ours) and the URL
/// a mail provider posts to when the reader unsubscribes there (RFC 8058).
#[derive(Clone, Copy)]
pub struct List<'a> {
    pub from_name: &'a str,
    pub unsubscribe: &'a str,
}

/// Waits before the two more tries of `Mailer::send`.
const RETRY_WAITS: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1500)];

/// Whether a `deliver` error is the provider being slow or down (a timeout, no connection, a
/// 429 or a 5xx), which another try can fix, and not a refusal.
fn transient(error: &str) -> bool {
    let status = error.strip_prefix("resend status ");
    matches!(error, "resend timeout" | "resend connection")
        || status.is_some_and(|c| c == "429" || c.starts_with('5'))
        || ["smtp transient", "smtp timeout", "smtp connection"]
            .iter()
            .any(|p| error.starts_with(p))
}

/// Mails being sent by a task nobody waits for (a sign-in link, answered before it is sent):
/// `drain` lets a shutdown wait for them.
static SENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Held by such a task until it ends.
pub struct Sending;

impl Sending {
    pub fn begin() -> Self {
        SENDING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Sending
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        SENDING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Waits until no `Sending` is held, at most `limit`.
pub async fn drain(limit: Duration) {
    let end = tokio::time::Instant::now() + limit;
    while SENDING.load(std::sync::atomic::Ordering::SeqCst) > 0 && tokio::time::Instant::now() < end
    {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

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
    /// The plain text is also sent as a branded HTML version (`html`), for mail we write;
    /// `web_url` is the web app, which serves its logo.
    pub async fn send(
        &self,
        to: &str,
        subject: &str,
        body: String,
        web_url: &str,
    ) -> Result<(), String> {
        let page = html(subject, &body, web_url);
        // Mail we write (sign-in links, notices) is safe to send twice, so a provider that is
        // slow or briefly down gets another try; newsletters and contact mail do not (below).
        let mut waits = RETRY_WAITS.into_iter();
        loop {
            let sent = self
                .deliver(to, None, subject, body.clone(), Some(page.clone()), None)
                .await;
            match (sent, waits.next()) {
                (Err(e), Some(wait)) if transient(&e) => {
                    tracing::warn!(error = %e, "mail not sent, trying again");
                    tokio::time::sleep(wait).await;
                }
                (sent, _) => return sent,
            }
        }
    }

    /// A newsletter: the caller's own text and HTML, from `list.from_name`, with the
    /// `List-Unsubscribe` and `List-Unsubscribe-Post` headers.
    pub async fn send_list(
        &self,
        to: &str,
        list: List<'_>,
        subject: &str,
        text: String,
        html: String,
    ) -> Result<(), String> {
        self.deliver(to, None, subject, text, Some(html), Some(list))
            .await
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
        self.deliver(to, reply_to, subject, body, None, None).await
    }

    async fn deliver(
        &self,
        to: &str,
        reply_to: Option<&str>,
        subject: &str,
        body: String,
        html: Option<String>,
        list: Option<List<'_>>,
    ) -> Result<(), String> {
        let named = |from: &Mailbox| match list {
            Some(l) => Mailbox::new(Some(l.from_name.to_owned()), from.email.clone()),
            None => from.clone(),
        };
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
            Self::Smtp { transport, from } => (transport, named(from)),
            Self::Resend {
                client,
                endpoint,
                api_key,
                from,
            } => {
                let mut mail = serde_json::json!({
                    "from": named(from).to_string(),
                    "to": [to],
                    "subject": subject,
                    "text": body,
                });
                if let Some(l) = list {
                    mail["headers"] = serde_json::json!({
                        "List-Unsubscribe": format!("<{}>", l.unsubscribe),
                        "List-Unsubscribe-Post": ONE_CLICK,
                    });
                }
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
        let message = smtp_message(from, to, reply_to, subject, body, html, list)?;
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

/// The message the SMTP transport sends.
fn smtp_message(
    from: Mailbox,
    to: &str,
    reply_to: Option<&str>,
    subject: &str,
    body: String,
    html: Option<String>,
    list: Option<List<'_>>,
) -> Result<Message, String> {
    let to = to.parse().map_err(|_| "recipient is not a mailbox")?;
    let mut message = Message::builder().from(from).to(to).subject(subject);
    if let Some(r) = reply_to {
        message = message.reply_to(r.parse().map_err(|_| "reply-to is not a mailbox")?);
    }
    if let Some(l) = list {
        let header = |name, value| HeaderValue::new(HeaderName::new_from_ascii_str(name), value);
        message = message
            .raw_header(header("List-Unsubscribe", format!("<{}>", l.unsubscribe)))
            .raw_header(header("List-Unsubscribe-Post", ONE_CLICK.to_owned()));
    }
    let message = match html {
        Some(page) => message.multipart(MultiPart::alternative_plain_html(body, page)),
        None => message.body(body),
    };
    message.map_err(|_| "message did not build".to_owned())
}

pub(crate) fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The HTML version of a plain-text mail: the subject as a heading, each paragraph, and every
/// line that is only a link as a button. A line ending in `:` right before a link is the
/// button's label ("Confirm my email:"). Everything is escaped; only `http(s)` links are linked.
/// Tables and inline styles, because mail clients ignore most CSS. The logo above it is the web
/// app's own icon (`{web_url}/icon-192.png`), so a self-hosted install shows its own.
fn html(subject: &str, text: &str, web_url: &str) -> String {
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
         <img src=\"{web}/icon-192.png\" width=\"28\" height=\"28\" alt=\"\" \
         style=\"display:inline-block;vertical-align:middle;border:0;border-radius:6px\"> \
         <span style=\"vertical-align:middle\">SocialReins</span></td></tr>\
         <tr><td style=\"background:#ffffff;border:1px solid #e7e5e4;border-radius:16px;\
         padding:32px 28px;font-size:16px;line-height:1.55\">\
         <h1 style=\"margin:0 0 20px;font-size:24px;line-height:1.25;font-weight:800;\
         letter-spacing:-0.02em\">{title}</h1>{body}</td></tr>\
         <tr><td style=\"padding:16px 4px 0;font-size:13px;line-height:1.5;color:{SOFT}\">\
         SocialReins, the open-source social media manager.</td></tr>\
         </table></td></tr></table></body></html>",
        title = escape(subject),
        web = escape(web_url)
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
            .send("a@example.com", "Hi", "Body".into(), "https://web.test")
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

    /// A stand-in that answers 503 to the first `fails` requests, then 200.
    async fn flaky(fails: usize) -> (String, Seen) {
        let seen = Seen::default();
        let log = seen.clone();
        let app = Router::new().route(
            "/emails",
            post(move |Json(body): Json<Value>| async move {
                let mut seen = log.lock().unwrap();
                seen.push((None, body));
                let status = if seen.len() <= fails {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::OK
                };
                (status, Json(json!({"id": "x"})))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/emails", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (url, seen)
    }

    #[tokio::test]
    async fn mail_we_write_is_tried_again_when_the_provider_is_down_for_a_moment() {
        let send = |url: String| async move {
            let mailer = Mailer::resend_at(&url, "re_test", "login@example.com").unwrap();
            mailer
                .send("a@example.com", "Hi", "Body".into(), "https://web.test")
                .await
        };
        // Two 503s and then it works: three requests, one Ok.
        let (url, seen) = flaky(2).await;
        send(url).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 3);
        // Down for good: three tries and the last error.
        let (url, seen) = mock(StatusCode::BAD_GATEWAY).await;
        assert_eq!(send(url).await.unwrap_err(), "resend status 502");
        assert_eq!(seen.lock().unwrap().len(), 3);
        // A refusal is not retried.
        let (url, seen) = mock(StatusCode::UNPROCESSABLE_ENTITY).await;
        assert_eq!(send(url).await.unwrap_err(), "resend status 422");
        assert_eq!(seen.lock().unwrap().len(), 1);
        // A rate limit is.
        let (url, seen) = flaky(1).await;
        send(url).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_newsletter_or_contact_mail_is_never_sent_twice_by_a_retry() {
        let (url, seen) = mock(StatusCode::SERVICE_UNAVAILABLE).await;
        let mailer = Mailer::resend_at(&url, "re_test", "login@example.com").unwrap();
        let list = super::List {
            from_name: "Ann",
            unsubscribe: "https://brain.test/u",
        };
        let sent = mailer.send_list("a@example.com", list, "Hi", "T".into(), "<p>T</p>".into());
        assert!(sent.await.is_err());
        let sent = mailer.send_reply_to("a@example.com", None, "Hi", "T".into());
        assert!(sent.await.is_err());
        assert_eq!(seen.lock().unwrap().len(), 2, "one request each");
    }

    #[test]
    fn only_a_provider_that_is_slow_or_down_is_transient() {
        for e in [
            "resend timeout",
            "resend connection",
            "resend status 429",
            "resend status 503",
            "smtp transient 451",
            "smtp timeout",
        ] {
            assert!(super::transient(e), "{e}");
        }
        for e in [
            "resend status 400",
            "resend status 422",
            "smtp permanent 550",
            "smtp tls",
            "recipient is not a mailbox",
        ] {
            assert!(!super::transient(e), "{e}");
        }
    }

    #[tokio::test]
    async fn drain_waits_for_a_mail_in_flight_and_gives_up_at_the_limit() {
        use std::time::{Duration, Instant};
        let held = super::Sending::begin();
        let started = Instant::now();
        super::drain(Duration::from_millis(300)).await;
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "waited for it"
        );
        let started = Instant::now();
        let release = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(held);
        });
        super::drain(Duration::from_secs(5)).await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "left as soon as it ended"
        );
        release.await.unwrap();
    }

    #[test]
    fn html_turns_a_labelled_link_into_a_button_and_escapes_everything() {
        let text =
            "Thanks, <b>Ann & co</b>.\n\nConfirm my email:\nhttps://x.test/v?token=a&b=\"c\"\n\n\
                    Not a link: javascript:alert(1)\nhttps://x.test/plain\n";
        let page = super::html("Hi <you>", text, "https://web.test/\"><b");
        // The header is the web app's icon beside the name, its address escaped.
        let logo = "<img src=\"https://web.test/&quot;&gt;&lt;b/icon-192.png\" width=\"28\" \
                    height=\"28\" alt=\"\" style=\"display:inline-block;vertical-align:middle;\
                    border:0;border-radius:6px\"> <span style=\"vertical-align:middle\">\
                    SocialReins</span>";
        assert!(page.contains(logo), "{page}");
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
    async fn a_newsletter_carries_its_name_and_the_one_click_headers() {
        let link = format!(
            "https://brain.test/v1/public/newsletter/unsubscribe?token={}",
            "a".repeat(64)
        );
        let list = super::List {
            from_name: "Ann, Inc.",
            unsubscribe: &link,
        };
        let (url, seen) = mock(StatusCode::OK).await;
        let mailer = Mailer::resend_at(&url, "re_test", "Brain <login@example.com>").unwrap();
        let send = mailer.send_list("a@example.com", list, "Hi", "T".into(), "<p>T</p>".into());
        send.await.unwrap();
        let (_, body) = seen.lock().unwrap().pop().unwrap();
        assert_eq!(body["from"], "\"Ann, Inc.\" <login@example.com>");
        assert_eq!(body["html"], "<p>T</p>");
        assert_eq!(body["headers"]["List-Unsubscribe"], format!("<{link}>"));
        assert_eq!(
            body["headers"]["List-Unsubscribe-Post"],
            "List-Unsubscribe=One-Click"
        );
        // Over SMTP the same headers, the long link on one unbroken line.
        let from: super::Mailbox = "login@example.com".parse().unwrap();
        // A name cannot leave its quotes, whatever it holds.
        let tricky = "Ann \"x\" \\ <evil@example.com>, Bob";
        let named = super::Mailbox::new(Some(tricky.to_owned()), from.email.clone());
        // What Resend gets: the quotes and the backslash escaped inside one quoted name.
        assert_eq!(
            named.to_string(),
            "\"Ann \\\"x\\\" \\\\ <evil@example.com>, Bob\" <login@example.com>"
        );
        // Over SMTP the name is one encoded word, so the header holds our address alone.
        let m = super::smtp_message(named, "a@example.com", None, "Hi", "T".into(), None, None);
        let raw = String::from_utf8(m.unwrap().formatted()).unwrap();
        let header = raw.lines().find(|l| l.starts_with("From: ")).unwrap();
        assert_eq!(
            header,
            "From: =?utf-8?b?QW5uICJ4IiBcIDxldmlsQGV4YW1wbGUuY29tPiwgQm9i?= <login@example.com>"
        );
        let args = (Some("<p>T</p>".to_owned()), Some(list));
        let message = super::smtp_message(
            from,
            "a@example.com",
            None,
            "Hi",
            "T".into(),
            args.0,
            args.1,
        );
        let raw = String::from_utf8(message.unwrap().formatted()).unwrap();
        let unfolded = raw.replace("\r\n ", " ");
        assert!(
            unfolded.contains(&format!("List-Unsubscribe: <{link}>\r\n")),
            "{raw}"
        );
        assert!(raw.contains("List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n"));
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
        let err = mailer
            .send("a@example.com", "Hi", "Body".into(), "https://web.test")
            .await;
        assert_eq!(err.unwrap_err(), "resend status 422");
    }
}
