Generating a human response can panic when a long text field contains multi-byte characters such as accented letters, CJK text or emoji: truncation cuts at a byte index inside a character.

Building the human-response prompt must never panic on any field it includes: the task prompt, desired outcome, additional human context, the latest question and the earlier-step transcript entries. Field limits count characters, not bytes. Text within a limit is kept whole. Longer text keeps as many leading characters as fit before the existing `... [truncated]` suffix, with trailing whitespace before the suffix trimmed as today, so the result never exceeds the limit. ASCII output must stay byte-for-byte identical to today's.

Add focused regression coverage.
