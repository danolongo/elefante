use elefante_core::DEFAULT_SYSTEM_PROMPT;

#[test]
fn standing_prompt_stays_small() {
    let words = DEFAULT_SYSTEM_PROMPT.split_whitespace().count();
    assert!(
        words < 300,
        "conservative prompt proxy grew to {words} words"
    );
}
