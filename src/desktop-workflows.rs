// Purpose: Run a desktop action and return fresh scoped observation feedback.

use super::*;
use std::future::Future;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum DesktopAction {
    ActivateWindow,
    Click,
    Drag,
    Scroll,
    PressKey,
    TypeText,
    PerformAction,
    SetValue,
    MoveWindow,
    ResizeWindow,
}

impl DesktopAction {
    fn name(self) -> String {
        serde_json::to_value(self)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkflowParams {
    pub action: DesktopAction,
    pub arguments: BTreeMap<String, serde_json::Value>,
    /// Optional get_app_state parameters. Scope defaults to the action target,
    /// then the window focused after the action. Never falls back to a desktop tree.
    pub state: Option<GetAppStateParams>,
    /// Delay before observation, in milliseconds. Default 200, maximum 2000.
    #[schemars(range(min = 0, max = 2000))]
    pub settle_ms: Option<u64>,
    /// Total workflow timeout in seconds. Default 30, maximum 120.
    #[schemars(range(min = 1, max = 120))]
    pub timeout_secs: Option<u64>,
}

impl WorkflowParams {
    fn validate(&self) -> std::result::Result<(), String> {
        if self.settle_ms.unwrap_or(200) > 2000 {
            return Err("settle_ms must be between 0 and 2000".into());
        }
        if !(1..=120).contains(&self.timeout_secs.unwrap_or(30)) {
            return Err("timeout_secs must be between 1 and 120".into());
        }
        Ok(())
    }

    fn observation(&self) -> std::result::Result<GetAppStateParams, String> {
        let state = self.state.clone().unwrap_or_default();
        if has_scope(&state) {
            return Ok(state);
        }
        let mut value = serde_json::to_value(state).map_err(|error| error.to_string())?;
        for key in [
            "window_id",
            "pid",
            "app_id",
            "wm_class",
            "title",
            "tty",
            "terminal_pid",
            "terminal_command",
            "terminal_cwd",
        ] {
            if let Some(target) = self.arguments.get(key).filter(|value| !value.is_null()) {
                value[key] = target.clone();
            }
        }
        if value.get("title").is_none_or(serde_json::Value::is_null) {
            if let Some(title) = self.arguments.get("window_title") {
                value["title"] = title.clone();
            }
        }
        serde_json::from_value(value)
            .map_err(|error| format!("invalid observation target: {error}"))
    }
}

fn has_scope(state: &GetAppStateParams) -> bool {
    state.window_target().has_target()
        || state
            .app_name_or_bundle_identifier
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
}

fn failure(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(
        serde_json::json!({"ok":false,"message":message.into()}).to_string(),
    )])
}

impl ComputerUseLinux {
    pub(super) async fn perform_and_observe(&self, params: WorkflowParams) -> CallToolResult {
        if let Err(error) = params.validate() {
            return failure(error);
        }
        let limit = Duration::from_secs(params.timeout_secs.unwrap_or(30));
        match timeout(limit, workflow_steps(params,
            |name, args| async move { self.dispatch_script_tool(&name, args).await },
            |mut state| async move {
                if !has_scope(&state) {
                    state.window_id = self.focused_window().await.0.focused_window.map(|window| window.window_id);
                }
                if !has_scope(&state) {
                    return failure("No scoped observation target is available; supply state.window_id, state.pid, or an app selector. The action has already completed.");
                }
                self.get_app_state(Parameters(state)).await
            },
        )).await {
            Ok(result) => result,
            Err(_) => failure("workflow runtime limit exceeded; observe again before retrying because dispatched input can still finish"),
        }
    }
}

async fn workflow_steps<A, AF, O, OF>(
    params: WorkflowParams,
    action: A,
    observe: O,
) -> CallToolResult
where
    A: FnOnce(String, serde_json::Value) -> AF,
    AF: Future<Output = std::result::Result<serde_json::Value, String>>,
    O: FnOnce(GetAppStateParams) -> OF,
    OF: Future<Output = CallToolResult>,
{
    if let Err(error) = params.validate() {
        return failure(error);
    }
    let state = match params.observation() {
        Ok(state) => state,
        Err(error) => return failure(error),
    };
    let name = params.action.name();
    let args = serde_json::Value::Object(params.arguments.into_iter().collect());
    let action = match action(name, args).await {
        Ok(action) => action,
        Err(error) => return failure(error),
    };
    sleep(Duration::from_millis(params.settle_ms.unwrap_or(200))).await;
    let observed = observe(state).await;
    feedback_result(action, observed)
}

