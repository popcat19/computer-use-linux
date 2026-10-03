// Purpose: Execute bounded Rhai desktop workflows without host-code access.

use futures_util::future::BoxFuture;
use rhai::{
    module_resolvers::DummyModuleResolver,
    packages::{BasicArrayPackage, BasicMapPackage, CorePackage, LogicPackage, Package},
    Dynamic, Engine, EvalAltResult, ImmutableString, Map, Module,
};
use rmcp::schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const MAX_SCRIPT_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 16 * 1024 * 1024;
const MAX_CALLS: u32 = 64;

type Dispatcher =
    Arc<dyn Fn(String, Value) -> BoxFuture<'static, Result<Value, String>> + Send + Sync>;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScriptParams {
    pub code: String,
    /// Total runtime in seconds, including desktop calls. Default 30, maximum 120.
    pub timeout_secs: Option<u64>,
    /// Maximum sequential desktop calls. Default 32, maximum 64.
    pub max_calls: Option<u32>,
}

#[derive(Debug, Default)]
pub(crate) struct ScriptOutput {
    pub calls: u32,
    pub outputs: Vec<Value>,
    pub error: Option<String>,
    output_bytes: usize,
    result_bytes: usize,
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn bounded_error(mut message: String) -> String {
    if message.len() > 8192 {
        let mut end = 8192;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str(" [error truncated]");
    }
    message
}

fn script_error(message: impl Into<String>) -> Box<EvalAltResult> {
    bounded_error(message.into()).into()
}

fn check_active(cancelled: &AtomicBool, deadline: Instant) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("script cancelled".into())
    } else if Instant::now() >= deadline {
        Err("script runtime limit exceeded".into())
    } else {
        Ok(())
    }
}

