//! Exchange rates, from [Frankfurter](https://frankfurter.dev), which serves
//! the European Central Bank's daily reference rates (about 30 currencies,
//! going back to 1999). A donation is converted once, when it is recorded, with
//! the rate of the day it was made.
//!
//! The ECB publishes rates on business days. For a weekend or a holiday
//! Frankfurter answers with the previous business day's rate, and says which
//! day it is for.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::civil::Date;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use reqwest::{Client, StatusCode};
use url::Url;

use crate::metrics::ResultLabels;
use crate::model::{Conversion, FxSource};
use crate::money;
use crate::store::{Store, StoreError};

/// The oldest a rate may be compared with the day it is for: long enough for
/// any weekend plus holidays (Easter, Christmas), short enough that a response
/// from the wrong period is not mistaken for one.
const MAX_RATE_AGE_DAYS: i32 = 10;

/// Largest response that is read.
const MAX_BODY: usize = 64 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FxError {
    /// Frankfurter does not publish this currency (or this date).
    #[error("no exchange rate is published for {0}")]
    Unsupported(String),
    #[error("the exchange rate service is rate limiting requests")]
    RateLimited,
    #[error("the exchange rate service could not be reached: {0}")]
    Http(String),
    #[error("unexpected answer from the exchange rate service: {0}")]
    Invalid(String),
    #[error("that amount converts to nothing in the base currency")]
    Dust,
    #[error("exchange rates are turned off")]
    Disabled,
}

/// A rate, and the day it is for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rate {
    /// Units of the base currency per unit of the foreign one.
    pub rate: f64,
    pub rate_date: Date,
}

#[derive(Clone, Debug)]
pub struct FxClient {
    http: Client,
    base_url: Url,
}

