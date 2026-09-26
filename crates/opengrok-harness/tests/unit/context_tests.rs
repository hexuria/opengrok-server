use super::*;
use crate::model::{ImagePart, ToolCallRef};

fn words(role: &str, bytes: usize) -> ChatMessage {
    ChatMessage::text(role, "x".repeat(bytes))
}

fn asked(model: &str, limit: Option<u64>, messages: Vec<ChatMessage>) -> ModelRequest {
    ModelRequest {
        model: model.to_string(),
        messages,
        system: Some("You are a coworker.".to_string()),
        context_tokens: limit,
        ..Default::default()
    }
}

#[test]
fn text_is_counted_by_its_bytes_so_cjk_is_not_read_at_a_quarter_of_its_cost() {
    let latin = asked("m", None, vec![ChatMessage::text("user", "a".repeat(300))]);
    let cjk = asked("m", None, vec![ChatMessage::text("user", "字".repeat(300))]);
    // 300 CJK characters are 900 bytes: three times the Latin estimate, not the same.
    assert!(estimate_tokens(&cjk) >= estimate_tokens(&latin) + 190);
}

#[test]
fn a_picture_is_a_fixed_cost_not_its_base64() {
    let mut shot = ChatMessage::text("tool", "screen");
    shot.images.push(ImagePart {
        mime: "image/png".to_string(),
        base64: "A".repeat(300_000),
    });
    let request = asked("m", None, vec![shot]);
    let estimate = estimate_tokens(&request);
    assert!(estimate < 2_000, "{estimate}");
    assert!(estimate >= IMAGE_TOKENS);
}

#[test]
fn a_request_with_no_limit_is_only_counted() {
    let mut request = asked("m", None, vec![words("user", 3_000_000)]);
    let before = request.clone();
    let mut window = Window::at_entry(&request);
    assert!(window.fit(&mut request).is_ok());
    assert_eq!(request.messages, before.messages);
    assert_eq!(request.system, before.system);
}

#[test]
fn a_request_that_fits_is_left_alone() {
    let mut request = asked(
        "m",
        Some(100_000),
        vec![words("user", 300), words("assistant", 300)],
    );
    let before = request.messages.clone();
    let mut window = Window::at_entry(&request);
    assert!(window.fit(&mut request).is_ok());
    assert_eq!(request.messages, before);
    assert_eq!(window.left_out(), 0);
}

#[test]
fn the_oldest_turns_go_first_and_this_turn_is_kept_whole() {
    // Ten earlier turns of ~2k tokens each, then this turn.
    let mut messages = Vec::new();
    for turn in 0..10 {
        messages.push(ChatMessage::text(
            "user",
            format!("question {turn} {}", "q".repeat(3_000)),
        ));
        messages.push(ChatMessage::text(
            "assistant",
            format!("answer {turn} {}", "a".repeat(3_000)),
        ));
    }
    messages.push(ChatMessage::text("user", "this turn"));
    let mut request = asked("m", Some(20_000), messages);
    let mut window = Window::at_entry(&request);
    let estimate = window.fit(&mut request).unwrap();

    let room = 20_000 - 5_000;
    assert!(
        estimate <= room / 10 * 8,
        "trimmed to 80% of the room, not the brim: {estimate}"
    );
    assert_eq!(request.messages.last().unwrap().content, "this turn");
    // Whole turns: what is left starts on a question, and every question keeps its answer.
    assert_eq!(request.messages[0].role, "user");
    assert!(request.messages[0].content.starts_with("question "));
    assert_eq!(window.left_out() % 2, 0);
    assert!(
        request
            .messages
            .iter()
            .any(|m| m.content.starts_with("answer 9"))
    );
    assert!(
        !request
            .messages
            .iter()
            .any(|m| m.content.starts_with("question 0"))
    );
    // The model is told, once, in the system prompt, and the prompt itself is kept.
    let system = request.system.unwrap();
    assert!(system.starts_with("You are a coworker."));
    assert!(system.contains(&format!("The {} oldest messages", window.left_out())));
}

#[test]
fn a_call_is_never_left_without_its_result() {
    let call = ChatMessage {
        role: "assistant".to_string(),
        tool_calls: vec![ToolCallRef {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: "{}".to_string(),
        }],
        ..Default::default()
    };
    let result = ChatMessage {
        role: "tool".to_string(),
        content: "r".repeat(30_000),
        tool_call_id: Some("call_1".to_string()),
        ..Default::default()
    };
    let messages = vec![
        words("user", 300),
        call,
        result,
        words("assistant", 300),
        ChatMessage::text("user", "now"),
    ];
    let mut request = asked("m", Some(12_000), messages);
    let mut window = Window::at_entry(&request);
    window.fit(&mut request).unwrap();
    let calls: Vec<_> = request
        .messages
        .iter()
        .flat_map(|m| &m.tool_calls)
        .collect();
    let results: Vec<_> = request
        .messages
        .iter()
        .filter_map(|m| m.tool_call_id.as_ref())
        .collect();
    assert_eq!(calls.len(), results.len(), "{:?}", request.messages);
    assert_eq!(window.left_out(), 4);
}

#[test]
fn the_note_is_rewritten_not_stacked() {
    let mut messages = Vec::new();
    for _ in 0..6 {
        messages.push(words("user", 6_000));
        messages.push(words("assistant", 6_000));
    }
    messages.push(ChatMessage::text("user", "now"));
    let mut request = asked("m", Some(16_000), messages);
    let mut window = Window::at_entry(&request);
    window.fit(&mut request).unwrap();
    // A round later the run has grown; the second fit updates the count in place.
    request.messages.push(words("assistant", 12_000));
    window.fit(&mut request).unwrap();
    let system = request.system.unwrap();
    assert_eq!(system.matches("[harness]").count(), 1, "{system}");
    assert!(system.contains(&format!("The {} oldest", window.left_out())));
}

#[test]
fn this_turn_alone_too_long_fails_with_a_sentence_naming_the_model() {
    let messages = vec![
        words("user", 3_000),
        words("assistant", 3_000),
        words("user", 60_000),
    ];
    let mut request = asked("xai/grok-4.6", Some(8_000), messages);
    let mut window = Window::at_entry(&request);
    let why = window.fit(&mut request).unwrap_err();
    assert!(why.contains("too long for xai/grok-4.6"), "{why}");
    assert!(why.contains("of 8000 tokens"), "{why}");
    // What could go, went; this turn stayed.
    assert_eq!(request.messages.len(), 1);
}

#[test]
fn with_no_user_message_everything_is_protected_and_it_fails_closed() {
    let messages = vec![words("assistant", 30_000), words("tool", 30_000)];
    let mut request = asked("m", Some(8_000), messages);
    let mut window = Window::at_entry(&request);
    assert!(window.fit(&mut request).is_err());
    assert_eq!(request.messages.len(), 2);
    assert_eq!(window.left_out(), 0);
}
