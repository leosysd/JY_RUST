use anyhow::{bail, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub slug: String,
    pub title: String,
    pub start_ts: i64,
    pub end_ts: i64,
    pub outcomes: Vec<String>,
    pub token_ids: Vec<String>,
    pub neg_risk: bool,
}

impl Market {
    pub fn seconds_left(&self) -> i64 {
        let now = chrono::Utc::now().timestamp();
        (self.end_ts - now).max(0)
    }

    pub fn seconds_elapsed(&self) -> i64 {
        let now = chrono::Utc::now().timestamp();
        (now - self.start_ts).max(0)
    }

    pub fn token_for(&self, outcome: &str) -> Option<&str> {
        self.outcomes
            .iter()
            .position(|o| o == outcome)
            .and_then(|i| self.token_ids.get(i))
            .map(|s| s.as_str())
    }
}

#[derive(Debug, Clone, Default)]
pub struct OrderBook {
    pub asks: Vec<(Decimal, Decimal)>, // (price, size), sorted ascending
    pub bids: Vec<(Decimal, Decimal)>, // sorted descending
    pub tick_size: Decimal,
    pub min_order_size: Decimal,
    pub neg_risk: bool,
}

impl OrderBook {
    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.first().map(|(p, _)| *p)
    }
}

pub type BookCache = Arc<RwLock<HashMap<String, OrderBook>>>;

pub fn new_book_cache() -> BookCache {
    Arc::new(RwLock::new(HashMap::new()))
}

pub struct ClobClient {
    http: reqwest::Client,
    pub clob_api: String,
    pub gamma_api: String,
    pub slug_prefix: String,
}

impl ClobClient {
    pub fn new(clob_api: &str, gamma_api: &str, slug_prefix: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("failed to build http client");
        Self {
            http,
            clob_api: clob_api.to_string(),
            gamma_api: gamma_api.to_string(),
            slug_prefix: slug_prefix.to_string(),
        }
    }

    pub async fn find_current_market(&self) -> Option<Market> {
        let now = chrono::Utc::now().timestamp();
        let slot = (now / 300) * 300;
        for candidate in [slot, slot - 300, slot + 300] {
            if let Some(m) = self.fetch_market(candidate).await {
                if m.start_ts <= now && now < m.end_ts {
                    return Some(m);
                }
            }
        }
        None
    }

    async fn fetch_market(&self, start_ts: i64) -> Option<Market> {
        let slug = format!("{}-{}", self.slug_prefix, start_ts);
        let url = format!("{}/events/slug/{}", self.gamma_api, slug);
        let resp: serde_json::Value = self.http.get(&url).send().await.ok()?.json().await.ok()?;

        let markets = resp.get("markets")?.as_array()?;
        let market = markets.first()?;

        if market.get("acceptingOrders")?.as_bool() != Some(true) {
            return None;
        }

        let outcomes = parse_json_list(market.get("outcomes")?)?;
        let token_ids = parse_json_list(market.get("clobTokenIds")?)?;
        if outcomes.len() != token_ids.len() {
            return None;
        }
        if !outcomes.contains(&"Up".to_string()) || !outcomes.contains(&"Down".to_string()) {
            return None;
        }

        let event_start = market
            .get("eventStartTime")
            .or_else(|| resp.get("startTime"))
            .and_then(|v| v.as_str())?;
        let end_date = market
            .get("endDate")
            .or_else(|| resp.get("endDate"))
            .and_then(|v| v.as_str())?;

        Some(Market {
            slug: slug.clone(),
            title: market
                .get("question")
                .or_else(|| resp.get("title"))
                .and_then(|v| v.as_str())
                .unwrap_or(&slug)
                .to_string(),
            start_ts: iso_to_ts(event_start)?,
            end_ts: iso_to_ts(end_date)?,
            outcomes: outcomes.clone(),
            token_ids,
            neg_risk: market.get("negRisk").and_then(|v| v.as_bool()).unwrap_or(false),
        })
    }

    pub async fn fetch_book(&self, token_id: &str) -> Result<OrderBook> {
        let url = format!("{}/book?token_id={}", self.clob_api, token_id);
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await?
            .json()
            .await?;

        if resp.get("error").is_some() {
            bail!("book error for {}: {}", token_id, resp);
        }

        Ok(parse_book(&resp))
    }

    pub async fn get_book(&self, token_id: &str, cache: &BookCache) -> Result<OrderBook> {
        {
            let guard = cache.read().await;
            if let Some(b) = guard.get(token_id) {
                return Ok(b.clone());
            }
        }
        let book = self.fetch_book(token_id).await?;
        {
            let mut guard = cache.write().await;
            guard.insert(token_id.to_string(), book.clone());
        }
        Ok(book)
    }
}

fn parse_json_list(v: &serde_json::Value) -> Option<Vec<String>> {
    match v {
        serde_json::Value::Array(arr) => Some(
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect(),
        ),
        serde_json::Value::String(s) => {
            serde_json::from_str::<Vec<String>>(s).ok().or_else(|| {
                let trimmed = s.trim_matches(|c| c == '[' || c == ']');
                Some(
                    trimmed
                        .split(',')
                        .map(|p| p.trim().trim_matches('"').to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                )
            })
        }
        _ => None,
    }
}

fn iso_to_ts(s: &str) -> Option<i64> {
    let fixed = s.replace('Z', "+00:00");
    chrono::DateTime::parse_from_rfc3339(&fixed)
        .ok()
        .map(|dt| dt.timestamp())
}

pub fn parse_book(v: &serde_json::Value) -> OrderBook {
    let parse_levels = |arr: Option<&serde_json::Value>| -> Vec<(Decimal, Decimal)> {
        let Some(serde_json::Value::Array(levels)) = arr else {
            return vec![];
        };
        let mut out: Vec<(Decimal, Decimal)> = levels
            .iter()
            .filter_map(|l| {
                let price = Decimal::from_str(l.get("price")?.as_str()?).ok()?;
                let size = Decimal::from_str(l.get("size")?.as_str()?).ok()?;
                Some((price, size))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };

    OrderBook {
        asks: parse_levels(v.get("asks")),
        bids: {
            let mut bids = parse_levels(v.get("bids"));
            bids.sort_by(|a, b| b.0.cmp(&a.0));
            bids
        },
        tick_size: v
            .get("tick_size")
            .and_then(|x| x.as_str())
            .and_then(|s| Decimal::from_str(s).ok())
            .unwrap_or(Decimal::new(1, 2)),
        min_order_size: v
            .get("min_order_size")
            .and_then(|x| x.as_str())
            .and_then(|s| Decimal::from_str(s).ok())
            .unwrap_or(Decimal::ONE),
        neg_risk: v.get("neg_risk").and_then(|x| x.as_bool()).unwrap_or(false),
    }
}