impl FxClient {
    /// `base_url` is the service, e.g. `https://api.frankfurter.dev`.
    pub fn new(base_url: Url) -> Result<FxClient, reqwest::Error> {
        Ok(FxClient {
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::limited(3))
                .user_agent(concat!("donarium/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base_url,
        })
    }

    /// The rate for one unit of `currency` in `base` on `date`.
    pub async fn rate(&self, date: Date, currency: &str, base: &str) -> Result<Rate, FxError> {
        let mut url = self.base_url.clone();
        url.set_path(&format!(
            "{}/v1/{date}",
            self.base_url.path().trim_end_matches('/')
        ));
        url.query_pairs_mut()
            .append_pair("base", currency)
            .append_pair("symbols", base);
        let mut response = self.http.get(url).send().await.map_err(|e| {
            FxError::Http(if e.is_timeout() {
                "timed out".into()
            } else if e.is_connect() {
                "could not connect".into()
            } else {
                "request error".into()
            })
        })?;
        match response.status() {
            s if s.is_success() => {}
            // Frankfurter answers 404 for a currency it does not know, and 422
            // for a base and symbol that are the same or otherwise invalid.
            StatusCode::NOT_FOUND | StatusCode::UNPROCESSABLE_ENTITY => {
                return Err(FxError::Unsupported(currency.to_string()));
            }
            StatusCode::TOO_MANY_REQUESTS => return Err(FxError::RateLimited),
            s => return Err(FxError::Http(format!("HTTP {s}"))),
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| FxError::Http("the download was interrupted".into()))?
        {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(FxError::Invalid("the response is too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        parse_answer(&body, date, base)
    }
}

/// `{"amount":1.0,"base":"USD","date":"2026-09-25","rates":{"EUR":0.85}}`
fn parse_answer(body: &[u8], asked: Date, base: &str) -> Result<Rate, FxError> {
    let invalid = |what: &str| FxError::Invalid(what.to_string());
    let json: serde_json::Value =
        serde_json::from_slice(body).map_err(|_| invalid("the answer is not JSON"))?;
    let rate = json
        .pointer(&format!("/rates/{base}"))
        .and_then(serde_json::Value::as_f64)
        .filter(|r| r.is_finite() && *r > 0.0 && *r < 1_000_000.0)
        .ok_or_else(|| invalid("no usable rate in the answer"))?;
    let rate_date: Date = json
        .get("date")
        .and_then(serde_json::Value::as_str)
        .and_then(|d| d.parse().ok())
        .ok_or_else(|| invalid("no date in the answer"))?;
    // The rate must be for the day asked about or a little before: never a
    // later day, and not one from long ago.
    let oldest = asked
        .checked_sub(jiff::Span::new().days(MAX_RATE_AGE_DAYS))
        .map_err(|_| invalid("a date out of range"))?;
    if rate_date > asked || rate_date < oldest {
        return Err(FxError::Invalid(format!(
            "the rate is for {rate_date}, not for {asked} or the days just before"
        )));
    }
    Ok(Rate { rate, rate_date })
}

/// Converts amounts to the base currency, with a cache in the database.
pub struct Fx {
    client: Option<FxClient>,
    base: String,
    /// Currencies the service said it has no rate for: not asked again until
    /// the server restarts (a rate can then still be entered by hand).
    unsupported: Mutex<HashSet<String>>,
    requests: Family<ResultLabels, Counter>,
}

impl Fx {
    /// `client` is `None` when exchange rates are turned off.
    pub fn new(
        client: Option<FxClient>,
        base: String,
        requests: Family<ResultLabels, Counter>,
    ) -> Arc<Fx> {
        Arc::new(Fx {
            client,
            base,
            unsupported: Mutex::new(HashSet::new()),
            requests,
        })
    }

    pub fn enabled(&self) -> bool {
        self.client.is_some()
    }

    fn count(&self, result: &str) {
        self.requests
            .get_or_create(&ResultLabels {
                result: result.into(),
            })
            .inc();
    }

    /// The ECB's rate for `currency` on `date`, from the cache or the service.
    async fn rate(&self, store: &Store, currency: &str, date: Date) -> Result<f64, FxError> {
        let corrupt = |e: StoreError| FxError::Http(format!("the rate cache failed: {e}"));
        if let Some(rate) = store
            .cached_rate(date, currency, &self.base)
            .await
            .map_err(corrupt)?
        {
            return Ok(rate);
        }
        let client = self.client.as_ref().ok_or(FxError::Disabled)?;
        if self
            .unsupported
            .lock()
            .is_ok_and(|set| set.contains(currency))
        {
            return Err(FxError::Unsupported(currency.to_string()));
        }
        match client.rate(date, currency, &self.base).await {
            Ok(found) => {
                self.count("ok");
                store
                    .save_rate(date, currency, &self.base, &found)
                    .await
                    .map_err(corrupt)?;
                Ok(found.rate)
            }
            Err(e) => {
                self.count(match &e {
                    FxError::Unsupported(_) => "unsupported",
                    FxError::RateLimited => "rate_limited",
                    _ => "error",
                });
                if let FxError::Unsupported(_) = &e
                    && let Ok(mut set) = self.unsupported.lock()
                {
                    set.insert(currency.to_string());
                }
                Err(e)
            }
        }
    }

    /// The donation's amount in the base currency, converted with the ECB's
    /// rate for the day it was made.
    pub async fn convert(
        &self,
        store: &Store,
        amount_minor: i64,
        currency: &str,
        date: Date,
    ) -> Result<Conversion, FxError> {
        if currency == self.base {
            return Ok(Conversion {
                base_minor: amount_minor,
                rate: 1.0,
                source: FxSource::Same,
            });
        }
        let rate = self.rate(store, currency, date).await?;
        let base_minor =
            money::convert(amount_minor, currency, rate, &self.base).ok_or(FxError::Dust)?;
        Ok(Conversion {
            base_minor,
            rate,
            source: FxSource::Ecb,
        })
    }

    /// Converts donations that are waiting for it. Returns how many were done.
    /// Stops early if the service is rate limiting or unreachable, since the
    /// rest would fail the same way.
    pub async fn fill_pending(&self, store: &Store) -> Result<usize, StoreError> {
        if !self.enabled() {
            return Ok(0);
        }
        let mut done = 0;
        for pending in store.pending_conversions(50).await? {
            match self
                .convert(
                    store,
                    pending.amount_minor,
                    &pending.currency,
                    pending.donated_on,
                )
                .await
            {
                Ok(conversion) => match store.set_conversion(pending.id, &conversion).await {
                    Ok(()) => done += 1,
                    // Converted by hand in the meantime.
                    Err(StoreError::InUse(_) | StoreError::NotFound) => {}
                    Err(e) => return Err(e),
                },
                Err(FxError::RateLimited | FxError::Http(_)) => break,
                // Unsupported or too small: it stays pending until someone
                // enters a rate.
                Err(_) => {}
            }
        }
        Ok(done)
    }
}

/// Tries to convert pending donations every `every`, starting a minute after
/// the server starts.
pub fn spawn_retry(fx: Arc<Fx>, store: Arc<Store>, every: Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            match fx.fill_pending(&store).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(converted = n, "converted pending donations"),
                Err(e) => tracing::warn!(error = %e, "converting pending donations failed"),
            }
            tokio::time::sleep(every).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, DonationKind, NewDonation};
    use jiff::civil::date;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn answer(base: &str, day: &str, rate: f64) -> serde_json::Value {
        json!({"amount": 1.0, "base": base, "date": day, "rates": {"EUR": rate}})
    }

    async fn serve(server: &MockServer, day: &str, currency: &str, body: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path(format!("/v1/{day}")))
            .and(query_param("base", currency))
            .and(query_param("symbols", "EUR"))
            .respond_with(body)
            .mount(server)
            .await;
    }

    fn client(server: &MockServer) -> FxClient {
        FxClient::new(server.uri().parse().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn reads_a_rate() {
        let server = MockServer::start().await;
        serve(
            &server,
            "2026-09-25",
            "USD",
            ResponseTemplate::new(200).set_body_json(answer("USD", "2026-09-25", 0.8531)),
        )
        .await;
        let rate = client(&server)
            .rate(date(2026, 9, 25), "USD", "EUR")
            .await
            .unwrap();
        assert_eq!(
            rate,
            Rate {
                rate: 0.8531,
                rate_date: date(2026, 9, 25)
            }
        );
    }

    #[tokio::test]
    async fn a_weekend_uses_the_previous_business_days_rate() {
        let server = MockServer::start().await;
        // Saturday the 26th: the service answers with Friday's rate and date.
        serve(
            &server,
            "2026-09-26",
            "USD",
            ResponseTemplate::new(200).set_body_json(answer("USD", "2026-09-25", 0.8531)),
        )
        .await;
        let rate = client(&server)
            .rate(date(2026, 9, 26), "USD", "EUR")
            .await
            .unwrap();
        assert_eq!(rate.rate_date, date(2026, 9, 25));
    }

    #[tokio::test]
    async fn answers_for_the_wrong_period_are_refused() {
        let server = MockServer::start().await;
        // From the future, and from a month ago.
        serve(
            &server,
            "2026-09-25",
            "USD",
            ResponseTemplate::new(200).set_body_json(answer("USD", "2026-09-26", 0.85)),
        )
        .await;
        serve(
            &server,
            "2026-09-25",
            "GBP",
            ResponseTemplate::new(200).set_body_json(answer("GBP", "2026-08-25", 1.1)),
        )
        .await;
        // Exactly ten days before is the limit.
        serve(
            &server,
            "2026-09-25",
            "CHF",
            ResponseTemplate::new(200).set_body_json(answer("CHF", "2026-09-15", 1.05)),
        )
        .await;
        let c = client(&server);
        assert!(
            matches!(c.rate(date(2026, 9, 25), "USD", "EUR").await, Err(FxError::Invalid(m)) if m.contains("not for"))
        );
        assert!(matches!(
            c.rate(date(2026, 9, 25), "GBP", "EUR").await,
            Err(FxError::Invalid(_))
        ));
        assert!(c.rate(date(2026, 9, 25), "CHF", "EUR").await.is_ok());
    }

    #[tokio::test]
    async fn errors_are_told_apart() {
        let server = MockServer::start().await;
        serve(
            &server,
            "2026-09-25",
            "XXX",
            ResponseTemplate::new(404).set_body_json(json!({"message": "not found"})),
        )
        .await;
        serve(&server, "2026-09-25", "BTC", ResponseTemplate::new(422)).await;
        serve(&server, "2026-09-25", "AUD", ResponseTemplate::new(429)).await;
        serve(&server, "2026-09-25", "CAD", ResponseTemplate::new(503)).await;
        serve(
            &server,
            "2026-09-25",
            "CZK",
            ResponseTemplate::new(200).set_body_string("<html>"),
        )
        .await;
        serve(
            &server,
            "2026-09-25",
            "DKK",
            ResponseTemplate::new(200).set_body_json(json!({"date": "2026-09-25", "rates": {}})),
        )
        .await;
        serve(
            &server,
            "2026-09-25",
            "HUF",
            ResponseTemplate::new(200).set_body_json(json!({"rates": {"EUR": 0.0025}})),
        )
        .await;
        serve(
            &server,
            "2026-09-25",
            "ILS",
            ResponseTemplate::new(200).set_body_json(answer("ILS", "2026-09-25", 0.0)),
        )
        .await;
        serve(
            &server,
            "2026-09-25",
            "INR",
            ResponseTemplate::new(200).set_body_json(answer("INR", "2026-09-25", -1.0)),
        )
        .await;
        serve(
            &server,
            "2026-09-25",
            "ISK",
            ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAX_BODY + 1]),
        )
        .await;
        let c = client(&server);
        let rate = |cur: &'static str| {
            let c = c.clone();
            async move { c.rate(date(2026, 9, 25), cur, "EUR").await }
        };
        assert_eq!(rate("XXX").await, Err(FxError::Unsupported("XXX".into())));
        assert_eq!(rate("BTC").await, Err(FxError::Unsupported("BTC".into())));
        assert_eq!(rate("AUD").await, Err(FxError::RateLimited));
        assert!(matches!(rate("CAD").await, Err(FxError::Http(_))));
        for bad in ["CZK", "DKK", "HUF", "ILS", "INR", "ISK"] {
            assert!(matches!(rate(bad).await, Err(FxError::Invalid(_))), "{bad}");
        }
    }

