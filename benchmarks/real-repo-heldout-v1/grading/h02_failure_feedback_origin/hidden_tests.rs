// Private held-out behavioral tests (h02). Appended by the grader to
// tests/http_contract.rs in a disposable grading copy; reuses that suite's
// mock model servers, Raindrop bridge capture and session fixtures.
#[cfg(test)]
mod engram_heldout_failure_origin {
    use super::*;

    type Seen = Arc<Mutex<Vec<(String, serde_json::Value)>>>;

    async fn wait_until(seen: &Seen, done: impl Fn(&[(String, serde_json::Value)]) -> bool) {
        for _ in 0..500 {
            if done(&seen.lock().unwrap()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn events<'a>(seen: &'a [(String, serde_json::Value)], name: &str, session_id: &str) -> Vec<&'a serde_json::Value> {
        seen.iter()
            .filter(|(path, _)| path == "events")
            .map(|(_, payload)| payload)
            .filter(|event| event["event"] == name && event["convo_id"] == session_id)
            .collect()
    }

    fn signals<'a>(seen: &'a [(String, serde_json::Value)], name: &str, event_id: &serde_json::Value) -> Vec<&'a serde_json::Value> {
        seen.iter()
            .filter(|(path, _)| path == "signals")
            .map(|(_, payload)| payload)
            .filter(|signal| signal["name"] == name && signal["event_id"] == *event_id)
            .collect()
    }

    /// State whose model always fails, so every step takes the failure path.
    async fn failing_model_state(seen: &Seen) -> AppState {
        let raindrop_url = spawn_raindrop_bridge_server(seen.clone()).await;
        let model_seen = Arc::new(Mutex::new(Vec::new()));
        let base_url = spawn_openai_error_server(StatusCode::INTERNAL_SERVER_ERROR, model_seen).await;
        let settings = settings_for_test(|settings| {
            settings.gemini_api_key = SecretString::new("test-key".to_string());
            settings.gemini_base_url = base_url;
            settings.raindrop_bridge_url = Some(raindrop_url);
        });
        AppState::build(settings).await.unwrap()
    }

    async fn post_failing_step(app: &axum::Router, session_id: &str, origin: Option<&str>) {
        let mut body = json!({
            "session_id": session_id,
            "user_id": "device-user-9",
            "prompt": "Open settings",
            "phone_state": "Current app: Settings (com.android.settings)",
            "installed_packages": "Settings:com.android.settings",
            "model_key": "agi-1-apex"
        });
        if let Some(origin) = origin {
            body["origin"] = json!(origin);
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/get_action")
                    .header("authorization", legacy_bearer("public@example.com"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            !response.status().is_success(),
            "the model failure must surface, got {}",
            response.status()
        );
    }

    /// The failed step's analytics event for a session, once delivered.
    async fn failure_event(seen: &Seen, session_id: &str) -> serde_json::Value {
        wait_until(seen, |seen| !events(seen, "step", session_id).is_empty()).await;
        let seen = seen.lock().unwrap();
        let found = events(&seen, "step", session_id);
        assert_eq!(found.len(), 1, "one failure event for {session_id}");
        let event = found[0].clone();
        assert_eq!(event["properties"]["model_call_complete"], json!(false), "{event}");
        assert_eq!(event["properties"]["action_outcome"], "failed", "{event}");
        event
    }

    async fn post_reaction(app: &axum::Router, session_id: &str, reaction: i64, cancelled: bool) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/reaction")
                    .header("authorization", legacy_bearer("public@example.com"))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "session_id": session_id,
                            "reaction": reaction,
                            "cancelled": cancelled,
                            "user_feedback": "benchmark feedback"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "reaction for {session_id}");
    }

    async fn reaction_state(seen: &Seen, sessions: &[(&str, Option<&str>)]) -> axum::Router {
        let raindrop_url = spawn_raindrop_bridge_server(seen.clone()).await;
        let settings = settings_for_test(|settings| {
            settings.raindrop_bridge_url = Some(raindrop_url);
        });
        let state = AppState::build(settings).await.unwrap();
        for (session_id, origin) in sessions {
            let mut session = owned_session(session_id, "public@example.com");
            session.meta.user_id = Some("device-user-9".to_string());
            session.meta.step_count = 3;
            session.meta.origin = origin.map(str::to_string);
            state.sessions.insert_session(session).await.unwrap();
        }
        routes::router(state)
    }

    /// The user_feedback event plus its user_feedback signal and the companion
    /// verified-outcome signal riding the same event id.
    async fn feedback_records(seen: &Seen, session_id: &str, outcome: &str) -> Vec<serde_json::Value> {
        wait_until(seen, |seen| {
            events(seen, "user_feedback", session_id).first().is_some_and(|event| {
                !signals(seen, "user_feedback", &event["event_id"]).is_empty()
                    && !signals(seen, outcome, &event["event_id"]).is_empty()
            })
        })
        .await;
        let seen = seen.lock().unwrap();
        let feedback = events(&seen, "user_feedback", session_id);
        assert_eq!(feedback.len(), 1, "one user_feedback event for {session_id}");
        let event = feedback[0].clone();
        let feedback_signal = signals(&seen, "user_feedback", &event["event_id"]);
        let outcome_signal = signals(&seen, outcome, &event["event_id"]);
        assert_eq!(feedback_signal.len(), 1, "user_feedback signal for {session_id}");
        assert_eq!(outcome_signal.len(), 1, "{outcome} signal for {session_id}");
        vec![event, feedback_signal[0].clone(), outcome_signal[0].clone()]
    }

    #[tokio::test]
    async fn first_step_failure_carries_the_declared_origin() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = routes::router(failing_model_state(&seen).await);
        post_failing_step(&app, "hx-fail-first", Some("mcp")).await;
        let event = failure_event(&seen, "hx-fail-first").await;
        assert_eq!(event["properties"]["origin"], "mcp", "{event}");
    }

    #[tokio::test]
    async fn first_step_failure_normalizes_origin_like_session_creation() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = routes::router(failing_model_state(&seen).await);
        let cases = [
            ("hx-fail-norm", " MCP ", Some("mcp")),
            ("hx-fail-open", "watch", Some("watch")),
            ("hx-fail-blank", "  ", None),
            ("hx-fail-pii", "user@example.com", None),
        ];
        for (session_id, origin, _) in &cases {
            post_failing_step(&app, session_id, Some(origin)).await;
        }
        for (session_id, _, expected) in &cases {
            let event = failure_event(&seen, session_id).await;
            match expected {
                Some(origin) => assert_eq!(event["properties"]["origin"], *origin, "{session_id}: {event}"),
                None => assert!(
                    event["properties"].get("origin").is_none(),
                    "{session_id}: an origin that session creation would drop stays absent: {event}"
                ),
            }
        }
    }

    #[tokio::test]
    async fn loaded_session_failure_keeps_the_frozen_origin() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let state = failing_model_state(&seen).await;
        let mut stamped = owned_session("hx-fail-frozen", "public@example.com");
        stamped.meta.origin = Some("app".to_string());
        state.sessions.insert_session(stamped).await.unwrap();
        let app = routes::router(state);
        post_failing_step(&app, "hx-fail-frozen", Some("mcp")).await;
        let event = failure_event(&seen, "hx-fail-frozen").await;
        assert_eq!(
            event["properties"]["origin"], "app",
            "the session's creation-time stamp wins over this step's declaration: {event}"
        );
    }

    #[tokio::test]
    async fn unstamped_session_failure_stays_untagged() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let state = failing_model_state(&seen).await;
        state
            .sessions
            .insert_session(owned_session("hx-fail-null", "public@example.com"))
            .await
            .unwrap();
        let app = routes::router(state);
        post_failing_step(&app, "hx-fail-null", Some("mcp")).await;
        let event = failure_event(&seen, "hx-fail-null").await;
        assert!(
            event["properties"].get("origin").is_none(),
            "a later declaration never tags a session created without origin: {event}"
        );
    }

    #[tokio::test]
    async fn reaction_feedback_event_carries_the_session_origin() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = reaction_state(&seen, &[("hx-react-fail", Some("mcp"))]).await;
        post_reaction(&app, "hx-react-fail", 0, true).await;
        let records = feedback_records(&seen, "hx-react-fail", "task_failed").await;
        assert_eq!(records[0]["properties"]["origin"], "mcp", "user_feedback event: {}", records[0]);
    }

    #[tokio::test]
    async fn reaction_outcome_signals_carry_the_session_origin() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = reaction_state(&seen, &[("hx-react-down", Some("mcp")), ("hx-react-up", Some("app"))]).await;
        post_reaction(&app, "hx-react-down", 0, true).await;
        post_reaction(&app, "hx-react-up", 1, false).await;
        let failed = feedback_records(&seen, "hx-react-down", "task_failed").await;
        assert_eq!(failed[1]["properties"]["origin"], "mcp", "user_feedback signal: {}", failed[1]);
        assert_eq!(failed[2]["properties"]["origin"], "mcp", "task_failed signal: {}", failed[2]);
        let completed = feedback_records(&seen, "hx-react-up", "task_completed").await;
        assert_eq!(completed[0]["properties"]["origin"], "app", "user_feedback event: {}", completed[0]);
        assert_eq!(completed[2]["properties"]["origin"], "app", "task_completed signal: {}", completed[2]);
    }

    #[tokio::test]
    async fn reaction_records_stay_untagged_without_origin() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = reaction_state(&seen, &[("hx-react-null", None)]).await;
        post_reaction(&app, "hx-react-null", 0, true).await;
        let records = feedback_records(&seen, "hx-react-null", "task_failed").await;
        for record in &records {
            assert!(record["properties"].get("origin").is_none(), "absent, never empty: {record}");
        }
    }

    #[tokio::test]
    async fn successful_steps_keep_their_origin_stamp() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let raindrop_url = spawn_raindrop_bridge_server(seen.clone()).await;
        let base_url = spawn_openai_action_server("finished({\"content\":\"Done\"})").await;
        let settings = settings_for_test(|settings| {
            settings.gemini_api_key = SecretString::new("test-key".to_string());
            settings.gemini_base_url = base_url;
            settings.raindrop_bridge_url = Some(raindrop_url);
        });
        let app = routes::router(AppState::build(settings).await.unwrap());
        let body = json!({
            "session_id": "hx-ok",
            "prompt": "Open settings",
            "phone_state": "Current app: Settings (com.android.settings)",
            "installed_packages": "Settings:com.android.settings",
            "model_key": "agi-1-apex",
            "origin": "mcp"
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/get_action")
                    .header("authorization", legacy_bearer("public@example.com"))
                    .header("host", "dev.example.test")
                    .header("x-forwarded-proto", "https")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        wait_until(&seen, |seen| {
            !events(seen, "step", "hx-ok").is_empty() && !events(seen, "session_end", "hx-ok").is_empty()
        })
        .await;
        let seen = seen.lock().unwrap();
        for name in ["step", "session_end"] {
            let found = events(&seen, name, "hx-ok");
            assert_eq!(found.len(), 1, "{name} event");
            assert_eq!(found[0]["properties"]["origin"], "mcp", "{name}: {}", found[0]);
        }
    }
}
