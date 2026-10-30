use crate::common::*;
use goon::cost::*;

fn price() -> Price {
    Price { input_per_1m: 10.0, output_per_1m: 20.0, cached_input_per_1m: Some(1.0), cache_write_per_1m: Some(2.0) }
}

fn table(models: &[(&str, Price)], default: Option<Price>) -> CostPricingTable {
    CostPricingTable { default, models: models.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() }
}

fn split(input: u64, output: u64, cached: Option<u64>, write: Option<u64>) -> UsageSplit {
    UsageSplit { input_tokens: input, output_tokens: output, total_tokens: input + output, cached_input_tokens: cached, cache_write_tokens: write }
}

fn budget(cost_spent: f64) -> SharedBudget {
    Arc::new(Mutex::new(RunBudget { spent: 0, limit: 0, cost_spent, cost_limit: 0.0, started_at: None, time_limit_ms: 0, ledger: None }))
}

fn price_json() -> Value {
    json!({ "inputPer1M": 10, "outputPer1M": 20, "cachedInputPer1M": 1, "cacheWritePer1M": 2 })
}

#[test]
fn compute_cost_splits_uncached_cached_cache_write_and_output() {
    let ledger = CostLedger::new(table(&[("m", price())], None), Some("m".into()));
    assert_eq!(ledger.compute_cost(&split(1_000_000, 500_000, Some(200_000), Some(100_000)), None), 17.4);
}

#[test]
fn missing_cached_rates_fall_back_to_the_input_rate() {
    let ledger = CostLedger::new(table(&[("m", Price { input_per_1m: 10.0, output_per_1m: 20.0, cached_input_per_1m: None, cache_write_per_1m: None })], None), Some("m".into()));
    assert_eq!(ledger.compute_cost(&split(1_000_000, 0, Some(500_000), None), None), 10.0);
}

#[test]
fn default_price_is_used_when_the_model_has_no_explicit_entry() {
    let ledger = CostLedger::new(table(&[], Some(Price { input_per_1m: 3.0, output_per_1m: 4.0, cached_input_per_1m: None, cache_write_per_1m: None })), Some("unknown".into()));
    assert_eq!(ledger.compute_cost(&split(1_000_000, 0, None, None), None), 3.0);
}

#[test]
fn flat_and_nested_pricing_shapes_normalize_to_the_same_table() {
    let flat = normalize_pricing(Some(&json!({ "m": price_json(), "default": price_json() })));
    let nested = normalize_pricing(Some(&json!({ "models": { "m": price_json() }, "default": price_json() })));
    assert_eq!(flat.models["m"], price());
    assert_eq!(nested.models["m"], price());
    assert_eq!(flat.default, Some(price()));
}

#[test]
fn max_cost_with_no_price_fails_closed() {
    let config = config::parse_config(&json!({ "model": "unknown", "maxCostUsd": 1, "pricing": { "other": price_json() } })).unwrap();
    let error = CostLedger::from_config(&config, None).unwrap_err();
    assert_match(&error.0, "No price configured");
    let free = config::parse_config(&json!({ "model": "unknown", "maxCostUsd": 0, "pricing": { "other": price_json() } })).unwrap();
    assert!(CostLedger::from_config(&free, None).is_ok());
}

#[test]
fn account_mirrors_the_ledger_total_into_the_budget() {
    let ledger = Arc::new(CostLedger::new(table(&[("m", price())], None), Some("m".into())));
    let b = budget(0.0);
    let delta = ledger.account(Some(&b), &split(1_000_000, 0, None, None), Some("m"));
    assert_eq!(delta, 10.0);
    assert_eq!(ledger.total(), 10.0);
    assert_eq!(b.lock().unwrap().cost_spent, 10.0);
}

#[test]
fn register_run_seeds_a_resumed_session_cost_without_resetting_the_mirror() {
    let ledger = Arc::new(CostLedger::new(table(&[("m", price())], None), Some("m".into())));
    let b = budget(5.0);
    ledger.register_run(&b);
    assert_eq!(ledger.total(), 5.0);
    ledger.account(Some(&b), &split(1_000_000, 0, None, None), Some("m"));
    assert_eq!(ledger.total(), 15.0);
    assert_eq!(b.lock().unwrap().cost_spent, 15.0);
}

