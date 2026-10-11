use elefante_core::DEFAULT_SYSTEM_PROMPT;

#[test]
fn standing_prompt_stays_small() {
    let words = DEFAULT_SYSTEM_PROMPT.split_whitespace().count();
    assert!(
        words < 300,
        "conservative prompt proxy grew to {words} words"
    );
}

#[test]
fn the_prompt_with_the_index_hint_stays_small() {
    let words = format!("{DEFAULT_SYSTEM_PROMPT} {}", elefante_core::MEMORY_USE_HINT)
        .split_whitespace()
        .count();
    assert!(words < 300, "prompt with the hint grew to {words} words");
}
