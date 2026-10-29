//! Run cost accounting.
//!
//! - `CostLedger::total` is canonical for one ledger (a run, or a parent + children sharing it).
//! - `session.costUsd` is canonical for a session and survives resume.
//! - `RunBudget::cost_spent` mirrors the shared ledger's total, never a child session.
//! - The ledger fails closed: `maxCostUsd > 0` with no price for the model is an error before any
//!   provider request is made.

use crate::config::Config;
use crate::jsutil::{js_round, to_fixed};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub struct Price {
    pub input_per_1m: f64,
    pub output_per_1m: f64,
    pub cached_input_per_1m: Option<f64>,
    pub cache_write_per_1m: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct CostPricingTable {
    pub default: Option<Price>,
    pub models: indexmap::IndexMap<String, Price>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageSplit {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

impl UsageSplit {
    /// The `usage` event payload (`{input, output, total[, cached][, cacheWrite]}`).
    pub fn event_fields(&self, map: &mut Map<String, Value>) {
        map.insert("input".into(), self.input_tokens.into());
        map.insert("output".into(), self.output_tokens.into());
        map.insert("total".into(), self.total_tokens.into());
        if let Some(c) = self.cached_input_tokens {
            map.insert("cached".into(), c.into());
        }
        if let Some(c) = self.cache_write_tokens {
            map.insert("cacheWrite".into(), c.into());
        }
    }
}

#[derive(Debug)]
pub struct CostPricingError(pub String);

impl std::fmt::Display for CostPricingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CostPricingError {}

fn as_price(value: &Value) -> Option<Price> {
    let o = value.as_object()?;
    let input = o.get("inputPer1M")?.as_f64()?;
    let output = o.get("outputPer1M")?.as_f64()?;
    Some(Price {
        input_per_1m: input,
        output_per_1m: output,
        cached_input_per_1m: o.get("cachedInputPer1M").and_then(Value::as_f64),
        cache_write_per_1m: o.get("cacheWritePer1M").and_then(Value::as_f64),
    })
}

/// Accepts the flat (`{ "model": {...}, "default": {...} }`) and nested (`{ "models": {...} }`) shapes.
pub fn normalize_pricing(pricing: Option<&Value>) -> CostPricingTable {
    let mut table = CostPricingTable::default();
    let Some(obj) = pricing.and_then(Value::as_object) else { return table };
    for (key, value) in obj {
        if key == "models" && value.is_object() && as_price(value).is_none() {
            for (model, price) in value.as_object().unwrap() {
                if let Some(p) = as_price(price) {
                    table.models.insert(model.clone(), p);
                }
            }
            continue;
        }
        if key == "default" {
            if let Some(p) = as_price(value) {
                table.default = Some(p);
                continue;
            }
        }
        if let Some(p) = as_price(value) {
            table.models.insert(key.clone(), p);
        }
    }
    table
}

/// Normalizes provider usage, falling back to local estimates when the provider omits it.
pub fn to_usage_split(usage: Option<&crate::model::Usage>, fallback_input: u64, fallback_output: u64) -> UsageSplit {
    let input = usage.and_then(|u| u.input_tokens).unwrap_or(fallback_input);
    let output = usage.and_then(|u| u.output_tokens).unwrap_or(fallback_output);
    let total = usage.and_then(|u| u.total_tokens()).unwrap_or(input + output);
    let cached = usage.and_then(|u| u.cache_read_tokens).unwrap_or(0);
    let cache_write = usage.and_then(|u| u.cache_write_tokens).unwrap_or(0);
    UsageSplit {
        input_tokens: input,
        output_tokens: output,
        total_tokens: total,
        cached_input_tokens: (cached > 0).then_some(cached),
        cache_write_tokens: (cache_write > 0).then_some(cache_write),
    }
}

pub fn format_usd(value: f64) -> String {
    let safe = if value.is_finite() && value > 0.0 { value } else { 0.0 };
    format!("${}", to_fixed(safe, 4))
}

pub fn format_duration_ms(ms: f64) -> String {
    let safe = if ms.is_finite() && ms > 0.0 { ms } else { 0.0 };
    if safe < 1000.0 {
        return format!("{}ms", js_round(safe) as i64);
    }
    if safe < 60_000.0 {
        return format!("{}s", to_fixed(safe / 1000.0, 1));
    }
    let minutes = (safe / 60_000.0).floor() as i64;
    let seconds = js_round((safe % 60_000.0) / 1000.0) as i64;
    format!("{minutes}m {seconds:02}s")
}

/// Per-run budget shared by a parent harness and its children.
#[derive(Debug, Default)]
pub struct RunBudget {
    pub spent: u64,
    pub limit: u64,
    pub cost_spent: f64,
    pub cost_limit: f64,
    pub started_at: Option<i64>,
    pub time_limit_ms: u64,
    pub ledger: Option<Arc<CostLedger>>,
}

pub type SharedBudget = Arc<Mutex<RunBudget>>;

pub fn create_run_budget(config: &Config, shared: Option<SharedBudget>) -> SharedBudget {
    match shared {
        Some(b) => {
            {
                // `costLimit ??= …` / `timeLimitMs ??= …`: a zero cap on a shared budget means unset.
                let mut budget = b.lock().unwrap();
                if budget.cost_limit == 0.0 {
                    budget.cost_limit = config.max_cost_usd;
                }
                if budget.time_limit_ms == 0 {
                    budget.time_limit_ms = config.max_time_ms;
                }
                if budget.started_at.is_none() {
                    budget.started_at = Some(crate::jsutil::now_millis());
                }
            }
            b
        }
        None => Arc::new(Mutex::new(RunBudget {
            spent: 0,
            limit: config.run_token_budget,
            cost_spent: 0.0,
            cost_limit: config.max_cost_usd,
            started_at: Some(crate::jsutil::now_millis()),
            time_limit_ms: config.max_time_ms,
            ledger: None,
        })),
    }
}

pub struct CapCheck {
    pub exceeded: bool,
    pub reason: String,
}

/// Prices are per one million tokens. Cached/cache-write rates fall back to the input rate.
#[derive(Debug)]
pub struct CostLedger {
    pub pricing: CostPricingTable,
    pub model: Option<String>,
    pub total: Mutex<f64>,
}

impl CostLedger {
    pub fn new(pricing: CostPricingTable, model: Option<String>) -> Self {
        CostLedger { pricing, model, total: Mutex::new(0.0) }
    }

    pub fn from_config(config: &Config, model: Option<&str>) -> Result<Arc<CostLedger>, CostPricingError> {
        let model = model.map(str::to_string).or_else(|| config.model.clone());
        let ledger = CostLedger::new(normalize_pricing(config.pricing.as_ref()), model.clone());
        if config.max_cost_usd > 0.0 {
            if let Some(m) = &model {
                if ledger.price_for(Some(m)).is_none() {
                    let configured = ledger.configured_models();
                    return Err(CostPricingError(format!(
                        "No price configured for model \"{m}\" while maxCostUsd > 0. Add pricing[\"{m}\"] (or pricing.default) to the goon config. Configured models: {}.",
                        if configured.is_empty() { "none".to_string() } else { configured.join(", ") }
                    )));
                }
            }
        }
        Ok(Arc::new(ledger))
    }

    pub fn configured_models(&self) -> Vec<String> {
        self.pricing.models.keys().cloned().collect()
    }

    pub fn price_for(&self, model: Option<&str>) -> Option<&Price> {
        match model {
            None => self.pricing.default.as_ref(),
            Some(m) => self.pricing.models.get(m).or(self.pricing.default.as_ref()),
        }
    }

    pub fn total(&self) -> f64 {
        *self.total.lock().unwrap()
    }

    pub fn compute_cost(&self, usage: &UsageSplit, model: Option<&str>) -> f64 {
        let Some(price) = self.price_for(model.or(self.model.as_deref())) else { return 0.0 };
        let input = usage.input_tokens as f64;
        let output = usage.output_tokens as f64;
        let cached = input.min(usage.cached_input_tokens.unwrap_or(0) as f64);
        let cache_write = (input - cached).max(0.0).min(usage.cache_write_tokens.unwrap_or(0) as f64);
        let uncached = (input - cached - cache_write).max(0.0);
        let input_rate = price.input_per_1m;
        let micro = uncached * input_rate + cached * price.cached_input_per_1m.unwrap_or(input_rate) + cache_write * price.cache_write_per_1m.unwrap_or(input_rate) + output * price.output_per_1m;
        micro.max(0.0) / 1_000_000.0
    }

    /// Links a run budget to this ledger unless it already shares another ledger, and seeds the
    /// ledger total from the budget mirror so resumed sessions keep counting.
    pub fn register_run(self: &Arc<Self>, budget: &SharedBudget) -> Arc<CostLedger> {
        let mut b = budget.lock().unwrap();
        if b.started_at.is_none() {
            b.started_at = Some(crate::jsutil::now_millis());
        }
        let ledger = b.ledger.clone().unwrap_or_else(|| self.clone());
        b.ledger = Some(ledger.clone());
        let mut total = ledger.total.lock().unwrap();
        if *total < b.cost_spent {
            *total = b.cost_spent;
        }
        drop(total);
        ledger
    }

    /// Adds one request's usage to the shared ledger and mirrors the total. Returns the delta.
    pub fn account(self: &Arc<Self>, budget: Option<&SharedBudget>, usage: &UsageSplit, model: Option<&str>) -> f64 {
        let ledger = budget.and_then(|b| b.lock().unwrap().ledger.clone()).unwrap_or_else(|| self.clone());
        let cost = ledger.compute_cost(usage, model.or(self.model.as_deref()));
        let total = {
            let mut t = ledger.total.lock().unwrap();
            *t += cost;
            *t
        };
        if let Some(b) = budget {
            let mut b = b.lock().unwrap();
            b.cost_spent = b.cost_spent.max(total);
        }
        cost
    }

    pub fn hard_cap_exceeded(budget: &RunBudget) -> CapCheck {
        if budget.cost_limit > 0.0 && budget.cost_spent >= budget.cost_limit {
            return CapCheck { exceeded: true, reason: format!("Run cost budget reached ({} of {}).", format_usd(budget.cost_spent), format_usd(budget.cost_limit)) };
        }
        if budget.time_limit_ms > 0 {
            if let Some(started) = budget.started_at {
                let elapsed = (crate::jsutil::now_millis() - started) as f64;
                if elapsed >= budget.time_limit_ms as f64 {
                    return CapCheck {
                        exceeded: true,
                        reason: format!("Run time budget reached ({} of {}).", format_duration_ms(elapsed), format_duration_ms(budget.time_limit_ms as f64)),
                    };
                }
            }
        }
        CapCheck { exceeded: false, reason: String::new() }
    }

    pub fn describe(budget: Option<&RunBudget>, session_cost_usd: f64) -> String {
        let mut parts = vec![format!("Session cost {}", format_usd(session_cost_usd))];
        if let Some(b) = budget {
            parts.push(format!("run {}{}", format_usd(b.cost_spent), if b.cost_limit > 0.0 { format!(" / {}", format_usd(b.cost_limit)) } else { String::new() }));
            if b.time_limit_ms > 0 {
                if let Some(started) = b.started_at {
                    parts.push(format!("time {} / {}", format_duration_ms((crate::jsutil::now_millis() - started) as f64), format_duration_ms(b.time_limit_ms as f64)));
                }
            }
        }
        parts.join(" · ")
    }

    /// Returns the warning when cost enforcement cannot work (missing or zero price).
    pub fn inert_warning(&self, model: Option<&str>) -> Option<String> {
        let model = model.or(self.model.as_deref());
        match self.price_for(model) {
            None => Some(format!("goon: no price configured for model \"{}\"; token caps still apply but cost tracking is inactive.", model.unwrap_or("(unknown)"))),
            Some(p) if p.input_per_1m == 0.0 && p.output_per_1m == 0.0 => Some(format!("goon: pricing for model \"{}\" is zero; maxCostUsd cannot stop a run.", model.unwrap_or(""))),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pricing_shapes_and_cost() {
        let table = normalize_pricing(Some(&json!({"models": {"m": {"inputPer1M": 1.0, "outputPer1M": 2.0, "cachedInputPer1M": 0.5}}, "default": {"inputPer1M": 3, "outputPer1M": 4}})));
        assert!(table.models.contains_key("m"));
        let ledger = Arc::new(CostLedger::new(table, Some("m".into())));
        let usage = UsageSplit { input_tokens: 1_000_000, output_tokens: 1_000_000, total_tokens: 2_000_000, cached_input_tokens: Some(500_000), cache_write_tokens: None };
        assert!((ledger.compute_cost(&usage, None) - 2.75).abs() < 1e-9);
        assert_eq!(format_usd(0.123456), "$0.1235");
        assert_eq!(format_duration_ms(61_500.0), "1m 02s");
        assert_eq!(format_duration_ms(999.4), "999ms");
        assert_eq!(format_duration_ms(1500.0), "1.5s");
    }
}
