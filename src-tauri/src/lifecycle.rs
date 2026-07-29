use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum LaunchBudget {
    Time { minutes: u16 },
    Cost { eur: f64 },
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Manual,
    TimeBudget,
    CostBudget,
    IdleTimeout,
    RemoteStopped,
}

impl std::fmt::Display for StopReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Manual => "manual",
            Self::TimeBudget => "time budget",
            Self::CostBudget => "cost budget",
            Self::IdleTimeout => "idle timeout",
            Self::RemoteStopped => "remote stop",
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTelemetry {
    pub elapsed_seconds: u64,
    pub accrued_cost_eur: f64,
    pub cost_per_hr_eur: f64,
    pub budget_kind: &'static str,
    pub budget_remaining_seconds: Option<u64>,
    pub budget_remaining_eur: Option<f64>,
    pub idle_remaining_seconds: u64,
}

#[derive(Debug, Error)]
pub enum BudgetError {
    #[error("time budget must be between 5 and 720 minutes")]
    Time,
    #[error("cost budget must be between €0.10 and €100.00")]
    Cost,
    #[error("idle timeout must be between 1 and 240 minutes")]
    IdleTimeout,
    #[error("cost rate must be a positive finite number")]
    CostRate,
}

pub struct BudgetTracker {
    budget: LaunchBudget,
    idle_timeout_ms: u64,
    started_at_epoch_ms: u64,
    last_tick_epoch_ms: u64,
    cost_per_hr_eur: f64,
    accrued_cost_eur: f64,
}

impl LaunchBudget {
    pub fn validate(self) -> Result<Self, BudgetError> {
        match self {
            Self::Time { minutes } if (5..=720).contains(&minutes) => Ok(self),
            Self::Time { .. } => Err(BudgetError::Time),
            Self::Cost { eur } if eur.is_finite() && (0.1..=100.0).contains(&eur) => Ok(self),
            Self::Cost { .. } => Err(BudgetError::Cost),
        }
    }
}

impl BudgetTracker {
    pub fn new(
        budget: LaunchBudget,
        idle_timeout_minutes: u16,
        started_at_epoch_ms: u64,
        cost_per_hr_eur: f64,
        now_epoch_ms: u64,
    ) -> Result<Self, BudgetError> {
        let budget = budget.validate()?;
        if !(1..=240).contains(&idle_timeout_minutes) {
            return Err(BudgetError::IdleTimeout);
        }
        if !cost_per_hr_eur.is_finite() || cost_per_hr_eur <= 0.0 {
            return Err(BudgetError::CostRate);
        }
        let elapsed_ms = now_epoch_ms.saturating_sub(started_at_epoch_ms);
        Ok(Self {
            budget,
            idle_timeout_ms: u64::from(idle_timeout_minutes) * 60_000,
            started_at_epoch_ms,
            last_tick_epoch_ms: now_epoch_ms,
            cost_per_hr_eur,
            accrued_cost_eur: elapsed_ms as f64 / 3_600_000.0 * cost_per_hr_eur,
        })
    }

    pub fn update_cost_per_hr(&mut self, cost_per_hr_eur: f64) -> Result<(), BudgetError> {
        if !cost_per_hr_eur.is_finite() || cost_per_hr_eur <= 0.0 {
            return Err(BudgetError::CostRate);
        }
        self.cost_per_hr_eur = cost_per_hr_eur;
        Ok(())
    }

    pub fn tick(
        &mut self,
        now_epoch_ms: u64,
        last_request_epoch_ms: u64,
    ) -> (SessionTelemetry, Option<StopReason>) {
        let delta_ms = now_epoch_ms.saturating_sub(self.last_tick_epoch_ms);
        self.accrued_cost_eur += delta_ms as f64 / 3_600_000.0 * self.cost_per_hr_eur;
        self.last_tick_epoch_ms = now_epoch_ms;

        let elapsed_ms = now_epoch_ms.saturating_sub(self.started_at_epoch_ms);
        let idle_ms = now_epoch_ms.saturating_sub(last_request_epoch_ms);
        let idle_remaining_ms = self.idle_timeout_ms.saturating_sub(idle_ms);
        let (budget_kind, budget_remaining_seconds, budget_remaining_eur, budget_reason) =
            match self.budget {
                LaunchBudget::Time { minutes } => {
                    let limit_ms = u64::from(minutes) * 60_000;
                    (
                        "time",
                        Some(limit_ms.saturating_sub(elapsed_ms) / 1_000),
                        None,
                        (elapsed_ms >= limit_ms).then_some(StopReason::TimeBudget),
                    )
                }
                LaunchBudget::Cost { eur } => (
                    "cost",
                    None,
                    Some((eur - self.accrued_cost_eur).max(0.0)),
                    (self.accrued_cost_eur >= eur).then_some(StopReason::CostBudget),
                ),
            };
        let stop_reason = if idle_ms >= self.idle_timeout_ms {
            Some(StopReason::IdleTimeout)
        } else {
            budget_reason
        };

        (
            SessionTelemetry {
                elapsed_seconds: elapsed_ms / 1_000,
                accrued_cost_eur: self.accrued_cost_eur,
                cost_per_hr_eur: self.cost_per_hr_eur,
                budget_kind,
                budget_remaining_seconds,
                budget_remaining_eur,
                idle_remaining_seconds: idle_remaining_ms / 1_000,
            },
            stop_reason,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_budget_stops_at_the_deadline() {
        let mut tracker =
            BudgetTracker::new(LaunchBudget::Time { minutes: 5 }, 10, 1_000, 0.36, 1_000).unwrap();

        let (_, reason) = tracker.tick(301_000, 301_000);

        assert_eq!(reason, Some(StopReason::TimeBudget));
    }

    #[test]
    fn cost_budget_integrates_rate_changes() {
        let mut tracker =
            BudgetTracker::new(LaunchBudget::Cost { eur: 0.15 }, 10, 0, 0.60, 0).unwrap();
        let (_, first_reason) = tracker.tick(300_000, 300_000);
        tracker.update_cost_per_hr(1.20).unwrap();
        let (telemetry, second_reason) = tracker.tick(600_000, 600_000);

        assert_eq!(first_reason, None);
        assert_eq!(second_reason, Some(StopReason::CostBudget));
        assert!((telemetry.accrued_cost_eur - 0.15).abs() < 0.000_001);
    }

    #[test]
    fn idle_timeout_wins_over_remaining_budget() {
        let mut tracker =
            BudgetTracker::new(LaunchBudget::Time { minutes: 60 }, 10, 0, 0.30, 0).unwrap();

        let (telemetry, reason) = tracker.tick(600_000, 0);

        assert_eq!(telemetry.idle_remaining_seconds, 0);
        assert_eq!(reason, Some(StopReason::IdleTimeout));
    }
}
