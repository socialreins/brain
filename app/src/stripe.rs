//! Stripe for Cloud billing: checkout and customer portal sessions, and the webhook signature.
//! Nothing here runs unless the `STRIPE_*` settings are set. The plan and what it allows
//! stay in brain (`billing`); Stripe only holds the money, and tells brain through the webhook
//! (`controllers::stripe`).
use std::time::Duration;

use serde_json::Value;

pub const DEFAULT_API_URL: &str = "https://api.stripe.com";
/// How far a webhook's timestamp may be from now, the same as Stripe's own libraries.
pub const TOLERANCE_SECONDS: i64 = 300;
/// Pages of 100 read when listing subscriptions (`Stripe::subscriptions`).
const LIST_PAGES: usize = 20;
/// X credit packs (decision 0006): `(pack id, credits)`, one-time, never expire.
pub const PACKS: [(&str, i64); 2] = [("x100", 100), ("x500", 500)];

/// `STRIPE_SECRET_KEY`, `STRIPE_WEBHOOK_SECRET` and a monthly and a yearly price per plan. No
/// `Debug`: it holds the keys.
#[derive(Clone)]
pub struct Stripe {
    secret: Option<String>,
    webhook_secret: Option<String>,
    /// `(plan id, interval, Stripe price id)`, the interval `month` or `year`.
    prices: Vec<(String, String, String)>,
    /// `(pack id, Stripe price id)` of the X credit packs.
    packs: Vec<(String, String)>,
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
        prices: [(&str, &str, Option<String>); 4],
        packs: [(&str, Option<String>); 2],
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
        let mut have: Vec<(String, String, String)> = Vec::new();
        for (plan, interval, price) in prices {
            if !shaped(&price, &["price_"]) {
                return Err("STRIPE_PRICE_*: not a Stripe price id (price_...)");
            }
            if let Some(p) = price {
                if have.iter().any(|(_, _, q)| *q == p) {
                    return Err("STRIPE_PRICE_*: two plans or intervals have the same price");
                }
                have.push((plan.to_owned(), interval.to_owned(), p));
            }
        }
        let mut sold: Vec<(String, String)> = Vec::new();
        for (pack, price) in packs {
            if !shaped(&price, &["price_"]) {
                return Err("STRIPE_PRICE_X*: not a Stripe price id (price_...)");
            }
            if let Some(p) = price {
                if have.iter().any(|(_, _, q)| *q == p) || sold.iter().any(|(_, q)| *q == p) {
                    return Err(
                        "STRIPE_PRICE_*: two plans, intervals or packs have the same price",
                    );
                }
                sold.push((pack.to_owned(), p));
            }
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
            packs: sold,
            api_url: DEFAULT_API_URL.into(),
            client,
        })
    }

    /// Whether sessions can be made: the key and both prices of every plan.
    pub fn configured(&self) -> bool {
        self.secret.is_some() && self.prices.len() == 4
    }

    /// Whether subscriptions can be read: the key alone.
    pub fn can_read(&self) -> bool {
        self.secret.is_some()
    }

    /// Whether credit packs can be sold: the key and both pack prices.
    pub fn packs_on(&self) -> bool {
        self.secret.is_some() && self.packs.len() == PACKS.len()
    }

    pub fn pack_price(&self, pack: &str) -> Option<&str> {
        self.packs
            .iter()
            .find(|(p, _)| p == pack)
            .map(|(_, price)| price.as_str())
    }

    /// Whether the webhook can be checked.
    pub fn webhook_on(&self) -> bool {
        self.webhook_secret.is_some()
    }

    pub fn price_of(&self, plan: &str, interval: &str) -> Option<&str> {
        self.prices
            .iter()
            .find(|(p, i, _)| p == plan && i == interval)
            .map(|(_, _, price)| price.as_str())
    }

    /// The plan a price (monthly or yearly) belongs to.
    pub fn plan_of(&self, price: &str) -> Option<&str> {
        self.prices
            .iter()
            .find(|(_, _, p)| p == price)
            .map(|(plan, _, _)| plan.as_str())
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
        self.call(reqwest::Method::POST, path, form, None).await
    }

    /// `idempotency`: Stripe's `Idempotency-Key`, so the same call made twice (a double click, a
    /// retry) answers the first one's result instead of making a second thing.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        form: &[(String, String)],
        idempotency: Option<&str>,
    ) -> Result<Value, StripeError> {
        let key = self
            .secret
            .as_deref()
            .ok_or_else(|| StripeError(None, "not_configured".into()))?;
        let mut req = self
            .client
            .request(method.clone(), format!("{}{path}", self.api_url))
            .basic_auth(key, None::<&str>);
        // A read puts its filters in the query, a write sends a form.
        req = if method == reqwest::Method::GET {
            req.query(form)
        } else {
            req.form(form)
        };
        if let Some(k) = idempotency {
            req = req.header("Idempotency-Key", k);
        }
        let res = req
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

    /// Every subscription Stripe still holds (the ended ones are left out by Stripe: `active`,
    /// `trialing`, `past_due`, `unpaid`, `paused`, `incomplete`), at most `LIST_PAGES` pages of
    /// 100. Read only.
    pub async fn subscriptions(&self) -> Result<Vec<Value>, StripeError> {
        let mut out: Vec<Value> = Vec::new();
        for _ in 0..LIST_PAGES {
            let mut query = vec![("limit".to_owned(), "100".to_owned())];
            if let Some(last) = out.last().and_then(|s| s["id"].as_str()) {
                query.push(("starting_after".to_owned(), last.to_owned()));
            }
            let page = self
                .call(reqwest::Method::GET, "/v1/subscriptions", &query, None)
                .await?;
            let data = page["data"].as_array().cloned().unwrap_or_default();
            out.extend(data);
            if page["has_more"].as_bool() != Some(true) {
                return Ok(out);
            }
        }
        // shortcut: more than LIST_PAGES x 100 live subscriptions are not all read; raise it then.
        Err(StripeError(None, "too_many".into()))
    }

    /// The card fingerprint of the payment method a subscription charges (`card.fingerprint`):
    /// one token per card across customers, so a trial can be limited per card as well as per
    /// person. Read only, at most two GETs: the subscription's default payment method (expanded),
    /// else the customer's `invoice_settings` default. `None` when no card was collected. The
    /// fingerprint is never logged; `trial_uses.card_hash` keeps its SHA-256.
    pub async fn card_fingerprint(
        &self,
        subscription: &str,
    ) -> Result<Option<String>, StripeError> {
        if !id_shaped(subscription, "sub_") {
            return Err(StripeError(None, "invalid_id".into()));
        }
        let query = [("expand[0]".to_owned(), "default_payment_method".to_owned())];
        let sub = self
            .call(
                reqwest::Method::GET,
                &format!("/v1/subscriptions/{subscription}"),
                &query,
                None,
            )
            .await?;
        if let Some(card) = card_of(&sub["default_payment_method"]) {
            return Ok(Some(card));
        }
        let Some(customer) = sub["customer"].as_str().filter(|c| id_shaped(c, "cus_")) else {
            return Ok(None);
        };
        let query = [(
            "expand[0]".to_owned(),
            "invoice_settings.default_payment_method".to_owned(),
        )];
        let customer = self
            .call(
                reqwest::Method::GET,
                &format!("/v1/customers/{customer}"),
                &query,
                None,
            )
            .await?;
        Ok(card_of(
            &customer["invoice_settings"]["default_payment_method"],
        ))
    }

    /// A Checkout Session: the URL to send the customer to. Called twice with the same `key`
    /// (and form) within Stripe's 24 hours it answers the same session, so two clicks start one
    /// checkout, not two.
    pub async fn checkout(
        &self,
        form: &[(String, String)],
        key: &str,
    ) -> Result<String, StripeError> {
        let session = self
            .call(
                reqwest::Method::POST,
                "/v1/checkout/sessions",
                form,
                Some(key),
            )
            .await?;
        session["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| StripeError(None, "no_url".into()))
    }

    /// Cancels a subscription at once, for a second live one a team started by mistake. `id` is
    /// a Stripe id (letters, digits and `_`), so it cannot add to the path.
    pub async fn cancel(&self, id: &str) -> Result<(), StripeError> {
        if !id_shaped(id, "sub_") {
            return Err(StripeError(None, "invalid_id".into()));
        }
        self.call(
            reqwest::Method::DELETE,
            &format!("/v1/subscriptions/{id}"),
            &[],
            None,
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

/// Whether a Stripe id has `prefix` and only the characters Stripe uses (`sub_...`, `cus_...`),
/// so it cannot add to a URL path.
fn id_shaped(id: &str, prefix: &str) -> bool {
    id.len() > prefix.len()
        && id.starts_with(prefix)
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The card fingerprint of an expanded payment method, if it is a card.
fn card_of(payment_method: &Value) -> Option<String> {
    payment_method["card"]["fingerprint"]
        .as_str()
        .filter(|fingerprint| !fingerprint.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::Stripe;

    fn stripe() -> Stripe {
        Stripe::new(
            Some("sk_test_1".into()),
            Some("whsec_1".into()),
            [
                ("scheduler", "month", Some("price_s".into())),
                ("automation", "month", Some("price_a".into())),
                ("scheduler", "year", Some("price_sy".into())),
                ("automation", "year", Some("price_ay".into())),
            ],
            [
                ("x100", Some("price_p1".into())),
                ("x500", Some("price_p5".into())),
            ],
        )
        .unwrap()
    }

    #[test]
    fn prices_map_both_ways_and_values_are_shape_checked() {
        let s = stripe();
        assert!(s.configured() && s.webhook_on());
        assert_eq!(s.price_of("automation", "month"), Some("price_a"));
        assert_eq!(s.price_of("automation", "year"), Some("price_ay"));
        assert_eq!(s.price_of("automation", "week"), None);
        assert_eq!(s.plan_of("price_s"), Some("scheduler"));
        assert_eq!(s.plan_of("price_sy"), Some("scheduler"));
        assert_eq!(s.plan_of("price_x"), None);
        assert!(s.packs_on());
        assert_eq!(s.pack_price("x500"), Some("price_p5"));
        assert_eq!(s.pack_price("x1"), None);
        let nopacks = || [("x100", None), ("x500", None)];
        let none = || {
            [
                ("scheduler", "month", None),
                ("automation", "month", None),
                ("scheduler", "year", None),
                ("automation", "year", None),
            ]
        };
        assert!(!Stripe::new(None, None, none(), nopacks())
            .unwrap()
            .configured());
        let mut monthly_only = none();
        monthly_only[0].2 = Some("price_1".into());
        monthly_only[1].2 = Some("price_2".into());
        let half = Stripe::new(Some("sk_test_1".into()), None, monthly_only, nopacks()).unwrap();
        assert!(!half.configured());
        let mut same = none();
        same[0].2 = Some("price_1".into());
        same[3].2 = Some("price_1".into());
        assert!(Stripe::new(None, None, same, nopacks()).is_err());
        assert!(Stripe::new(Some("nope".into()), None, none(), nopacks()).is_err());
        assert!(Stripe::new(None, Some("sk_test_x".into()), none(), nopacks()).is_err());
        let mut bad = none();
        bad[0].2 = Some("prod_1".into());
        assert!(Stripe::new(None, None, bad, nopacks()).is_err());
        let mut taken = none();
        taken[0].2 = Some("price_1".into());
        assert!(Stripe::new(
            None,
            None,
            taken,
            [("x100", Some("price_1".into())), ("x500", None)]
        )
        .is_err());
    }
}