pub(crate) async fn execute_script<F>(params: ScriptParams, dispatch: F) -> ScriptOutput
where
    F: Fn(String, Value) -> BoxFuture<'static, Result<Value, String>> + Send + Sync + 'static,
{
    let timeout_secs = params.timeout_secs.unwrap_or(30);
    let max_calls = params.max_calls.unwrap_or(32);
    let invalid = if params.code.is_empty() || params.code.len() > MAX_SCRIPT_BYTES {
        Some("code must contain 1 to 65536 bytes")
    } else if !(1..=120).contains(&timeout_secs) {
        Some("timeout_secs must be between 1 and 120")
    } else if !(1..=MAX_CALLS).contains(&max_calls) {
        Some("max_calls must be between 1 and 64")
    } else {
        None
    };
    if let Some(error) = invalid {
        return ScriptOutput {
            error: Some(error.into()),
            ..Default::default()
        };
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    // Dropping the MCP future must also stop the blocking interpreter.
    let _cancel = CancelOnDrop(cancelled.clone());
    let runtime = tokio::runtime::Handle::current();
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let dispatch: Dispatcher = Arc::new(dispatch);
    match tokio::task::spawn_blocking(move || {
        evaluate(
            params.code,
            max_calls,
            deadline,
            cancelled,
            runtime,
            dispatch,
        )
    })
    .await
    {
        Ok(output) => output,
        Err(error) => ScriptOutput {
            error: Some(format!("script worker failed: {error}")),
            ..Default::default()
        },
    }
}

fn evaluate(
    code: String,
    max_calls: u32,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    runtime: tokio::runtime::Handle,
    dispatch: Dispatcher,
) -> ScriptOutput {
    let state = Arc::new(Mutex::new(ScriptOutput::default()));
    // Exclude native string padding, blobs, and clocks: native loops cannot be
    // interrupted by interpreter progress checks.
    let mut engine = Engine::new_raw();
    engine.register_global_module(CorePackage::new().as_shared_module());
    engine.register_global_module(LogicPackage::new().as_shared_module());
    engine.register_global_module(BasicArrayPackage::new().as_shared_module());
    engine.register_global_module(BasicMapPackage::new().as_shared_module());
    engine.set_module_resolver(DummyModuleResolver::new());
    for symbol in ["eval", "print", "debug", "Fn", "curry", "call"] {
        engine.disable_symbol(symbol);
    }
    engine
        .set_max_operations(100_000)
        .set_max_expr_depths(64)
        .set_max_variables(128)
        .set_max_string_size(MAX_OUTPUT_BYTES)
        .set_max_array_size(32768)
        .set_max_map_size(131072)
        .set_fail_on_invalid_map_property(true);
    let progress_cancelled = cancelled.clone();
    let progress_state = state.clone();
    engine.on_progress(move |_| {
        check_active(&progress_cancelled, deadline)
            .err()
            .or_else(|| progress_state.lock().unwrap().error.clone())
            .map(Dynamic::from)
    });

    let call_state = state.clone();
    let mut tools = Module::new();
    tools.set_native_fn(
        "invoke",
        move |name: ImmutableString, args: Map| -> Result<Dynamic, Box<EvalAltResult>> {
            let name = name.as_str();
            let result = (|| -> Result<Dynamic, String> {
                check_active(&cancelled, deadline)?;
                {
                    let mut state = call_state.lock().unwrap();
                    if let Some(error) = &state.error {
                        return Err(error.clone());
                    }
                    if state.calls >= max_calls {
                        return Err("script tool-call limit exceeded".into());
                    }
                    if matches!(name, "run_script" | "run_shell" | "complete_interaction") {
                        return Err(format!("tool is not available to scripts: {name}"));
                    }
                    state.calls += 1;
                }
                let args = rhai::serde::from_dynamic::<Value>(&Dynamic::from(args))
                    .map_err(|error| error.to_string())?;
                if serde_json::to_vec(&args)
                    .map_err(|error| error.to_string())?
                    .len()
                    > MAX_SCRIPT_BYTES
                {
                    return Err("tool arguments exceed 65536 bytes".into());
                }
                let result = runtime.block_on(async {
                    let mut poll = tokio::time::interval(Duration::from_millis(10));
                    let call = dispatch(name.to_owned(), args);
                    tokio::pin!(call);
                    loop {
                        tokio::select! {
                            biased;
                            _ = poll.tick() => check_active(&cancelled, deadline)?,
                            result = &mut call => break result,
                        }
                    }
                })?;
                check_active(&cancelled, deadline)?;
                let bytes = serde_json::to_vec(&result)
                    .map_err(|error| error.to_string())?
                    .len();
                {
                    let mut state = call_state.lock().unwrap();
                    state.result_bytes += bytes;
                    if state.result_bytes > MAX_RESULT_BYTES {
                        return Err("cumulative tool results exceed 16 MiB".into());
                    }
                }
                if result.get("ok") == Some(&Value::Bool(false))
                    || result.get("isError") == Some(&Value::Bool(true))
                {
                    return Err(format!(
                        "{name} failed: {}",
                        result.get("message").unwrap_or(&result)
                    ));
                }
                rhai::serde::to_dynamic(result).map_err(|error| error.to_string())
            })();
            result.map_err(|error| {
                let error = bounded_error(error);
                call_state.lock().unwrap().error = Some(error.clone());
                script_error(error)
            })
        },
    );

    engine.register_static_module("tools", tools.into());

    let emit_state = state.clone();
    engine.register_fn(
        "emit",
        move |value: Dynamic| -> Result<(), Box<EvalAltResult>> {
            let mut state = emit_state.lock().unwrap();
            if let Some(error) = &state.error {
                return Err(script_error(error));
            }
            let value: Value = rhai::serde::from_dynamic(&value).map_err(|error| {
                state.error = Some(error.to_string());
                script_error(error.to_string())
            })?;
            let bytes = serde_json::to_vec(&value)
                .map_err(|error| {
                    state.error = Some(error.to_string());
                    script_error(error.to_string())
                })?
                .len();
            if state.output_bytes + bytes > MAX_OUTPUT_BYTES || state.outputs.len() >= 64 {
                let error = "emitted output exceeds 4 MiB or 64 values";
                state.error = Some(error.into());
                return Err(script_error(error));
            }
            state.output_bytes += bytes;
            state.outputs.push(value);
            Ok(())
        },
    );

    let result = engine.run(&code);
    let mut output = std::mem::take(&mut *state.lock().unwrap());
    if output.error.is_none() {
        output.error = result.err().map(|error| bounded_error(error.to_string()));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(code: &str) -> ScriptParams {
        ScriptParams {
            code: code.into(),
            timeout_secs: None,
            max_calls: None,
        }
    }

    async fn run(code: &str) -> ScriptOutput {
        execute_script(params(code), |name, args| {
            Box::pin(async move { Ok(json!({"ok": true, "name": name, "args": args})) })
        })
        .await
    }

    #[tokio::test]
    async fn chains_branches_and_emits_selected_results() {
        let output = run(r#"
            let first = tools::invoke("list_windows", #{});
            if first.ok {
                for n in 0..3 {
                    let next = tools::invoke("click", #{x: n, y: 5});
                    emit(next.args.x);
                }
            }
        "#)
        .await;
        assert_eq!(output.error, None);
        assert_eq!(output.calls, 4);
        assert_eq!(output.outputs, vec![json!(0), json!(1), json!(2)]);
    }

    #[tokio::test]
    async fn no_implicit_output() {
        let output = run(r#"tools::invoke("list_windows", #{});"#).await;
        assert_eq!(output.error, None);
        assert!(output.outputs.is_empty());
    }

    #[tokio::test]
    async fn rejects_host_access_recursion_and_infinite_work() {
        for code in [
            r#"import "/etc/passwd" as host;"#,
            r#"eval("1 + 1");"#,
            r#"print("stdout pollution");"#,
            r#"let s = ""; s.pad(1, "");"#,
            r#"let f = Fn("foo"); for n in 0..40 { f = Fn("foo").curry(f, f); }"#,
            "let f = |x| x;",
            r#"tools::invoke("run_shell", #{command: "id"});"#,
            r#"tools::invoke("run_script", #{code: "1"});"#,
            r#"tools::invoke("complete_interaction", #{});"#,
            "loop {}",
            "fn recurse() { recurse(); } recurse();",
            r#"let s = "x"; loop { s += s; }"#,
        ] {
            assert!(run(code).await.error.is_some(), "accepted: {code}");
        }
    }

    #[tokio::test]
    async fn bounds_calls_and_preserves_partial_output() {
        let mut input =
            params(r#"emit("before"); tools::invoke("a", #{}); tools::invoke("b", #{});"#);
        input.max_calls = Some(1);
        let output = execute_script(input, |_, _| Box::pin(async { Ok(json!({})) })).await;
        assert_eq!(output.calls, 1);
        assert_eq!(output.outputs, vec![json!("before")]);
        assert!(output.error.unwrap().contains("tool-call limit"));
    }

    #[tokio::test]
    async fn failure_cannot_be_caught_to_continue_actions() {
        let output = execute_script(
            params(
                r#"
            try { tools::invoke("fail", #{}); } catch (e) {}
            tools::invoke("must_not_run", #{});
        "#,
            ),
            |_, _| Box::pin(async { Ok(json!({"ok": false, "message": "failed"})) }),
        )
        .await;
        assert_eq!(output.calls, 1);
        assert!(output.error.unwrap().contains("failed"));
    }

    #[tokio::test]
    async fn emit_conversion_failure_cannot_be_caught_to_continue_actions() {
        let output = run(r#"
            try { emit(0..2); } catch (error) {}
            tools::invoke("must_not_run", #{});
        "#)
        .await;
        assert!(output.error.is_some());
        assert_eq!(output.calls, 0);
    }

    #[tokio::test]
    async fn times_out_pending_desktop_calls() {
        let mut input = params(r#"tools::invoke("pending", #{});"#);
        input.timeout_secs = Some(1);
        let start = Instant::now();
        let output = execute_script(input, |_, _| Box::pin(std::future::pending())).await;
        assert!(output.error.unwrap().contains("runtime limit"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn dropping_request_stops_pending_dispatch_and_later_calls() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel();
        let notifications = Arc::new(Mutex::new(Some((started_tx, stopped_tx))));
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = calls.clone();
        let request = tokio::spawn(execute_script(
            params(r#"tools::invoke("pending", #{}); tools::invoke("later", #{});"#),
            move |_, _| {
                counter.fetch_add(1, Ordering::Relaxed);
                let (started, stopped) = notifications.lock().unwrap().take().unwrap();
                Box::pin(async move {
                    struct Stopped(Option<tokio::sync::oneshot::Sender<()>>);
                    impl Drop for Stopped {
                        fn drop(&mut self) {
                            let _ = self.0.take().unwrap().send(());
                        }
                    }
                    let _stopped = Stopped(Some(stopped));
                    let _ = started.send(());
                    std::future::pending().await
                })
            },
        ));
        started_rx.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(1), stopped_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn supported_accessibility_tree_fits_recursive_data_limits() {
        let nodes: Vec<Value> = (0..2000)
            .map(|index| {
                json!({
                    "index": index, "role": "entry", "name": "field", "text": "value",
                    "states": ["editable", "visible"], "children": [], "actions": [],
                    "bounds": {"x": 0, "y": 0, "width": 10, "height": 10},
                    "identifier": "node", "parent": 0, "app": "test", "value": null,
                    "description": "", "editable": true,
                })
            })
            .collect();
        let output = execute_script(
            params(
                r#"
            let state = tools::invoke("get_app_state", #{});
            emit(state.accessibility_tree.len());
        "#,
            ),
            move |_, _| {
                let nodes = nodes.clone();
                Box::pin(async move { Ok(json!({"accessibility_tree": nodes})) })
            },
        )
        .await;
        assert_eq!(output.error, None);
        assert_eq!(output.outputs, vec![json!(2000)]);
    }

    #[tokio::test]
    async fn bounds_arguments_results_and_emissions() {
        let output = run("for n in 0..65 { emit(n); }").await;
        assert_eq!(output.outputs.len(), 64);
        assert!(output.error.unwrap().contains("emitted output"));

        let output = execute_script(
            params(
                r#"
            let first = tools::invoke("large", #{});
            tools::invoke("later", #{text: first});
        "#,
            ),
            |_, _| Box::pin(async { Ok(json!("x".repeat(MAX_SCRIPT_BYTES))) }),
        )
        .await;
        assert!(output.error.unwrap().contains("arguments exceed"));

        let output = execute_script(
            params(
                r#"
            for n in 0..20 { tools::invoke("large", #{}); }
        "#,
            ),
            |_, _| Box::pin(async { Ok(json!("x".repeat(1024 * 1024))) }),
        )
        .await;
        assert!(output.error.unwrap().contains("cumulative tool results"));

        let output = execute_script(
            params(
                r#"
            for n in 0..8 { emit(tools::invoke("large", #{})); }
        "#,
            ),
            |_, _| Box::pin(async { Ok(json!("x".repeat(1024 * 1024))) }),
        )
        .await;
        assert!(output.error.unwrap().contains("emitted output"));
    }

    #[test]
    fn bounds_error_text_without_splitting_unicode() {
        let error = bounded_error("界".repeat(10000));
        assert!(error.len() < 8300);
        assert!(error.ends_with("[error truncated]"));
    }

    #[tokio::test]
    async fn validates_limits_before_dispatch() {
        for input in [
            params(""),
            params(&" ".repeat(MAX_SCRIPT_BYTES + 1)),
            ScriptParams {
                timeout_secs: Some(0),
                ..params("1")
            },
            ScriptParams {
                timeout_secs: Some(121),
                ..params("1")
            },
            ScriptParams {
                max_calls: Some(0),
                ..params("1")
            },
            ScriptParams {
                max_calls: Some(65),
                ..params("1")
            },
        ] {
            let output = execute_script(input, |_, _| panic!("invalid script dispatched")).await;
            assert!(output.error.is_some());
            assert_eq!(output.calls, 0);
        }
    }
}
