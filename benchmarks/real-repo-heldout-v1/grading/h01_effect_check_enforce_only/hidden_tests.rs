// Private held-out behavioral tests (h01). Appended by the grader to
// tests/http_contract.rs in a disposable grading copy; reuses that suite's
// mock model, GCS trace sink and session fixtures.
#[cfg(test)]
mod engram_heldout_effect_check {
    use super::*;

    const DEAD_TAP: &str = r#"click({"point_2d":[100,100]})"#;
    const NOTE_MARKER: &str = "[effect-check]";
    const NOTE_TEXT: &str = "left the screen unchanged";

    /// One step after a recorded tap, with the next screenshot either identical
    /// (the tap provably did nothing) or different. Returns every payload the
    /// mock model received and the state, so the persisted step can be read.
    async fn step_after_tap(mode: &str, session_id: &str, next_screen: &str) -> (String, AppState) {
        let owner = format!("{session_id}@theagi.company");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base_url =
            spawn_openai_action_server_with_seen(Arc::new(DEAD_TAP.to_string()), seen.clone()).await;
        let state = test_state_with_gemini_and_loopguard(base_url, mode).await;
        let mut session = ground_out_session(session_id, &owner, 10.0);
        session.steps = vec![StepText {
            prompt: "Do the task".to_string(),
            action: r#"click({"point_2d": [834, 96]})"#.to_string(),
            ..StepText::default()
        }];
        session.images = vec![ImageEntry {
            step_index: 0,
            image: EFFECT_SCREEN_A.to_string(),
        }];
        state.sessions.insert_session(session).await.unwrap();
        let app = routes::router(state.clone());
        let response = app
            .clone()
            .oneshot(effect_check_request(session_id, &owner, next_screen))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "LOOPGUARD_MODE={mode}: step must succeed");
        let payloads = seen
            .lock()
            .unwrap()
            .iter()
            .map(|payload| payload.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(payloads.contains("Do the task"), "LOOPGUARD_MODE={mode}: model never called");
        (payloads, state)
    }

    async fn recorded_prompts(state: &AppState, session_id: &str) -> Vec<String> {
        let session = state.sessions.get_session(session_id).await.unwrap().unwrap();
        session.steps.into_iter().map(|step| step.prompt).collect()
    }

    #[tokio::test]
    async fn shadow_mode_sends_no_effect_check_note_to_the_model() {
        let _fixture = LOOPGUARD_FIXTURE_LOCK.lock().await;
        let (payloads, _) = step_after_tap("shadow", "hx-effect-shadow", EFFECT_SCREEN_A).await;
        assert!(
            !payloads.contains(NOTE_MARKER) && !payloads.contains(NOTE_TEXT),
            "LOOPGUARD_MODE=shadow must not steer the model with the effect-check note"
        );
    }

    #[tokio::test]
    async fn off_mode_sends_no_effect_check_note_to_the_model() {
        let _fixture = LOOPGUARD_FIXTURE_LOCK.lock().await;
        let (payloads, _) = step_after_tap("off", "hx-effect-off", EFFECT_SCREEN_A).await;
        assert!(
            !payloads.contains(NOTE_MARKER) && !payloads.contains(NOTE_TEXT),
            "LOOPGUARD_MODE=off must not steer the model with the effect-check note"
        );
    }

    #[tokio::test]
    async fn non_enforcing_modes_record_no_effect_check_note_in_the_step() {
        let _fixture = LOOPGUARD_FIXTURE_LOCK.lock().await;
        for mode in ["shadow", "off"] {
            let session_id = format!("hx-effect-record-{mode}");
            let (_, state) = step_after_tap(mode, &session_id, EFFECT_SCREEN_A).await;
            let prompts = recorded_prompts(&state, &session_id).await;
            assert!(prompts.len() >= 2, "LOOPGUARD_MODE={mode}: the new step must be recorded");
            for prompt in &prompts {
                assert!(
                    !prompt.contains(NOTE_MARKER),
                    "LOOPGUARD_MODE={mode}: a recorded step prompt reaches later model turns: {prompt}"
                );
            }
        }
    }

    #[tokio::test]
    async fn enforce_mode_still_delivers_the_note_for_a_dead_tap() {
        let _fixture = LOOPGUARD_FIXTURE_LOCK.lock().await;
        let (payloads, _) = step_after_tap("enforce", "hx-effect-enforce", EFFECT_SCREEN_A).await;
        assert!(payloads.contains(NOTE_MARKER), "enforce must deliver the effect-check note");
        assert!(payloads.contains(NOTE_TEXT), "enforce must keep the note's wording");
    }

    #[tokio::test]
    async fn enforce_mode_stays_silent_when_the_screen_moved() {
        let _fixture = LOOPGUARD_FIXTURE_LOCK.lock().await;
        let (payloads, _) = step_after_tap("enforce", "hx-effect-moved", EFFECT_SCREEN_B).await;
        assert!(
            !payloads.contains(NOTE_MARKER),
            "a moving screen must not be reported as unchanged"
        );
    }

    /// Captures this thread's INFO logs, where shadow LoopGuard reports what it
    /// would have done.
    #[derive(Clone)]
    struct LogBuffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
        type Writer = LogBuffer;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Screen evidence still feeds LoopGuard in shadow mode. The same tap twice
    /// on an unchanged screen is a screen-confirmed dead button, which the guard
    /// scores one strike before the blind identical-run threshold; shadow
    /// reports that decision without acting on it. Each recorded step also
    /// keeps its screen fingerprint for later revisit checks.
    #[tokio::test]
    async fn shadow_mode_still_scores_unchanged_screens() {
        let _fixture = LOOPGUARD_FIXTURE_LOCK.lock().await;
        let logs = LogBuffer(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        let session_id = "hx-effect-shadow-score";
        let owner = "hx-effect-shadow-score@theagi.company";
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base_url =
            spawn_openai_action_server_with_seen(Arc::new(DEAD_TAP.to_string()), seen.clone()).await;
        let state = test_state_with_gemini_and_loopguard(base_url, "shadow").await;
        state
            .sessions
            .insert_session(ground_out_session(session_id, owner, 10.0))
            .await
            .unwrap();
        let app = routes::router(state.clone());
        for step in 1..=2 {
            let response = app
                .clone()
                .oneshot(effect_check_request(session_id, owner, EFFECT_SCREEN_A))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "step {step} must succeed");
            let body = response_json(response).await;
            assert_eq!(body["action"]["name"], "click", "shadow never alters the action: {body}");
        }
        let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let shadow_lines: Vec<&str> = logs
            .lines()
            .filter(|line| line.contains("loopguard_shadow") && line.contains(session_id))
            .collect();
        assert!(
            shadow_lines.iter().any(|line| line.contains("rung=\"inject\"")),
            "shadow must still score a repeated tap on an unchanged screen; loopguard logs: {:?}",
            logs.lines().filter(|line| line.contains("loopguard")).collect::<Vec<_>>()
        );
        let session = state.sessions.get_session(session_id).await.unwrap().unwrap();
        assert_eq!(session.steps.len(), 2);
        assert!(
            session.steps.iter().all(|step| step.screen_fingerprint.is_some()),
            "every recorded step keeps its screen fingerprint in shadow mode"
        );
        // Whether the note reaches the model is graded by the cases above;
        // this case isolates the preserved evidence path.
        assert!(!seen.lock().unwrap().is_empty(), "model never called");
    }
}
