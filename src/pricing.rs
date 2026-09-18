use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl Tokens {
    /// No tokens of any kind, so no cost at any price.
    pub fn is_empty(&self) -> bool {
        *self == Tokens::default()
    }

    pub fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_creation += other.cache_creation;
    }
}

/// Price per million tokens: (input, output).
/// Cache read is input/10; cache creation is input * 1.25.
/// WARNING: Matching is by prefix (model.starts_with(id)). No id may be a
/// prefix of another: if violated, a shorter entry silently shadows the
/// longer one and returns the wrong price with no error.
const PRICES: &[(&str, f64, f64)] = &[
    ("claude-opus-5", 15.0, 75.0),
    ("claude-fable-5-1", 15.0, 75.0),
    ("claude-sonnet-5", 3.0, 15.0),
    ("claude-haiku-4-5", 1.0, 5.0),
];

fn prices_for(model: &str) -> Option<(f64, f64)> {
    PRICES
        .iter()
        .find(|(id, _, _)| model.starts_with(id))
        .map(|(_, i, o)| (*i, *o))
}

fn cost_of((pin, pout): (f64, f64), t: &Tokens) -> f64 {
    let m = 1_000_000.0;
    (t.input as f64 / m) * pin
        + (t.output as f64 / m) * pout
        + (t.cache_read as f64 / m) * (pin / 10.0)
        + (t.cache_creation as f64 / m) * (pin * 1.25)
}

/// Theoretical list-price cost of a session, each model's tokens priced at
/// that model's own rate. On a Max plan the user pays nothing per token, so
/// callers must label this as an estimate.
///
/// `None` when a model that actually spent tokens is missing from the table.
/// A partial sum would be indistinguishable from a complete one, and the day
/// Anthropic ships an id this table has never seen, a confident `US$ 0.00`
/// next to millions of tokens is the worst of the available answers.
///
/// Entries that spent nothing are skipped before that test, because the
/// condition is whether an unknown *cost* is being left out, not whether an
/// unknown *name* is present. Transcripts carry `<synthetic>` for messages
/// that never came from inference — an API error, an interruption — always
/// with every counter at zero. Those are not a model the table is missing;
/// they are not a model at all, and letting three of them erase the cost of
/// two hundred million real tokens is the confident-wrong-answer failure
/// running in the other direction.
pub fn cost_usd(by_model: &HashMap<String, Tokens>) -> Option<f64> {
    by_model
        .iter()
        .filter(|(_, t)| !t.is_empty())
        .try_fold(0.0, |acc, (model, t)| {
            Some(acc + cost_of(prices_for(model)?, t))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: Option<f64>, b: f64) {
        let a = a.expect("expected a priced session");
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    fn million(field: fn(&mut Tokens) -> &mut u64) -> Tokens {
        let mut t = Tokens::default();
        *field(&mut t) = 1_000_000;
        t
    }

    fn session<const N: usize>(entries: [(&str, Tokens); N]) -> HashMap<String, Tokens> {
        entries
            .into_iter()
            .map(|(m, t)| (m.to_string(), t))
            .collect()
    }

    #[test]
    fn opus_input_and_output() {
        approx(
            cost_usd(&session([("claude-opus-5", million(|t| &mut t.input))])),
            15.0,
        );
        approx(
            cost_usd(&session([("claude-opus-5", million(|t| &mut t.output))])),
            75.0,
        );
    }

    #[test]
    fn cache_read_is_a_tenth_of_input() {
        approx(
            cost_usd(&session([(
                "claude-opus-5",
                million(|t| &mut t.cache_read),
            )])),
            1.5,
        );
    }

    #[test]
    fn cache_creation_carries_a_premium() {
        approx(
            cost_usd(&session([(
                "claude-opus-5",
                million(|t| &mut t.cache_creation),
            )])),
            18.75,
        );
    }

    #[test]
    fn model_id_suffixes_still_match() {
        approx(
            cost_usd(&session([(
                "claude-opus-5-20260101",
                million(|t| &mut t.input),
            )])),
            15.0,
        );
    }

    #[test]
    fn a_mixed_session_prices_each_model_at_its_own_rate() {
        // A long Opus run with a trailing background Haiku call. Pricing the
        // whole transcript at whichever model answered last was out by 15x.
        let cost = cost_usd(&session([
            ("claude-opus-5", million(|t| &mut t.output)),
            ("claude-haiku-4-5", million(|t| &mut t.output)),
        ]));
        approx(cost, 75.0 + 5.0);
    }

    #[test]
    fn an_unknown_model_makes_the_whole_estimate_unknown() {
        // Not 15.0 and not 0.0: part of the session has no price, so the
        // total is not a number anyone should be shown.
        assert_eq!(
            cost_usd(&session([
                ("claude-opus-5", million(|t| &mut t.input)),
                ("a-model-shipped-tomorrow", million(|t| &mut t.input)),
            ])),
            None
        );
        assert_eq!(
            cost_usd(&session([(
                "a-model-shipped-tomorrow",
                million(|t| &mut t.input)
            )])),
            None
        );
    }

    #[test]
    fn empty_tokens_cost_nothing() {
        approx(
            cost_usd(&session([("claude-opus-5", Tokens::default())])),
            0.0,
        );
        approx(cost_usd(&HashMap::new()), 0.0);
    }

    #[test]
    fn a_model_that_spent_nothing_does_not_erase_the_bill() {
        // Real transcripts carry `<synthetic>` for messages that never came
        // from inference, always with every counter at zero. Three of them
        // were enough to blank the cost of a whole session.
        let mut mixed = session([("claude-opus-5", million(|t| &mut t.input))]);
        mixed.insert("<synthetic>".into(), Tokens::default());
        approx(cost_usd(&mixed), 15.0);
    }

    #[test]
    fn a_model_that_actually_spent_tokens_still_blanks_the_bill() {
        // The guarantee this function exists for: an id the table has never
        // seen, with real tokens behind it, must not be quietly priced at
        // zero and summed into a confident total.
        let mut mixed = session([("claude-opus-5", million(|t| &mut t.input))]);
        mixed.insert("claude-from-the-future".into(), million(|t| &mut t.output));
        assert_eq!(cost_usd(&mixed), None);
    }
}
