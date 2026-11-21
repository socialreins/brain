//! Stripe for Cloud billing: checkout and customer portal sessions, and the webhook signature.
//! Nothing here runs unless the four `STRIPE_*` settings are set. The plan and what it allows
//! stay in brain (`billing`); Stripe only holds the money, and tells brain through the webhook
//! (`controllers::stripe`).
use std::time::Duration;

use serde_json::Value;

pub const DEFAULT_API_URL: &str = "https://api.stripe.com";
/// How far a webhook's timestamp may be from now, the same as Stripe's own libraries.
pub const TOLERANCE_SECONDS: i64 = 300;

/// `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET` and one price per plan. No `Debug`: it holds the
/// keys.
#[derive(Clone)]
pub struct Stripe {
    secret: Option<String>,
    webhook_secret: Option<String>,
    /// `(plan id, Stripe price id)`.
    prices: Vec<(String, String)>,
    /// Tests point this at a mock.
    pub api_url: String,
    client: reqwest::Client,
}

/// Why a call to Stripe failed: the HTTP status and Stripe's error code, never a key or a body.
#[derive(Debug)]
pub struct StripeError(pub Option<u16>, pub String);

impl Stripe {
    /// A key that is not `sk_` or `rk_`, a webhook secret that is not `whsec_` or a price that
    /// is not `price_` fails at start, so a pasted wrong value does not wait for the first sale.
    pub fn new(
        secret: Option<String>,
        webhook_secret: Option<String>,
        prices: [(&str, Option<String>); 2],
    ) -> Result<Self, &'static str> {
        let shaped = |v: &Option<String>, prefixes: &[&str]| {
            v.as_deref()
                .is_none_or(|s| prefixes.iter().any(|p| s.starts_with(p)))
        };
        if !shaped(&secret, &["sk_", "rk_"]) {
            return Err("STRIPE_SECRET_KEY: not a Stripe secret key");
        }
        if !shaped(&webhook_secret, &["whsec_"]) {
            return Err("STRIPE_WEBHOOK_SECRET: not a Stripe signing secret (whsec_...)");
        }
        if prices[0].1.is_some() && prices[0].1 == prices[1].1 {
            return Err("STRIPE_PRICE_SCHEDULER and STRIPE_PRICE_AUTOMATION are the same price");
        }
        let mut have = Vec::new();
        for (plan, price) in prices {
            if !shaped(&price, &["price_"]) {
                return Err("STRIPE_PRICE_*: not a Stripe price id (price_...)");
            }
            have.extend(price.map(|p| (plan.to_owned(), p)));
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| "the Stripe HTTP client could not be built")?;
        Ok(Self {
            secret,
            webhook_secret,
            prices: have,
            api_url: DEFAULT_API_URL.into(),
            client,
        })
    }

    /// Whether sessions can be made: the key and a price for every plan.
    pub fn configured(&self) -> bool {
        self.secret.is_some() && self.prices.len() == 2
    }

    /// Whether the webhook can be checked.
    pub fn webhook_on(&self) -> bool {
        self.webhook_secret.is_some()
    }

    pub fn price_of(&self, plan: &str) -> Option<&str> {
        self.prices
            .iter()
            .find(|(p, _)| p == plan)
            .map(|(_, price)| price.as_str())
    }

    pub fn plan_of(&self, price: &str) -> Option<&str> {
        self.prices
            .iter()
            .find(|(_, p)| p == price)
            .map(|(plan, _)| plan.as_str())
    }

    /// Whether `header` (`Stripe-Signature`) signs the raw `body` at a time within the
    /// tolerance. Never true without a signing secret.
    pub fn signed(&self, body: &[u8], header: &str, now: i64) -> bool {
        self.webhook_secret.as_deref().is_some_and(|secret| {
            socialreins_core::webhook::verify_timestamped(
                secret,
                body,
                header,
                now,
                TOLERANCE_SECONDS,
            )
        })
    }

    async fn post(&self, path: &str, form: &[(String, String)]) -> Result<Value, StripeError> {
        self.call(reqwest::Method::POST, path, form).await
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        form: &[(String, String)],
    ) -> Result<Value, StripeError> {
        let key = self
            .secret
            .as_deref()
            .ok_or_else(|| StripeError(None, "not_configured".into()))?;
        let res = self
            .client
            .request(method, format!("{}{path}", self.api_url))
            .basic_auth(key, None::<&str>)
            .form(form)
            .send()
            .await
            .map_err(|_| StripeError(None, "unreachable".into()))?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        let code = body["error"]["code"]
            .as_str()
            .or(body["error"]["type"].as_str())
            .unwrap_or("error");
        Err(StripeError(
            Some(status.as_u16()),
            code.chars().take(60).collect(),
        ))
    }

    /// A Checkout Session for a subscription: the URL to send the customer to.
    pub async fn checkout(&self, form: &[(String, String)]) -> Result<String, StripeError> {
        self.post("/v1/checkout/sessions", form).await?["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| StripeError(None, "no_url".into()))
    }

    /// Cancels a subscription at once, for a second live one a team started by mistake. `id` is
    /// a Stripe id (letters, digits and `_`), so it cannot add to the path.
    pub async fn cancel(&self, id: &str) -> Result<(), StripeError> {
        if !id.starts_with("sub_") || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(StripeError(None, "invalid_id".into()));
        }
        self.call(
            reqwest::Method::DELETE,
            &format!("/v1/subscriptions/{id}"),
            &[],
        )
        .await
        .map(|_| ())
    }

    /// A Billing Portal session for `customer`: where they manage the card and cancel.
    pub async fn portal(&self, customer: &str, return_url: &str) -> Result<String, StripeError> {
        let form = [
            ("customer".to_owned(), customer.to_owned()),
            ("return_url".to_owned(), return_url.to_owned()),
        ];
        self.post("/v1/billing_portal/sessions", &form).await?["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| StripeError(None, "no_url".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::Stripe;

    fn stripe() -> Stripe {
        Stripe::new(
            Some("sk_test_1".into()),
            Some("whsec_1".into()),
            [
                ("scheduler", Some("price_s".into())),
                ("automation", Some("price_a".into())),
            ],
        )
        .unwrap()
    }

    #[test]
    fn prices_map_both_ways_and_values_are_shape_checked() {
        let s = stripe();
        assert!(s.configured() && s.webhook_on());
        assert_eq!(s.price_of("automation"), Some("price_a"));
        assert_eq!(s.plan_of("price_s"), Some("scheduler"));
        assert_eq!(s.plan_of("price_x"), None);
        assert!(
            !Stripe::new(None, None, [("scheduler", None), ("automation", None)])
                .unwrap()
                .configured()
        );
        let none = [("scheduler", None), ("automation", None)];
        let same = [
            ("scheduler", Some("price_1".into())),
            ("automation", Some("price_1".into())),
        ];
        assert!(Stripe::new(None, None, same).is_err());
        assert!(Stripe::new(Some("nope".into()), None, none.clone()).is_err());
        assert!(Stripe::new(None, Some("sk_test_x".into()), none).is_err());
        let bad = [("scheduler", Some("prod_1".into())), ("automation", None)];
        assert!(Stripe::new(None, None, bad).is_err());
    }
}