fn feedback_result(action: serde_json::Value, observed: CallToolResult) -> CallToolResult {
    let action_completed = action.get("ok") == Some(&serde_json::Value::Bool(true));
    let state = observed.structured_content.clone().or_else(|| {
        observed
            .content
            .iter()
            .filter_map(|block| block.as_text())
            .find_map(|block| serde_json::from_str(&block.text).ok())
    });
    let state_observed = observed.is_error != Some(true)
        && state.as_ref().is_some_and(|state| {
            state
                .get("observation_available")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or_else(|| {
                    state
                        .get("accessibility_error")
                        .is_some_and(serde_json::Value::is_null)
                        || state
                            .get("screenshot")
                            .is_some_and(|image| !image.is_null())
                })
        });
    let metadata = serde_json::json!({
        "ok": action_completed && state_observed,
        "action": action,
        "state": state,
        "feedback": {"action_completed":action_completed,"state_observed":state_observed,"verification_required":true},
        "message": "Inspect the fresh state to verify the intended effect; successful input dispatch is not effect verification.",
    });
    let mut content = vec![Content::text(metadata.to_string())];
    content.extend(
        observed
            .content
            .into_iter()
            .filter(|block| block.as_image().is_some()),
    );
    let mut result = if action_completed && state_observed {
        CallToolResult::success(content)
    } else {
        CallToolResult::error(content)
    };
    result.structured_content = Some(metadata);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn params() -> WorkflowParams {
        serde_json::from_value(json!({"action":"scroll","arguments":{"pid":42},"settle_ms":0}))
            .unwrap()
    }

    #[tokio::test]
    async fn validates_before_actions_and_infers_scope() {
        let mut input = params();
        input.settle_ms = Some(2001);
        let result = workflow_steps(
            input,
            |_, _| async { panic!("action dispatched") },
            |_| async { panic!("observed") },
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(params().observation().unwrap().pid, Some(42));
        let titled: WorkflowParams =
            serde_json::from_value(json!({"action":"click","arguments":{"window_title":"target"}}))
                .unwrap();
        assert_eq!(
            titled.observation().unwrap().title.as_deref(),
            Some("target")
        );
        let input: WorkflowParams = serde_json::from_value(
            json!({"action":"scroll","arguments":{"pid":42},"state":{"app_id":"chosen"}}),
        )
        .unwrap();
        assert_eq!(
            input.observation().unwrap().app_id.as_deref(),
            Some("chosen")
        );
        assert_eq!(input.observation().unwrap().pid, None);
        assert!(serde_json::from_value::<WorkflowParams>(
            json!({"action":"run_shell","arguments":{}})
        )
        .is_err());
    }

    #[tokio::test]
    async fn failed_input_still_returns_fresh_feedback_and_images() {
        let order = &AtomicUsize::new(0);
        let result = workflow_steps(params(), |name, _| async move {
            assert_eq!(name, "scroll");
            assert_eq!(order.fetch_add(1, Ordering::Relaxed), 0);
            Ok(json!({"ok":false,"message":"refused"}))
        }, |state| async move {
            assert_eq!(state.pid, Some(42));
            assert_eq!(order.fetch_add(1, Ordering::Relaxed), 1);
            crate::tool_output::state_result(json!({
                "accessibility_error":null,"screenshot":{"data_url":"data:image/png;base64,aGVsbG8="},
            })).unwrap()
        }).await;
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[1].as_image().is_some());
        let metadata = result.structured_content.unwrap();
        assert_eq!(metadata["feedback"]["action_completed"], false);
        assert_eq!(metadata["feedback"]["state_observed"], true);
        assert!(!metadata.to_string().contains("base64"));
    }

    #[tokio::test]
    async fn cancellation_during_settle_prevents_observation() {
        let mut input = params();
        input.settle_ms = Some(2000);
        let observed = Arc::new(AtomicUsize::new(0));
        let count = observed.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let request = tokio::spawn(workflow_steps(
            input,
            move |_, _| async move {
                let _ = started_tx.send(());
                Ok(json!({"ok":true}))
            },
            move |_| async move {
                count.fetch_add(1, Ordering::Relaxed);
                failure("unexpected observation")
            },
        ));
        started_rx.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(observed.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn input_success_is_not_effect_verification_or_observation_success() {
        let unavailable = feedback_result(
            json!({"ok":true}),
            crate::tool_output::state_result(json!({
                "observation_available":false,"accessibility_error":null,"screenshot":null,
            }))
            .unwrap(),
        );
        assert_eq!(unavailable.is_error, Some(true));
        let result = feedback_result(json!({"ok":true}), failure("no target"));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["feedback"]["verification_required"],
            true
        );
    }
}
