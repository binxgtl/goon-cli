use crate::common::*;
use std::time::Duration;

fn harness_for(f: &Fixture, url: &str, registry: Arc<tools::ToolRegistry>) -> (Arc<harness::Harness>, Arc<session::SessionStore>) {
    let rt = runtime(&f.cwd, &f.data, url, json!({ "timeoutMs": 3000, "runTokenBudget": 100000 }));
    let st = store(&rt);
    (harness::Harness::new(rt, st.clone(), registry, harness::noop_emit(), None, Default::default()).unwrap(), st)
}

#[tokio::test]
async fn bounded_parallel_reads_drain_before_mutation_and_results_keep_model_order() {
    let f = fixture();
    let calls_spec: Vec<ToolCallSpec> = ["slow_read", "fast_read", "third_read", "mutate"].iter().enumerate().map(|(i, n)| tc(&format!("call{i}"), n, json!({}))).collect();
    let order_ok = Arc::new(Mutex::new(None));
    let ok = order_ok.clone();
    let p = provider(move |body, n| {
        if n == 1 {
            return calls(calls_spec.clone());
        }
        let ids: Vec<String> = body["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").map(|m| m["tool_call_id"].as_str().unwrap().to_string()).collect();
        *ok.lock().unwrap() = Some(ids);
        text("done")
    })
    .await;
    let reg = registry(&f.cwd, tools::approve_all(), false);
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let order = Arc::new(Mutex::new(Vec::<String>::new()));
    for (name, delay) in [("slow_read", 40u64), ("fast_read", 5), ("third_read", 5)] {
        let (a, m, o) = (active.clone(), maximum.clone(), order.clone());
        let exec: tools::ToolExec = Arc::new(move |_, _| {
            let (a, m, o) = (a.clone(), m.clone(), o.clone());
            Box::pin(async move {
                let now = a.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(now, Ordering::SeqCst);
                o.lock().unwrap().push(format!("start:{name}"));
                tokio::time::sleep(Duration::from_millis(delay)).await;
                a.fetch_sub(1, Ordering::SeqCst);
                o.lock().unwrap().push(format!("done:{name}"));
                Ok(name.to_string())
            })
        });
        reg.register_json(name, "test read", &json!({ "type": "object", "properties": {} }), true, None, None, None, exec).unwrap();
    }
    let (a, o) = (active.clone(), order.clone());
    let mutate: tools::ToolExec = Arc::new(move |_, _| {
        assert_eq!(a.load(Ordering::SeqCst), 0);
        o.lock().unwrap().push("mutate".into());
        Box::pin(async { Ok("mutated".to_string()) })
    });
    reg.register_json("mutate", "test mutation", &json!({ "type": "object", "properties": {} }), false, None, None, None, mutate).unwrap();
    let (h, st) = harness_for(&f, &p.url, reg);
    h.run("test", &token(), None).await.unwrap();
    st.close().unwrap();
    assert_eq!(maximum.load(Ordering::SeqCst), 3);
    let order = order.lock().unwrap();
    let pos = |s: &str| order.iter().position(|x| x == s).unwrap();
    assert!(pos("mutate") > pos("done:third_read"));
    assert_eq!(order_ok.lock().unwrap().clone().unwrap(), vec!["call0", "call1", "call2", "call3"]);
}

#[tokio::test]
async fn cancelled_tools_are_drained_and_every_unstarted_call_gets_a_terminal_result() {
    let f = fixture();
    let p = provider(|_, _| calls(vec![tc("slow", "slow", json!({})), tc("later", "mutate", json!({}))])).await;
    let reg = registry(&f.cwd, tools::approve_all(), false);
    let cancel = token();
    let drained = Arc::new(AtomicUsize::new(0));
    let mutated = Arc::new(AtomicUsize::new(0));
    let (c, d) = (cancel.clone(), drained.clone());
    let slow: tools::ToolExec = Arc::new(move |_, signal: CancellationToken| {
        let (c, d) = (c.clone(), d.clone());
        Box::pin(async move {
            c.cancel();
            let aborted = tokio::select! {
                _ = signal.cancelled() => true,
                _ = tokio::time::sleep(Duration::from_millis(50)) => false,
            };
            d.fetch_add(1, Ordering::SeqCst);
            if aborted {
                return Err(anyhow!("This operation was aborted"));
            }
            Ok("never".to_string())
        })
    });
    reg.register_json("slow", "cancel fixture", &json!({ "type": "object", "properties": {} }), true, None, None, None, slow).unwrap();
    let m = mutated.clone();
    let mutate: tools::ToolExec = Arc::new(move |_, _| {
        m.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("bad".to_string()) })
    });
    reg.register_json("mutate", "must not run", &json!({ "type": "object", "properties": {} }), false, None, None, None, mutate).unwrap();
    let (h, st) = harness_for(&f, &p.url, reg);
    assert!(h.run("test", &cancel, None).await.is_err());
    assert_eq!(drained.load(Ordering::SeqCst), 1);
    assert_eq!(mutated.load(Ordering::SeqCst), 0);
    let session = st.session.lock().unwrap();
    let results: Vec<Value> = session.messages.iter().filter(|m| m["role"] == "tool").flat_map(|m| m["content"].as_array().cloned().unwrap_or_default()).collect();
    assert_eq!(results.iter().map(|r| r["toolCallId"].as_str().unwrap()).collect::<Vec<_>>(), vec!["slow", "later"]);
    assert!(results.iter().all(|r| r["output"]["type"] == "error-text"));
}
