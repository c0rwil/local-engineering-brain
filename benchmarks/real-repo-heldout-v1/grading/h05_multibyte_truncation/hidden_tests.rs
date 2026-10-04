// Private held-out behavioral tests (h05). Written by the grader as a new
// integration-test crate in a disposable grading copy. Uses only the public
// human-response prompt builder, never the private helper the fix changes.
mod engram_heldout_truncation {
    use agi_backend_rust::models::runtime_metadata::TaskMetadata;
    use agi_backend_rust::models::session::{SessionData, StepText};
    use agi_backend_rust::services::human_response::{
        build_human_response_context, build_human_response_prompt,
    };

    const SUFFIX: &str = "... [truncated]";
    const FIELD_LIMIT: usize = 4000;
    const HISTORY_LIMIT: usize = 1200;

    /// The contract, measured in characters: a value within the limit is kept
    /// whole; a longer one keeps as many leading characters as fit before the
    /// existing suffix, with trailing whitespace trimmed as before. For ASCII
    /// this is exactly the historical output.
    fn expected(value: &str, limit: usize) -> String {
        if value.chars().count() <= limit {
            return value.to_string();
        }
        let head: String = value.chars().take(limit - SUFFIX.chars().count()).collect();
        format!("{}{}", head.trim_end(), SUFFIX)
    }

    fn session(
        task_prompt: Option<String>,
        metadata: Option<TaskMetadata>,
        mut steps: Vec<StepText>,
        question: &str,
    ) -> SessionData {
        let mut session = SessionData::default();
        session.meta.session_id = "heldout-truncation".to_string();
        session.meta.task_prompt = task_prompt;
        session.meta.task_metadata = metadata;
        steps.push(StepText {
            action: format!("message_user({})", serde_json::json!({ "content": question })),
            ..StepText::default()
        });
        session.steps = steps;
        session
    }

    fn prompt_for(session: &SessionData) -> String {
        let context =
            build_human_response_context(session).expect("the latest step asks the human");
        build_human_response_prompt(&context)
    }

    fn section<'a>(prompt: &'a str, header: &str) -> &'a str {
        let start = prompt
            .find(header)
            .unwrap_or_else(|| panic!("missing section {header:?}"))
            + header.len();
        let rest = &prompt[start..];
        match rest.find("\n\n") {
            Some(end) => &rest[..end],
            None => rest,
        }
    }

    fn line_after<'a>(prompt: &'a str, label: &str) -> &'a str {
        let line = prompt
            .lines()
            .find(|line| line.starts_with(label))
            .unwrap_or_else(|| panic!("missing transcript line {label:?}"));
        &line[label.len()..]
    }

    #[test]
    fn multibyte_task_prompt_cut_inside_a_character_does_not_panic() {
        // Two-byte characters: the historical byte cut lands inside one.
        let task = "é".repeat(5000);
        let prompt = prompt_for(&session(Some(task.clone()), None, vec![], "Which account?"));
        let field = section(&prompt, "Original task prompt:\n");
        assert_eq!(field, expected(&task, FIELD_LIMIT));
        assert_eq!(field.chars().count(), FIELD_LIMIT);
    }

    #[test]
    fn multibyte_context_fields_and_question_truncate_by_characters() {
        let outcome = "中".repeat(4500);
        let human_context = "😀".repeat(4100);
        let question = "ü".repeat(4321);
        let metadata = TaskMetadata {
            desired_outcome: Some(outcome.clone()),
            llm_human_context: Some(human_context.clone()),
            ..TaskMetadata::default()
        };
        let prompt = prompt_for(&session(
            Some("Pay the electricity bill".to_string()),
            Some(metadata),
            vec![],
            &question,
        ));
        assert_eq!(section(&prompt, "Desired outcome:\n"), expected(&outcome, FIELD_LIMIT));
        assert_eq!(
            section(&prompt, "Additional human context:\n"),
            expected(&human_context, FIELD_LIMIT)
        );
        assert_eq!(
            section(&prompt, "Latest question from the agent:\n"),
            expected(&question, FIELD_LIMIT)
        );
    }

    #[test]
    fn multibyte_transcript_entries_truncate_by_characters() {
        // Earlier-step history uses its own, shorter limit.
        let long_prompt = "😀".repeat(1300);
        let long_output = "ß".repeat(1250);
        let within_limit = format!("a{}", "é".repeat(1000));
        let steps = vec![
            StepText {
                prompt: long_prompt.clone(),
                raw_output: Some(long_output.clone()),
                ..StepText::default()
            },
            StepText {
                prompt: within_limit.clone(),
                ..StepText::default()
            },
        ];
        let prompt = prompt_for(&session(
            Some("Find the receipt".to_string()),
            None,
            steps,
            "Which receipt?",
        ));
        let prompts: Vec<&str> = prompt
            .lines()
            .filter_map(|line| line.strip_prefix("- Step prompt: "))
            .collect();
        assert_eq!(prompts.len(), 2, "both earlier step prompts stay in the transcript");
        assert_eq!(prompts[0], expected(&long_prompt, HISTORY_LIMIT));
        assert_eq!(prompts[1], within_limit, "a field within the limit is kept whole");
        assert_eq!(
            line_after(&prompt, "- Useful step output: "),
            expected(&long_output, HISTORY_LIMIT)
        );
    }

    #[test]
    fn ascii_fields_truncate_exactly_as_before() {
        let task = "word ".repeat(1000).trim_end().to_string();
        let metadata = TaskMetadata {
            desired_outcome: Some("x".repeat(FIELD_LIMIT)),
            llm_human_context: Some("y".repeat(FIELD_LIMIT + 1)),
            ..TaskMetadata::default()
        };
        let prompt = prompt_for(&session(Some(task.clone()), Some(metadata), vec![], "Which one?"));
        let field = section(&prompt, "Original task prompt:\n");
        assert_eq!(field, expected(&task, FIELD_LIMIT));
        assert!(field.ends_with("word... [truncated]"), "{}", &field[field.len() - 40..]);
        assert_eq!(section(&prompt, "Desired outcome:\n"), "x".repeat(FIELD_LIMIT));
        assert_eq!(
            section(&prompt, "Additional human context:\n"),
            format!("{}{}", "y".repeat(FIELD_LIMIT - SUFFIX.len()), SUFFIX)
        );
    }

    #[test]
    fn multibyte_text_within_the_character_limit_is_kept_whole() {
        // 3001 characters but 6001 bytes: the limit counts characters.
        let task = format!("a{}", "é".repeat(3000));
        let prompt = prompt_for(&session(Some(task.clone()), None, vec![], "Which account?"));
        assert_eq!(section(&prompt, "Original task prompt:\n"), task);
    }
}
