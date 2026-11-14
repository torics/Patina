//! Hard limits on a run. Every agent in a run draws from the same tracker.
use crate::provider_config::Price;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
#[serde(default)]
pub struct Budgets {
    pub max_steps: u32,
    pub max_tokens: u64,
    /// Enforced only where a price is configured for the model.
    pub max_cost_usd: Option<f64>,
    /// `None` means unlimited.
    pub max_wall_clock_s: Option<u64>,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            max_steps: 50,
            max_tokens: 2_000_000,
            max_cost_usd: None,
            max_wall_clock_s: Some(2 * 3600),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct Usage {
    pub steps: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `None` while any used model has no configured price: the cost is unknown, not zero.
    pub cost_usd: Option<f64>,
    /// True when any token count was estimated rather than reported by the provider.
    pub estimated: bool,
    pub elapsed_s: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Limit {
    Steps,
    Tokens,
    Cost,
    WallClock,
}

impl Limit {
    pub fn describe(&self) -> &'static str {
        match self {
            Limit::Steps => "the step limit",
            Limit::Tokens => "the token limit",
            Limit::Cost => "the cost limit",
            Limit::WallClock => "the time limit",
        }
    }
}

struct Inner {
    budgets: Budgets,
    usage: Usage,
    cost_known: bool,
    /// Time used before this process picked the run up (after a resume).
    elapsed_before: Duration,
    started: Instant,
}

pub struct BudgetTracker(Mutex<Inner>);

impl BudgetTracker {
    pub fn new(budgets: Budgets) -> BudgetTracker {
        BudgetTracker::resume(budgets, Usage::default())
    }

    /// Continue from usage recorded earlier.
    pub fn resume(budgets: Budgets, usage: Usage) -> BudgetTracker {
        let cost_known = usage.cost_usd.is_some() || (usage.input_tokens == 0 && usage.output_tokens == 0);
        let elapsed_before = Duration::from_secs(usage.elapsed_s);
        BudgetTracker(Mutex::new(Inner {
            budgets,
            usage,
            cost_known,
            elapsed_before,
            started: Instant::now(),
        }))
    }

    pub fn set_budgets(&self, budgets: Budgets) {
        self.0.lock().unwrap().budgets = budgets;
    }

    pub fn budgets(&self) -> Budgets {
        self.0.lock().unwrap().budgets.clone()
    }

    pub fn add_step(&self) {
        self.0.lock().unwrap().usage.steps += 1;
    }

    /// Record one model turn. `price` is the price of the model that served it, if configured.
    pub fn add_tokens(&self, input: u64, output: u64, estimated: bool, price: Option<&Price>) {
        let mut i = self.0.lock().unwrap();
        i.usage.input_tokens += input;
        i.usage.output_tokens += output;
        i.usage.estimated |= estimated;
        match price {
            Some(p) if i.cost_known => {
                let add = (input as f64 * p.input + output as f64 * p.output) / 1_000_000.0;
                i.usage.cost_usd = Some(i.usage.cost_usd.unwrap_or(0.0) + add);
            }
            _ => {
                i.cost_known = false;
                i.usage.cost_usd = None;
            }
        }
    }

    pub fn usage(&self) -> Usage {
        let i = self.0.lock().unwrap();
        let mut u = i.usage.clone();
        u.elapsed_s = (i.elapsed_before + i.started.elapsed()).as_secs();
        u
    }

    /// The first limit that is used up, if any. Checked before each step.
    pub fn exhausted(&self) -> Option<Limit> {
        let u = self.usage();
        let b = self.budgets();
        if u.steps >= b.max_steps {
            Some(Limit::Steps)
        } else if u.input_tokens + u.output_tokens >= b.max_tokens {
            Some(Limit::Tokens)
        } else if matches!((b.max_cost_usd, u.cost_usd), (Some(max), Some(cost)) if cost >= max) {
            Some(Limit::Cost)
        } else if b.max_wall_clock_s.is_some_and(|max| u.elapsed_s >= max) {
            Some(Limit::WallClock)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(b: Budgets) -> BudgetTracker {
        BudgetTracker::new(b)
    }

    #[test]
    fn defaults_match_the_brief() {
        let b = Budgets::default();
        assert_eq!(
            (b.max_steps, b.max_tokens, b.max_cost_usd, b.max_wall_clock_s),
            (50, 2_000_000, None, Some(7200))
        );
        assert_eq!(
            serde_json::from_str::<Budgets>(r#"{"max_steps": 3}"#).unwrap().max_tokens,
            2_000_000
        );
    }

    #[test]
    fn step_and_token_limits_trip_exactly_at_the_limit() {
        let t = tracker(Budgets {
            max_steps: 2,
            max_tokens: 100,
            ..Budgets::default()
        });
        assert_eq!(t.exhausted(), None);
        t.add_step();
        t.add_tokens(60, 39, false, None);
        assert_eq!(t.exhausted(), None);
        t.add_tokens(0, 1, false, None);
        assert_eq!(t.exhausted(), Some(Limit::Tokens));
        let t = tracker(Budgets {
            max_steps: 2,
            ..Budgets::default()
        });
        t.add_step();
        t.add_step();
        assert_eq!(t.exhausted(), Some(Limit::Steps));
    }

    #[test]
    fn cost_adds_up_with_prices_and_is_unknown_without_one() {
        let price = Price { input: 2.0, output: 10.0 };
        let t = tracker(Budgets {
            max_cost_usd: Some(0.02),
            ..Budgets::default()
        });
        t.add_tokens(1000, 1000, false, Some(&price));
        assert!((t.usage().cost_usd.unwrap() - 0.012).abs() < 1e-12);
        assert_eq!(t.exhausted(), None);
        t.add_tokens(1000, 1000, false, Some(&price));
        assert_eq!(t.exhausted(), Some(Limit::Cost));

        // One turn on an unpriced model makes the total unknown; the cost limit can no longer trip.
        let t = tracker(Budgets {
            max_cost_usd: Some(0.000001),
            ..Budgets::default()
        });
        t.add_tokens(1000, 1000, false, Some(&price));
        t.add_tokens(1000, 1000, true, None);
        t.add_tokens(1000, 1000, false, Some(&price));
        let u = t.usage();
        assert_eq!((u.cost_usd, u.estimated, u.input_tokens), (None, true, 3000));
        assert_eq!(t.exhausted(), None);
    }

    #[test]
    fn wall_clock_counts_time_from_before_a_resume_and_raising_a_budget_clears_it() {
        let t = BudgetTracker::resume(
            Budgets {
                max_wall_clock_s: Some(100),
                ..Budgets::default()
            },
            Usage {
                elapsed_s: 100,
                ..Usage::default()
            },
        );
        assert_eq!(t.exhausted(), Some(Limit::WallClock));
        t.set_budgets(Budgets {
            max_wall_clock_s: Some(500),
            ..Budgets::default()
        });
        assert_eq!(t.exhausted(), None);
        t.set_budgets(Budgets {
            max_wall_clock_s: None,
            ..Budgets::default()
        });
        assert_eq!(t.exhausted(), None);
    }
}
