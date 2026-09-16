#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl Tokens {
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

/// Theoretical list-price cost. On a Max plan the user pays nothing per
/// token, so callers must label this as an estimate.
pub fn cost_usd(model: &str, t: &Tokens) -> f64 {
    let Some((pin, pout)) = prices_for(model) else {
        return 0.0;
    };
    let m = 1_000_000.0;
    (t.input as f64 / m) * pin
        + (t.output as f64 / m) * pout
        + (t.cache_read as f64 / m) * (pin / 10.0)
        + (t.cache_creation as f64 / m) * (pin * 1.25)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn opus_input_and_output() {
        let t = Tokens {
            input: 1_000_000,
            output: 0,
            cache_read: 0,
            cache_creation: 0,
        };
        approx(cost_usd("claude-opus-5", &t), 15.0);

        let t = Tokens {
            input: 0,
            output: 1_000_000,
            cache_read: 0,
            cache_creation: 0,
        };
        approx(cost_usd("claude-opus-5", &t), 75.0);
    }

    #[test]
    fn cache_read_is_a_tenth_of_input() {
        let t = Tokens {
            input: 0,
            output: 0,
            cache_read: 1_000_000,
            cache_creation: 0,
        };
        approx(cost_usd("claude-opus-5", &t), 1.5);
    }

    #[test]
    fn cache_creation_carries_a_premium() {
        let t = Tokens {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_creation: 1_000_000,
        };
        approx(cost_usd("claude-opus-5", &t), 18.75);
    }

    #[test]
    fn model_id_suffixes_still_match() {
        let t = Tokens {
            input: 1_000_000,
            output: 0,
            cache_read: 0,
            cache_creation: 0,
        };
        approx(cost_usd("claude-opus-5-20260101", &t), 15.0);
    }

    #[test]
    fn unknown_model_costs_zero_rather_than_guessing() {
        let t = Tokens {
            input: 1_000_000,
            output: 0,
            cache_read: 0,
            cache_creation: 0,
        };
        approx(cost_usd("some-future-model", &t), 0.0);
    }

    #[test]
    fn empty_tokens_cost_nothing() {
        approx(cost_usd("claude-opus-5", &Tokens::default()), 0.0);
    }
}
