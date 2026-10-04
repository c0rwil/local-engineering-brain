
#[cfg(test)]
mod engram_benchmark_environment {
    use super::*;

    #[test]
    fn test_mode_does_not_restore_scrubbed_dotenv_values() {
        std::env::set_var("ENVIRONMENT", "test");
        std::env::remove_var("USE_APP_GUIDES");
        std::env::remove_var("AGENT_API_URL");
        let settings = Settings::from_env();
        assert!(settings.use_app_guides, "test mode must keep the server default after a scrub");
        assert_ne!(settings.agent_api_url.as_deref(), Some("http://127.0.0.1:9/benchmark-dotenv"));
    }

    #[test]
    fn development_still_loads_dotenv() {
        std::env::set_var("ENVIRONMENT", "development");
        std::env::remove_var("USE_APP_GUIDES");
        std::env::remove_var("AGENT_API_URL");
        let settings = Settings::from_env();
        assert!(!settings.use_app_guides, "development must still load its .env");
        assert_eq!(settings.agent_api_url.as_deref(), Some("http://127.0.0.1:9/benchmark-dotenv"));
    }

    fn poison_with_original_failure() {
        let result = std::panic::catch_unwind(|| {
            let _guard = TEST_ENV_LOCK.lock().unwrap();
            panic!("benchmark original assertion remains visible");
        });
        let failure = result.expect_err("the original assertion must still fail");
        assert_eq!(failure.downcast_ref::<&str>(), Some(&"benchmark original assertion remains visible"));
    }

    #[test]
    fn poisoned_lock_does_not_break_settings_harness() {
        poison_with_original_failure();
        let settings = settings_for_test(|_| {});
        assert_eq!(settings.environment, "test");
        assert!(!settings.use_redis_sessions);
    }

    #[test]
    fn poisoned_lock_does_not_break_cluster_contract() {
        poison_with_original_failure();
        redis_cluster_mode_does_not_default_to_local_redis_url();
    }

    #[test]
    fn poisoned_lock_does_not_break_upstash_contract() {
        poison_with_original_failure();
        upstash_mode_does_not_default_to_local_redis_url();
    }
}
