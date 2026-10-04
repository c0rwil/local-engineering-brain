
#[cfg(test)]
mod engram_benchmark_guide_family {
    use super::*;
    use crate::config::AgentDataSource;
    use crate::models::api::SessionActionRequest;
    use crate::services::partner_runtime::apply_guide_policy_to_request;
    use axum::{extract::State, routing::{get, post}, Json, Router};
    use secrecy::SecretString;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PKG: &str = "com.google.android.apps.maps";
    const TASK: &str = "Reserve a table through Maps.";
    const APP: &str = "Maps application navigation.";

    async fn search(State(hits): State<Arc<AtomicUsize>>) -> Json<Value> {
        hits.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!([{"id":"bench-guide", "guide_name":"Restaurant booking",
            "guide":TASK, "similarity":0.99}]))
    }

    fn request(switches: Value, session: bool) -> ActionRequest {
        let mut value = serde_json::json!({
            "session_id":"bench-session", "prompt":"book a table at the italian place downtown", "installed_packages":"",
            "phone_state":{"current_app":"Maps", "package_name":PKG},
            "device_metadata":{"screen_width":1080,"screen_height":2400},
            "use_guide_cache":false
        });
        value.as_object_mut().unwrap().extend(switches.as_object().unwrap().clone());
        if session {
            serde_json::from_value::<SessionActionRequest>(value).unwrap()
                .into_action_request("bench-session".into())
        } else {
            serde_json::from_value(value).unwrap()
        }
    }

    async fn resolve(mut request: ActionRequest, policy: Option<(bool, bool)>) -> (ResolvedGuides, usize) {
        let hits = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route("/v1/guides/search", get(search)).with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut settings = Settings::from_env();
        settings.agent_data_source = AgentDataSource::Api;
        settings.agent_api_url = Some(format!("http://{address}"));
        settings.agent_api_key = SecretString::new("benchmark-fake-key".into());
        settings.use_app_guides = true;
        let settings = Arc::new(settings);
        let store = Arc::new(AppGuideStore::new(settings.clone(), reqwest::Client::new(), None));
        store.seed_for_tests(PKG, APP);
        let client = GuideClient::with_session_store(settings, reqwest::Client::new(), SessionStore::memory()).with_app_guide_store(store);
        let mut context = PartnerRuntimeContext::default_for_model("agi-1-apex".into());
        if let Some((guides, task)) = policy {
            context.is_default = false;
            context.profile_status = Some("active".into());
            context.enforcement_flags.insert("guides".into(), true);
            context.guide_policy.guides_enabled = guides;
            context.guide_policy.task_guides_enabled = task;
            apply_guide_policy_to_request(&mut request, &context);
            // Resolve the effective request separately from the policy object. This
            // verifies that request conversion/merging carries its restrictions.
            context = PartnerRuntimeContext::default_for_model("agi-1-apex".into());
        }
        let resolved = client.resolve_for_action(&AgentDb::Disabled, &request, "agi-1-apex", None,
            &context, Some("benchmark@example.invalid"), false).await;
        server.abort();
        (resolved, hits.load(Ordering::SeqCst))
    }

    async fn check(switches: Value, session: bool, policy: Option<(bool, bool)>, task: bool, app: bool) {
        let (resolved, hits) = resolve(request(switches, session), policy).await;
        assert_eq!(hits, usize::from(task), "task switch must prevent actual search I/O");
        assert_eq!(resolved.task_guide.as_deref(), task.then_some(TASK));
        assert_eq!(resolved.task_guide_searched, task);
        assert!(!resolved.task_guide_deferred, "disabled work must not be deferred");
        assert_eq!(resolved.app_guide.as_deref(), app.then_some(APP));
        assert_eq!(resolved.app_guide_package.as_deref(), app.then_some(PKG));
    }

    #[tokio::test]
    async fn absent_preserves_both() { check(serde_json::json!({}), false, None, true, true).await; }
    #[tokio::test]
    async fn task_opt_out_skips_search() { check(serde_json::json!({"use_task_guides":false}), false, None, false, true).await; }
    #[tokio::test]
    async fn app_opt_out_preserves_task() { check(serde_json::json!({"use_app_guides":false}), false, None, true, false).await; }
    #[tokio::test]
    async fn master_overrides_family_opt_in() { check(serde_json::json!({"use_guide":false,"use_task_guides":true,"use_app_guides":true}), false, None, false, false).await; }
    #[tokio::test]
    async fn both_families_off() { check(serde_json::json!({"use_task_guides":false,"use_app_guides":false}), false, None, false, false).await; }
    #[tokio::test]
    async fn session_conversion_preserves_task_opt_out() { check(serde_json::json!({"use_task_guides":false}), true, None, false, true).await; }
    #[tokio::test]
    async fn session_conversion_preserves_app_opt_out() { check(serde_json::json!({"use_app_guides":false}), true, None, true, false).await; }
    #[tokio::test]
    async fn policy_restricts_task_opt_in() { check(serde_json::json!({"use_task_guides":true}), false, Some((true,false)), false, true).await; }
    #[tokio::test]
    async fn policy_cannot_override_caller_opt_out() { check(serde_json::json!({"use_task_guides":false}), false, Some((true,true)), false, true).await; }
    #[tokio::test]
    async fn policy_master_disables_both() { check(serde_json::json!({"use_task_guides":true,"use_app_guides":true}), false, Some((false,true)), false, false).await; }
    #[tokio::test]
    async fn disabled_task_does_not_schedule_app_open_warm() {
        let switches = serde_json::json!({"use_task_guides":false,"use_guide_cache":true,
            "phone_state":{"current_app":"Launcher","package_name":"com.android.launcher"}});
        let (resolved, hits) = resolve(request(switches, false), None).await;
        assert_eq!(hits, 0);
        assert!(resolved.task_guide.is_none());
        assert!(!resolved.task_guide_deferred, "an opt-out cannot queue deferred task work");
        assert!(!resolved.task_guide_searched);
    }
    #[tokio::test]
    async fn enabled_app_open_still_schedules_warm() {
        let switches = serde_json::json!({"use_guide_cache":true,
            "phone_state":{"current_app":"Launcher","package_name":"com.android.launcher"}});
        let (resolved, hits) = resolve(request(switches, false), None).await;
        assert_eq!(hits, 0);
        assert!(resolved.task_guide_deferred, "control must exercise the deferred path");
    }

    async fn cache_pipeline(State(reads): State<Arc<AtomicUsize>>, Json(commands): Json<Vec<Vec<Value>>>) -> Json<Value> {
        Json(Value::Array(commands.iter().map(|command| {
            match command.first().and_then(Value::as_str).unwrap_or("") {
                "PING" => serde_json::json!({"result":"PONG"}),
                "GET" => { reads.fetch_add(1, Ordering::SeqCst); serde_json::json!({"result":TASK}) },
                "EXPIRE" => serde_json::json!({"result":1}),
                other => panic!("unexpected mock cache command {other}"),
            }
        }).collect()))
    }

    async fn cache_followup(enabled: bool) {
        let reads = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route("/pipeline", post(cache_pipeline)).with_state(reads.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut settings = Settings::from_env();
        settings.agent_data_source = AgentDataSource::Api;
        settings.agent_api_url = Some("http://127.0.0.1:9/unexpected-search".into());
        settings.use_redis_sessions = true;
        settings.require_redis_when_enabled = true;
        settings.redis_url = None;
        settings.redis_cluster_enabled = false;
        settings.redis_pool_enabled = false;
        settings.upstash_redis_rest_url = Some(format!("http://{address}"));
        settings.upstash_redis_rest_token = SecretString::new("benchmark-fake-cache-token".into());
        settings.redis_reconnect_interval = std::time::Duration::from_secs(3600);
        settings.guide_cache_ttl_seconds = 0;
        let settings = Arc::new(settings);
        let store = SessionStore::connect(settings.clone()).await.unwrap();
        let client = GuideClient::with_session_store(settings, reqwest::Client::new(), store);
        let request = request(serde_json::json!({"prompt":"", "use_task_guides":enabled,
            "use_guide_cache":true,"use_app_guides":false}), false);
        let mut session = SessionData::default();
        session.steps.push(crate::models::session::StepText {
            prompt: "book a table at the italian place downtown".into(),
            action: "click({\"x\":1,\"y\":1})".into(),
            ..Default::default()
        });
        reads.store(0, Ordering::SeqCst);
        let resolved = client.resolve_for_action(&AgentDb::Disabled, &request, "agi-1-apex", Some(&session),
            &PartnerRuntimeContext::default_for_model("agi-1-apex".into()), Some("benchmark@example.invalid"), false).await;
        assert_eq!(resolved.task_guide.as_deref(), enabled.then_some(TASK));
        assert!(!resolved.task_guide_deferred);
        assert!(!resolved.task_guide_searched);
        assert_eq!(reads.load(Ordering::SeqCst) > 0, enabled, "disabled task guides must not perform later cache pickup");
        server.abort();
    }
    #[tokio::test]
    async fn disabled_task_skips_later_cache_pickup() { cache_followup(false).await; }
    #[tokio::test]
    async fn enabled_task_preserves_later_cache_pickup() { cache_followup(true).await; }
}
