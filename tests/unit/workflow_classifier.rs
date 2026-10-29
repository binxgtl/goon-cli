use crate::common::*;
use goon::workflow::{classify_workflow_complexity, execute_workflow, format_workflow_details, parse_markdown_workflow, WorkflowDefinition, WorkflowHooks, WorkflowStep};

fn classify(prompt: &str) -> workflow::WorkflowClassification {
    classify_workflow_complexity(prompt, 0)
}

#[test]
fn conservative_default_is_single_for_single_action_or_ambiguous_prompts() {
    for prompt in [
        "fix the login bug",
        "add a test for parseUser",
        "refactor src/utils.ts",
        "write docs for the CLI",
        "update package.json",
        "run the tests",
        "review the code and fix issues if needed",
        "plan the feature",
        "chạy kiểm tra hồi quy",
        "",
    ] {
        let result = classify(prompt);
        assert_eq!(result.mode, "single", "expected single for {prompt:?}: {}", result.reason);
        assert!(result.reason.contains("single:"));
    }
}

#[test]
fn multi_for_three_distinct_imperative_verbs() {
    let result = classify("Implement auth, fix the login bug and update the docs");
    assert_eq!(result.mode, "multi");
    assert!(result.reason.contains("3 distinct imperative verbs"));
}

#[test]
fn multi_for_two_verbs_chained_by_a_sequencing_connector() {
    for prompt in ["Fix the bug then update the tests", "Write the parser, after that update the docs", "thêm handler rồi... sau đó sửa test"] {
        let result = classify(prompt);
        assert_eq!(result.mode, "multi", "expected multi for {prompt:?}: {}", result.reason);
    }
}

#[test]
fn multi_for_two_sequencing_connectors_even_with_one_verb_kind() {
    assert_eq!(classify("run the suite, then run lint, then run build").mode, "multi");
}

#[test]
fn multi_for_multi_step_keyword_plus_a_second_action_signal() {
    for prompt in ["multi-step task: implement the parser and write tests", "plan the workflow then implement it"] {
        let result = classify(prompt);
        assert_eq!(result.mode, "multi", "expected multi for {prompt:?}: {}", result.reason);
    }
}

#[test]
fn multi_for_several_path_mentions_plus_multiple_verbs() {
    assert_eq!(classify("fix and update src/a.ts, src/b.ts, src/c.ts").mode, "multi");
    assert_eq!(classify("update src/a.ts, src/b.ts, src/c.ts").mode, "single");
}

#[test]
fn plain_and_never_makes_a_task_multi_on_its_own() {
    assert_eq!(classify("Fix the bug and update the tests").mode, "single");
}

#[test]
fn caller_provided_file_count_counts_toward_the_path_signal() {
    assert_eq!(classify_workflow_complexity("update the handlers", 3).mode, "single");
    assert_eq!(classify_workflow_complexity("fix and update the handlers", 3).mode, "multi");
}

fn def(steps: Vec<Value>) -> WorkflowDefinition {
    WorkflowDefinition { name: "w".into(), description: "d".into(), scope: "builtin", path: None, inputs: None, steps: steps.into_iter().map(|s| WorkflowStep(s.as_object().unwrap().clone())).collect() }
}

#[derive(Default)]
struct Calls {
    dag: AtomicUsize,
    prompt: AtomicUsize,
    events: Mutex<Vec<Value>>,
}

fn hooks(calls: &Arc<Calls>, prompt_out: impl Fn(&str) -> String + Send + Sync + 'static, dag: bool, multi: bool) -> WorkflowHooks {
    let (cp, cd, ce) = (calls.clone(), calls.clone(), calls.clone());
    let prompt_out = Arc::new(prompt_out);
    WorkflowHooks {
        cwd: ".".into(),
        run_prompt: Some(Arc::new(move |prompt, _, _| {
            cp.prompt.fetch_add(1, Ordering::SeqCst);
            let out = prompt_out(&prompt);
            Box::pin(async move { Ok(out) })
        })),
        run_dag: dag.then(|| {
            let f: Arc<dyn Fn(String, CancellationToken) -> workflow::StepFuture + Send + Sync> = Arc::new(move |goal: String, _| {
                cd.dag.fetch_add(1, Ordering::SeqCst);
                let head: String = goal.chars().take(12).collect();
                Box::pin(async move { Ok(format!("dag:{head}")) })
            });
            f
        }),
        multi_agent_enabled: multi,
        on_event: Some(Arc::new(move |e| ce.events.lock().unwrap().push(e))),
        ..Default::default()
    }
}

