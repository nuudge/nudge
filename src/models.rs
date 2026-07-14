use anyhow::Result;

use crate::llm;

// (display label, API model id). The TUI's /model picker renders labels;
// the id is what goes on the wire. Used as-is by guest/--connect clients
// (no local API key to fetch with) and as the fallback when the owning
// process's `list_models` call fails (offline, bad key, etc).
pub const MODELS: &[(&str, &str)] = &[
    ("Fable 5", "claude-fable-5"),
    ("Mythos 5", "claude-mythos-5"),
    ("Mythos Preview", "claude-mythos-preview"),
    ("Opus 4.8", "claude-opus-4-8"),
    ("Opus 4.7", "claude-opus-4-7"),
    ("Opus 4.6", "claude-opus-4-6"),
    ("Sonnet 4.6", "claude-sonnet-4-6"),
];
pub const DEFAULT_MODEL: &str = "claude-opus-4-8";

pub fn owned_models(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(l, i)| (l.to_string(), i.to_string()))
        .collect()
}

// Refreshes the /model picker from the provider so newly released models show
// up without a code change; keeps the static `fallback` on any fetch failure.
pub async fn resolve_models(
    provider: &llm::AnthropicProvider,
    fallback: &[(&str, &str)],
) -> Vec<(String, String)> {
    pick_models(provider.list_models().await, fallback)
}

fn pick_models(
    fetched: Result<Vec<(String, String)>>,
    fallback: &[(&str, &str)],
) -> Vec<(String, String)> {
    match fetched {
        Ok(models) if !models.is_empty() => models,
        Ok(_) => owned_models(fallback),
        Err(e) => {
            eprintln!("[models] falling back to built-in list: {e:#}");
            owned_models(fallback)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FALLBACK: &[(&str, &str)] = &[("Fallback", "fallback-1")];

    #[test]
    fn pick_models_prefers_a_nonempty_fetch() {
        let fetched = Ok(vec![("Fresh".to_string(), "fresh-1".to_string())]);
        assert_eq!(
            pick_models(fetched, FALLBACK),
            vec![("Fresh".to_string(), "fresh-1".to_string())]
        );
    }

    #[test]
    fn pick_models_falls_back_on_empty_fetch() {
        assert_eq!(
            pick_models(Ok(Vec::new()), FALLBACK),
            owned_models(FALLBACK)
        );
    }

    #[test]
    fn pick_models_falls_back_on_fetch_error() {
        let fetched = Err(anyhow::anyhow!("network down"));
        assert_eq!(pick_models(fetched, FALLBACK), owned_models(FALLBACK));
    }
}
