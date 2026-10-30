use goon::reasoning::{visible_answer, AnswerPart, AnswerProjection};

fn project(chunks: &[&str], policy: &str) -> (String, String) {
    let mut projection = AnswerProjection::new(policy);
    let mut parts = vec![];
    for chunk in chunks {
        parts.extend(projection.push(chunk));
    }
    parts.extend(projection.finish());
    let mut answer = String::new();
    let mut reasoning = String::new();
    for part in parts {
        match part {
            AnswerPart::Text(t) => answer.push_str(&t),
            AnswerPart::Reasoning(r) => reasoning.push_str(&r),
        }
    }
    (answer, reasoning)
}

#[test]
fn inline_reasoning_projection_is_invariant_across_every_chunk_boundary() {
    let cases = [
        ("split markers and Unicode", "\n<think>ẩn 🐧\nsecond line</think>Answer ✓", "Answer ✓", "ẩn 🐧\nsecond line"),
        ("multiple leading blocks", "<think>one</think>\n<THINK>two</THINK>\nAnswer", "\nAnswer", "onetwo"),
        ("nested blocks", "<think>outer<think>inner</think>end</think>Answer", "Answer", "outerinnerend"),
        ("unclosed block", "<think>private</thi", "", "private</thi"),
        ("empty reasoning", "<think></think>Answer", "Answer", ""),
        ("literal code fence", "```xml\n<think>literal</think>\n```", "```xml\n<think>literal</think>\n```", ""),
        ("literal later in answer", "Use `<think>...</think>` literally.", "Use `<think>...</think>` literally.", ""),
        ("partial opening at clean end", "  <thi", "  <thi", ""),
        ("no markers, exact whitespace", "\n  ordinary < than\n", "\n  ordinary < than\n", ""),
    ];
    for (name, input, answer, reasoning) in cases {
        let expected = (answer.to_string(), reasoning.to_string());
        let mut cuts: Vec<usize> = input.char_indices().map(|(i, _)| i).collect();
        cuts.push(input.len());
        for cut in cuts {
            assert_eq!(project(&[&input[..cut], &input[cut..]], ""), expected, "{name} cut at {cut}");
        }
        let chars: Vec<String> = input.chars().map(String::from).collect();
        let refs: Vec<&str> = chars.iter().map(String::as_str).collect();
        assert_eq!(project(&refs, ""), expected, "{name} per character");
    }
}

#[test]
fn inline_extraction_off_preserves_literal_leading_tags() {
    let raw = "<think>literal document element</think> body";
    assert_eq!(visible_answer(raw, "off"), raw);
}

#[test]
fn large_token_sized_reasoning_stream_is_discarded_without_losing_the_final_answer() {
    let mut p = AnswerProjection::new("");
    assert!(p.push("<think>").is_empty());
    for _ in 0..20_000 {
        assert_eq!(p.push("x"), vec![AnswerPart::Reasoning("x".into())]);
    }
    assert_eq!(p.push("</think>done"), vec![AnswerPart::Text("done".into())]);
}