#[tokio::test]
async fn dag_step_runs_the_dag_hook_only_when_classifier_says_multi_and_multi_agent_is_enabled() {
    let calls = Arc::new(Calls::default());
    let wf = def(vec![json!({ "id": "plan_and_build", "title": "DAG", "type": "dag", "prompt": "implement the parser, then fix the loader and update the tests" })]);
    let res = execute_workflow(&wf, "task input", &hooks(&calls, |p| format!("prompt:{}", p.chars().take(12).collect::<String>()), true, true)).await.unwrap();
    assert_eq!(res.status, "completed");
    assert_eq!(calls.dag.load(Ordering::SeqCst), 1);
    assert_eq!(calls.prompt.load(Ordering::SeqCst), 0);
    let events = calls.events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "workflow_classified");
    assert_eq!(events[0]["mode"], "multi");
    assert_match(events[0]["reason"].as_str().unwrap(), "multi:");
}

#[tokio::test]
async fn dag_step_degrades_to_a_linear_prompt_when_the_classifier_says_single() {
    let calls = Arc::new(Calls::default());
    let wf = def(vec![json!({ "id": "s", "title": "DAG", "type": "dag", "prompt": "fix the login bug" })]);
    let res = execute_workflow(&wf, "input", &hooks(&calls, |p| format!("prompt:{p}"), true, true)).await.unwrap();
    assert_eq!(res.status, "completed");
    assert_eq!(calls.dag.load(Ordering::SeqCst), 0);
    assert_eq!(calls.prompt.load(Ordering::SeqCst), 1);
    assert_eq!(calls.events.lock().unwrap()[0]["mode"], "single");
    assert_match(&res.final_output, "prompt:fix the login bug");
}

#[tokio::test]
async fn dag_step_stays_linear_when_multi_agent_mode_is_off_or_the_hook_is_missing() {
    let wf = def(vec![json!({ "id": "s", "type": "dag", "prompt": "implement the parser, then fix the loader and update the tests" })]);
    let calls = Arc::new(Calls::default());
    let off = execute_workflow(&wf, "input", &hooks(&calls, |_| "linear".into(), true, false)).await.unwrap();
    assert_eq!(off.final_output, "linear");
    assert_eq!(calls.dag.load(Ordering::SeqCst), 0);
    let no_hook = execute_workflow(&wf, "input", &hooks(&calls, |_| "linear".into(), false, true)).await.unwrap();
    assert_eq!(no_hook.final_output, "linear");
}

#[tokio::test]
async fn old_workflows_without_dag_steps_never_classify_and_stay_linear() {
    let calls = Arc::new(Calls::default());
    let wf = def(vec![
        json!({ "id": "one", "type": "prompt", "prompt": "implement the parser, then fix the loader and update the tests" }),
        json!({ "id": "two", "type": "prompt", "prompt": "done: $prev" }),
    ]);
    let res = execute_workflow(&wf, "input", &hooks(&calls, |p| p.to_string(), false, true)).await.unwrap();
    assert_eq!(res.status, "completed");
    assert!(calls.events.lock().unwrap().is_empty());
    assert_match(&res.final_output, "implement the parser");
}

#[test]
fn markdown_workflows_accept_a_dag_step_and_details_render_it() {
    let wf = parse_markdown_workflow(
        "plan-build.md",
        "---\nname: plan-build\ndescription: plan then build\n---\n## Step 1: Decompose [dag]\nImplement the parser, then fix the loader and update the tests\n",
        "workspace",
        Path::new("/tmp/plan-build.md"),
    );
    assert_eq!(wf.steps.len(), 1);
    assert_eq!(wf.steps[0].kind(), "dag");
    assert_match(wf.steps[0].str("prompt").unwrap(), "Implement the parser");
    assert_match(&format_workflow_details(&wf), "dag goal=\"Implement the parser");
}