#[test]
fn hard_caps_detect_cost_and_time_budgets() {
    let now = jsutil::now_millis();
    let cost = RunBudget { spent: 0, limit: 0, cost_spent: 2.0, cost_limit: 2.0, started_at: Some(now), time_limit_ms: 0, ledger: None };
    assert!(CostLedger::hard_cap_exceeded(&cost).exceeded);
    let time = RunBudget { spent: 0, limit: 0, cost_spent: 0.0, cost_limit: 0.0, started_at: Some(now - 100), time_limit_ms: 10, ledger: None };
    assert!(CostLedger::hard_cap_exceeded(&time).exceeded);
}

#[test]
fn create_run_budget_fills_configured_caps_without_resetting_a_shared_budget() {
    let shared: SharedBudget = Arc::new(Mutex::new(RunBudget { spent: 3, limit: 10, cost_spent: 1.0, cost_limit: 0.0, started_at: None, time_limit_ms: 0, ledger: None }));
    let config = config::parse_config(&json!({ "runTokenBudget": 99, "maxCostUsd": 5, "maxTimeMs": 1000 })).unwrap();
    let result = create_run_budget(&config, Some(shared.clone()));
    assert!(Arc::ptr_eq(&result, &shared));
    let b = result.lock().unwrap();
    assert_eq!((b.spent, b.limit, b.cost_limit, b.time_limit_ms), (3, 10, 5.0, 1000));
    assert!(b.started_at.is_some());
}

#[test]
fn to_usage_split_reads_token_details_and_estimates_fallbacks() {
    let usage = model::Usage { input_tokens: Some(100), output_tokens: Some(20), cache_read_tokens: Some(30), cache_write_tokens: Some(10), reasoning_tokens: None };
    assert_eq!(to_usage_split(Some(&usage), 0, 0), split(100, 20, Some(30), Some(10)));
    assert_eq!(to_usage_split(None, 7, 3), split(7, 3, None, None));
}

#[test]
fn format_usd_never_prints_negative_or_nan_values() {
    assert_eq!(format_usd(1.23456), "$1.2346");
    assert_eq!(format_usd(-1.0), "$0.0000");
    assert_eq!(format_usd(f64::NAN), "$0.0000");
}

fn harness_fixture(f: &Fixture) -> (Arc<harness::Harness>, Arc<Mutex<Vec<Value>>>) {
    let rt = runtime(&f.cwd, &f.data, "http://127.0.0.1:9/v1", json!({ "maxCostUsd": 0, "pricing": { "test-model": { "inputPer1M": 1000, "outputPer1M": 2000 } } }));
    let st = store(&rt);
    let tools = registry(Path::new("/tmp"), tools::approve_all(), false);
    let (emit, log) = events();
    (harness::Harness::new(rt, st, tools, emit, None, Default::default()).unwrap(), log)
}

fn usage(input: u64, output: u64) -> model::Usage {
    model::Usage { input_tokens: Some(input), output_tokens: Some(output), ..Default::default() }
}

#[test]
fn harness_account_usage_updates_session_canonical_and_budget_mirror() {
    let f = fixture();
    let (h, log) = harness_fixture(&f);
    let b = budget(0.0);
    h.cost_ledger.register_run(&b);
    h.account_usage(Some(&usage(1000, 500)), Some("test-model"), Some(&b), 0, 0);
    assert_eq!(h.cost_ledger.total(), 2.0);
    assert_eq!(h.store.session.lock().unwrap().cost(), 2.0);
    assert_eq!(b.lock().unwrap().cost_spent, 2.0);
    let events = log.lock().unwrap();
    let usage = events.iter().find(|e| e["type"] == "usage").unwrap();
    assert_eq!(usage["costUsd"], 2);
    assert_eq!(usage["total"], 1500);
}

#[test]
fn child_harness_never_clobbers_a_shared_budget_with_its_own_session_cost() {
    let f = fixture();
    let parent =
        Arc::new(CostLedger::new(table(&[("test-model", Price { input_per_1m: 1.0, output_per_1m: 0.0, cached_input_per_1m: None, cache_write_per_1m: None })], None), Some("test-model".into())));
    *parent.total.lock().unwrap() = 5.0;
    let shared: SharedBudget = Arc::new(Mutex::new(RunBudget { spent: 0, limit: 0, cost_spent: 5.0, cost_limit: 0.0, started_at: None, time_limit_ms: 0, ledger: Some(parent.clone()) }));
    let (child, _) = harness_fixture(&f);
    child.account_usage(Some(&usage(1_000_000, 0)), Some("test-model"), Some(&shared), 0, 0);
    assert_eq!(parent.total(), 6.0);
    assert_eq!(shared.lock().unwrap().cost_spent, 6.0);
    assert_eq!(child.store.session.lock().unwrap().cost(), 1.0);
    assert_eq!(child.cost_ledger.total(), 0.0);
}
