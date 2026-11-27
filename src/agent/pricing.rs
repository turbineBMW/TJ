//! What a model response cost, from its token counts and Anthropic's list prices, so a thread's
//! cost can climb while the agent works. Claude Code only reports its own figure when the turn
//! ends; that figure replaces this estimate, so a price that is out of date here is only wrong
//! until then.
//!
//! Prices are first-party list prices in dollars per million tokens. A model missing here gets no
//! running estimate, only the figure at the end of the turn.

use serde_json::Value;

/// Dollars per million tokens. A cache write costs 1.25 times input for five minutes and twice
/// input for an hour.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rates {
    input: f64,
    output: f64,
    cache_read: f64,
}

/// Matched in order against the model id, so a longer name comes before one it contains
/// (`opus-5-5` before `opus-5`).
const PRICES: &[(&str, Rates)] = &[
    ("fable-5-2", Rates { input: 8.0, output: 40.0, cache_read: 0.20 }),
    ("mythos-5-2", Rates { input: 8.0, output: 40.0, cache_read: 0.20 }),
    ("fable-5", Rates { input: 10.0, output: 50.0, cache_read: 1.00 }),
    ("mythos-5", Rates { input: 10.0, output: 50.0, cache_read: 1.00 }),
    ("opus-5-5", Rates { input: 4.0, output: 20.0, cache_read: 0.20 }),
    ("opus-5", Rates { input: 5.0, output: 25.0, cache_read: 0.50 }),
    ("opus-4-8", Rates { input: 5.0, output: 25.0, cache_read: 0.50 }),
    ("opus-4-7", Rates { input: 5.0, output: 25.0, cache_read: 0.50 }),
    ("opus-4-6", Rates { input: 5.0, output: 25.0, cache_read: 0.50 }),
    ("opus-4-5", Rates { input: 5.0, output: 25.0, cache_read: 0.50 }),
    ("sonnet-5-5", Rates { input: 2.0, output: 10.0, cache_read: 0.10 }),
    ("sonnet-5", Rates { input: 2.0, output: 10.0, cache_read: 0.20 }),
    ("sonnet-4", Rates { input: 3.0, output: 15.0, cache_read: 0.30 }),
    ("haiku-5-5", Rates { input: 0.10, output: 0.50, cache_read: 0.01 }),
    ("haiku-4-5", Rates { input: 1.0, output: 5.0, cache_read: 0.10 }),
];

/// Haiku 5.5 charges five times as much for a prompt over 100K tokens.
const HAIKU_5_5_LONG_PROMPT: u64 = 100_000;

fn rates(model: &str) -> Option<Rates> {
    PRICES.iter().find(|(name, _)| model.contains(name)).map(|(_, rates)| *rates)
}

/// What one response cost, from its `usage` as the Messages API reports it. `None` for a model
/// whose prices aren't known.
pub fn cost(model: &str, usage: &Value) -> Option<f64> {
    let mut rates = rates(model)?;
    let tokens = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let (input, output, cache_read) =
        (tokens("input_tokens"), tokens("output_tokens"), tokens("cache_read_input_tokens"));
    let cache_write = tokens("cache_creation_input_tokens");
    // Claude Code caches for an hour; the split says so when it's there.
    let hour = usage["cache_creation"]["ephemeral_1h_input_tokens"].as_u64().unwrap_or(0).min(cache_write);
    let five_minutes = cache_write - hour;
    let mut scale = 1.0;
    if model.contains("haiku-5-5") && input + cache_read + cache_write > HAIKU_5_5_LONG_PROMPT {
        scale *= 5.0;
    }
    // Fast mode is twice the price on every model that has it.
    if usage.get("speed").and_then(Value::as_str) == Some("fast") {
        scale *= 2.0;
    }
    rates = Rates { input: rates.input * scale, output: rates.output * scale, cache_read: rates.cache_read * scale };
    let dollars = rates.input * input as f64
        + rates.output * output as f64
        + rates.cache_read * cache_read as f64
        + rates.input * 1.25 * five_minutes as f64
        + rates.input * 2.0 * hour as f64;
    Some(dollars / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn longer_names_win() {
        assert_eq!(rates("claude-opus-5-5").unwrap().input, 4.0);
        assert_eq!(rates("claude-opus-5").unwrap().input, 5.0);
        assert_eq!(rates("claude-sonnet-5-5[1m]").unwrap().input, 2.0);
        assert_eq!(rates("claude-sonnet-4-6").unwrap().input, 3.0);
        assert_eq!(rates("claude-fable-5-1").unwrap().input, 10.0);
        assert_eq!(rates("claude-3-5-sonnet"), None);
    }

    #[test]
    fn fast_mode_and_long_haiku_prompts_cost_more() {
        let usage = json!({"input_tokens": 1_000_000});
        assert_eq!(cost("claude-opus-5-5", &usage), Some(4.0));
        assert_eq!(cost("claude-opus-5-5", &json!({"input_tokens": 1_000_000, "speed": "fast"})), Some(8.0));
        assert_eq!(cost("claude-haiku-5-5", &json!({"input_tokens": 1_000})), Some(0.0001));
        assert!((cost("claude-haiku-5-5", &usage).unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(cost("gpt-5", &usage), None);
    }

    /// Every result in the recorded sessions says what each model cost since the last one, with
    /// the tokens it was charged for. The prices here have to come to the same amount.
    #[test]
    fn prices_match_what_claude_code_charged() {
        let mut checked = 0;
        for fixture in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/claude")).unwrap() {
            let text = std::fs::read_to_string(fixture.unwrap().path()).unwrap();
            for line in text.lines() {
                let record: Value = serde_json::from_str(line).unwrap();
                let Some(models) = record["msg"]["model_usage_since_last_result"].as_object() else { continue };
                for (model, u) in models {
                    let usage = json!({
                        "input_tokens": u["inputTokens"],
                        "output_tokens": u["outputTokens"],
                        "cache_read_input_tokens": u["cacheReadInputTokens"],
                        "cache_creation_input_tokens": u["cacheCreationInputTokens"],
                        "cache_creation": {"ephemeral_1h_input_tokens": u["cacheCreationEphemeral1hInputTokens"]},
                    });
                    // The long-prompt price is per request; a result adds several up.
                    let prompt = ["inputTokens", "cacheReadInputTokens", "cacheCreationInputTokens"];
                    if prompt.iter().map(|k| u[k].as_u64().unwrap()).sum::<u64>() > HAIKU_5_5_LONG_PROMPT {
                        continue;
                    }
                    let charged = u["costUSD"].as_f64().unwrap();
                    let priced = cost(model, &usage).unwrap();
                    assert!((priced - charged).abs() < 1e-9, "{model}: priced {priced}, charged {charged}");
                    checked += 1;
                }
            }
        }
        assert!(checked >= 5, "only {checked} results had per-model costs");
    }
}
