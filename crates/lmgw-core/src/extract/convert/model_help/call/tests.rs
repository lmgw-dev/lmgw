use super::*;

fn bound() -> Bound {
    Bound::Counted {
        max_tokens: 7050,
        context: 8192,
        prompt: 1142,
        image_counted: true,
    }
}

fn judge(finish: FinishReason, content: &[ContentPart]) -> Asked {
    judged(finish, content, Usage::default(), bound(), None, false)
}

fn failed(finish: FinishReason, content: &[ContentPart]) -> String {
    match judge(finish, content) {
        Asked::Failed(why) => why,
        other => panic!("taken for a page: {other:?}"),
    }
}

#[test]
fn an_answer_that_ran_to_its_bound_is_no_page_and_names_the_bound() {
    let text = [ContentPart::text("half a page")];
    let usage = Usage {
        completion_tokens: Some(7050),
        ..Default::default()
    };
    let Asked::Failed(why) = judged(FinishReason::Length, &text, usage, bound(), None, false)
    else {
        panic!("a cut answer was taken for a page");
    };
    assert!(why.contains("after 7050 tokens"), "{why}");
    assert!(why.contains("max_tokens 7050"), "{why}");
    assert!(!why.contains("half a page"));
    assert_eq!(
        judge(FinishReason::Stop, &text),
        Asked::Text {
            text: "half a page".into(),
            handed_to: None,
            doctags: false,
        }
    );
    assert!(matches!(
        judge(FinishReason::StopSequence("END".into()), &text),
        Asked::Text { .. }
    ));
    let empty = [ContentPart::text("  \n")];
    assert!(failed(FinishReason::Stop, &empty).contains("no text"));
    assert!(failed(FinishReason::ContentFilter, &text).contains("filtered"));
}

/// S1: a refusal arrives with `finish_reason: stop`; its words are the
/// model's about the request, never the page's text.
#[test]
fn a_refusal_is_a_failed_page_whatever_else_the_answer_holds() {
    let refusal = ContentPart::Refusal {
        text: "I'm sorry, I can't assist with that".into(),
    };
    let why = failed(FinishReason::Stop, std::slice::from_ref(&refusal));
    assert!(why.contains("refused"), "{why}");
    assert!(!why.contains("sorry"), "{why}");
    let mixed = [ContentPart::text("Some of the page"), refusal];
    let why = failed(FinishReason::Stop, &mixed);
    assert!(
        why.contains("refused") && !why.contains("Some of the page"),
        "{why}"
    );
}

/// S1: only a stop is a whole answer.
#[test]
fn any_finish_reason_but_a_stop_is_a_failed_page_naming_it() {
    let text = [ContentPart::text("most of a page")];
    let why = failed(
        FinishReason::Other("model_context_window_exceeded".into()),
        &text,
    );
    assert!(
        why.contains("the finish reason 'model_context_window_exceeded'"),
        "{why}"
    );
    assert!(!why.contains("most of a page"));
    assert!(failed(FinishReason::Other("pause_turn".into()), &text).contains("'pause_turn'"));
    assert!(failed(FinishReason::ToolUse, &text).contains("tool call"));
    // An upstream's reason is its own text: only a plain word is quoted.
    for odd in ["", "<script>alert(1)</script>", &"x".repeat(65)] {
        let why = failed(FinishReason::Other(odd.to_string()), &text);
        assert!(why.contains("a finish reason lmgw does not know"), "{why}");
        assert!(!why.contains("script") && !why.contains("xxxx"), "{why}");
    }
}

#[test]
fn the_request_carries_the_image_the_prompt_and_nothing_about_the_caller() {
    let ir = request("example-vision", b"png", "image/png", "Transcribe.".into());
    let sent = serde_json::to_string(&ir).unwrap();
    assert!(sent.contains("Transcribe.") && sent.contains("image/png"));
    assert_eq!(ir.params.temperature, Some(0.0));
    assert_eq!(ir.messages.len(), 1);
}

#[test]
fn a_page_s_prompt_follows_the_form_its_answering_model_is_asked_in() {
    let page = PageAsk {
        ask: Ask::ImageAndText,
        text: "Rent   800",
    };
    assert_eq!(page.prompt(true), prompts::DOCTAGS_PROMPT);
    assert!(page.prompt(false).ends_with("Extracted text:\nRent   800"));
}