    #[tokio::test]
    async fn a_service_that_is_down_is_an_error_not_a_panic() {
        let dead = FxClient::new("http://127.0.0.1:1".parse().unwrap()).unwrap();
        assert!(matches!(
            dead.rate(date(2026, 9, 25), "USD", "EUR").await,
            Err(FxError::Http(_))
        ));
    }

    fn service(server: &MockServer) -> Arc<Fx> {
        Fx::new(Some(client(server)), "EUR".into(), Family::default())
    }

    #[tokio::test]
    async fn conversion_is_exact_and_cached_by_day() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/2026-09-25"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer(
                "USD",
                "2026-09-25",
                0.9255,
            )))
            .expect(1) // the second conversion comes from the cache
            .mount(&server)
            .await;
        let store = Store::memory().await.unwrap();
        let fx = service(&server);
        // 10.00 USD at 0.9255 is exactly 9.255 EUR: rounds up to 9.26.
        let c = fx
            .convert(&store, 1000, "USD", date(2026, 9, 25))
            .await
            .unwrap();
        assert_eq!(
            (c.base_minor, c.rate, c.source),
            (926, 0.9255, FxSource::Ecb)
        );
        let again = fx
            .convert(&store, 4000, "USD", date(2026, 9, 25))
            .await
            .unwrap();
        assert_eq!(again.base_minor, 3702);
        // The cache survives in the database, so a new service uses it too.
        let other = service(&server);
        assert_eq!(
            other
                .convert(&store, 1000, "USD", date(2026, 9, 25))
                .await
                .unwrap()
                .base_minor,
            926
        );
    }

    #[tokio::test]
    async fn the_base_currency_needs_no_lookup() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let store = Store::memory().await.unwrap();
        let c = service(&server)
            .convert(&store, 1234, "EUR", date(2026, 9, 25))
            .await
            .unwrap();
        assert_eq!(
            (c.base_minor, c.rate, c.source),
            (1234, 1.0, FxSource::Same)
        );
        // Even with rates turned off.
        let off = Fx::new(None, "EUR".into(), Family::default());
        assert!(
            off.convert(&store, 1234, "EUR", date(2026, 9, 25))
                .await
                .is_ok()
        );
        assert_eq!(
            off.convert(&store, 1234, "USD", date(2026, 9, 25)).await,
            Err(FxError::Disabled)
        );
    }

    #[tokio::test]
    async fn unsupported_currencies_are_not_asked_about_again() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;
        let store = Store::memory().await.unwrap();
        let fx = service(&server);
        for day in [date(2026, 9, 25), date(2026, 9, 24), date(2026, 9, 23)] {
            assert_eq!(
                fx.convert(&store, 100, "BTC", day).await,
                Err(FxError::Unsupported("BTC".into()))
            );
        }
    }

    #[tokio::test]
    async fn dust_is_not_converted_to_nothing() {
        let server = MockServer::start().await;
        serve(
            &server,
            "2026-09-25",
            "JPY",
            ResponseTemplate::new(200).set_body_json(answer("JPY", "2026-09-25", 0.0004)),
        )
        .await;
        let store = Store::memory().await.unwrap();
        // 1 yen at 0.0004 euro is 0.04 of a cent: nothing that can be stored.
        assert_eq!(
            service(&server)
                .convert(&store, 1, "JPY", date(2026, 9, 25))
                .await,
            Err(FxError::Dust)
        );
    }

    #[tokio::test]
    async fn lookups_are_counted_by_result() {
        let server = MockServer::start().await;
        serve(
            &server,
            "2026-09-25",
            "USD",
            ResponseTemplate::new(200).set_body_json(answer("USD", "2026-09-25", 0.9)),
        )
        .await;
        serve(&server, "2026-09-25", "XXX", ResponseTemplate::new(404)).await;
        serve(&server, "2026-09-25", "AUD", ResponseTemplate::new(429)).await;
        serve(&server, "2026-09-25", "CAD", ResponseTemplate::new(503)).await;
        let store = Store::memory().await.unwrap();
        let counters: Family<ResultLabels, Counter> = Family::default();
        let fx = Fx::new(Some(client(&server)), "EUR".into(), counters.clone());
        for cur in ["USD", "XXX", "AUD", "CAD"] {
            let _ = fx.convert(&store, 100, cur, date(2026, 9, 25)).await;
        }
        let count = |r: &str| {
            counters
                .get_or_create(&ResultLabels { result: r.into() })
                .get()
        };
        assert_eq!(
            (
                count("ok"),
                count("unsupported"),
                count("rate_limited"),
                count("error")
            ),
            (1, 1, 1, 1)
        );
    }

    fn pending(currency: &str, minor: i64, day: jiff::civil::Date) -> NewDonation {
        NewDonation {
            project: "a-b".into(),
            platform: "direct".into(),
            amount_minor: minor,
            currency: currency.into(),
            donated_on: day,
            kind: DonationKind::OneOff,
            note: String::new(),
            conversion: None,
        }
    }

    async fn store_with_pending(donations: &[NewDonation]) -> Store {
        let store = Store::memory().await.unwrap();
        let who = Actor::new("u", "u");
        let now: jiff::Timestamp = "2026-09-26T12:00:00Z".parse().unwrap();
        store
            .create_project("github.com/a/b", 1.0, "", &who, now)
            .await
            .unwrap();
        for d in donations {
            store.record_donation(d, &who, now).await.unwrap();
        }
        store
    }

    #[tokio::test]
    async fn pending_donations_are_converted_later() {
        let server = MockServer::start().await;
        serve(
            &server,
            "2026-09-20",
            "USD",
            ResponseTemplate::new(200).set_body_json(answer("USD", "2026-09-18", 0.8)),
        )
        .await;
        serve(&server, "2026-09-21", "BTC", ResponseTemplate::new(404)).await;
        let store = store_with_pending(&[
            pending("USD", 1000, date(2026, 9, 20)),
            pending("BTC", 5, date(2026, 9, 21)),
        ])
        .await;
        let fx = service(&server);
        assert_eq!(fx.fill_pending(&store).await.unwrap(), 1);
        let all = store.list_donations(None).await.unwrap();
        let usd = all.iter().find(|d| d.currency == "USD").unwrap();
        let c = usd.conversion.unwrap();
        assert_eq!((c.base_minor, c.source), (800, FxSource::Ecb));
        let btc = all.iter().find(|d| d.currency == "BTC").unwrap();
        assert_eq!(btc.conversion, None, "left for someone to enter a rate");
        assert_eq!(store.pending_conversions(10).await.unwrap().len(), 1);
        // Nothing more to do: a second pass converts nothing and asks no one.
        assert_eq!(fx.fill_pending(&store).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_rate_limited_service_stops_the_pass() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .expect(1) // the second donation is not even tried
            .mount(&server)
            .await;
        let store = store_with_pending(&[
            pending("USD", 1000, date(2026, 9, 20)),
            pending("GBP", 1000, date(2026, 9, 21)),
        ])
        .await;
        assert_eq!(service(&server).fill_pending(&store).await.unwrap(), 0);
        assert_eq!(store.pending_conversions(10).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn turned_off_rates_leave_donations_pending() {
        let store = store_with_pending(&[pending("USD", 1000, date(2026, 9, 20))]).await;
        let off = Fx::new(None, "EUR".into(), Family::default());
        assert!(!off.enabled());
        assert_eq!(off.fill_pending(&store).await.unwrap(), 0);
        assert_eq!(store.pending_conversions(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn voided_and_converted_donations_are_not_pending() {
        let store = store_with_pending(&[
            pending("USD", 1000, date(2026, 9, 20)),
            pending("USD", 2000, date(2026, 9, 21)),
        ])
        .await;
        let who = Actor::new("u", "u");
        let all = store.list_donations(None).await.unwrap();
        store
            .void_donation(all[0].id, &who, "2026-09-27T00:00:00Z".parse().unwrap())
            .await
            .unwrap();
        let left = store.pending_conversions(10).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].amount_minor, 1000);

        // Set by hand: once.
        let conversion = Conversion {
            base_minor: 900,
            rate: 0.9,
            source: FxSource::Manual,
        };
        store.set_conversion(left[0].id, &conversion).await.unwrap();
        assert!(matches!(
            store.set_conversion(left[0].id, &conversion).await,
            Err(StoreError::InUse(_))
        ));
        assert!(matches!(
            store.set_conversion(9999, &conversion).await,
            Err(StoreError::NotFound)
        ));
        assert!(store.pending_conversions(10).await.unwrap().is_empty());
    }
}
