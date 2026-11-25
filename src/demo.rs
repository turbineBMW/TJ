//! Canned agent sessions for `--demo` and `--replay`: they drive the real thread UI
//! through [`ScriptedAgent`](crate::agent::ScriptedAgent) without running a model.

use std::{path::Path, time::Duration};

use crate::agent::{
    AgentEvent, Spend, UsageWindow,
    claude::{Parser, read_fixture},
};
use crate::sample;

/// The sample reply, streamed like a real turn. `fast` is a stress test (about
/// 4,500 characters a second); otherwise it's about claude.ai's pace.
pub fn sample_stream(fast: bool) -> Vec<(Duration, AgentEvent)> {
    let (words, every) = if fast { (20, Duration::from_millis(16)) } else { (6, Duration::from_millis(100)) };
    let mut events = vec![
        (Duration::ZERO, AgentEvent::SessionInfo { session_id: "demo".into(), model: Some("claude-opus-5-5".into()) }),
        (Duration::ZERO, AgentEvent::Plan("max".into())),
        (Duration::ZERO, AgentEvent::Usage(sample_usage())),
        (Duration::ZERO, AgentEvent::Context { tokens: Some(186_000), window: Some(1_000_000) }),
        (Duration::from_millis(300), AgentEvent::TextStarted),
    ];
    events.extend(chunk_words(sample::REPLY, words).into_iter().map(|chunk| (every, AgentEvent::TextDelta(chunk))));
    events.push((
        Duration::ZERO,
        AgentEvent::TurnFinished { cost_usd: Some(0.0412), duration_ms: Some(9_200), is_error: false, error: None },
    ));
    events
}

/// A subscription part way through its windows, so the status column's meters have
/// something to show.
fn sample_usage() -> Vec<UsageWindow> {
    let now = chrono::Utc::now().timestamp();
    vec![
        UsageWindow {
            id: "five_hour".into(),
            utilization: 0.42,
            resets_at: Some(now + 2 * 3600 + 14 * 60),
            ..Default::default()
        },
        UsageWindow {
            id: "seven_day".into(),
            utilization: 0.81,
            resets_at: Some(now + 2 * 86_400 + 3600),
            severity: Some("warning".into()),
            ..Default::default()
        },
        UsageWindow {
            id: "spend_monthly".into(),
            utilization: 0.27,
            resets_at: Some(now + 6 * 86_400),
            spend: Some(Spend { used: 27.14, limit: Some(100.0), currency: "USD".into() }),
            ..Default::default()
        },
    ]
}

/// A recorded session from `tests/fixtures/claude/`, played back through the real parser.
pub fn replay(path: &Path) -> anyhow::Result<Vec<(Duration, AgentEvent)>> {
    let file = std::fs::File::open(path)?;
    let mut parser = Parser;
    Ok(read_fixture(file)
        .iter()
        .flat_map(|message| parser.parse(message))
        .map(|event| (Duration::from_millis(40), event))
        .collect())
}

/// Split text into chunks of about `words` words, keeping all whitespace so the
/// concatenation is byte-identical to the input.
pub fn chunk_words(text: &str, words: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0;
    let mut in_word = false;
    for ch in text.chars() {
        let is_space = ch.is_whitespace();
        if in_word && is_space {
            count += 1;
            if count >= words {
                chunks.push(std::mem::take(&mut current));
                count = 0;
            }
        }
        in_word = !is_space;
        current.push(ch);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::chunk_words;

    #[test]
    fn chunks_rejoin_to_the_input() {
        let text = crate::sample::REPLY;
        assert_eq!(chunk_words(text, 6).concat(), text);
        assert_eq!(chunk_words(text, 1).concat(), text);
    }
}
