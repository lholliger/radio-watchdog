use std::collections::HashMap;
use std::sync::Arc;
use chrono::{DateTime, Duration, Utc};
use tokio::sync::RwLock;
use tracing::{info, warn, error};
use super::slack::SlackMessageSender;

#[derive(Debug, Clone, PartialEq)]
pub enum AlertState {
    NewFailing,              // First time alert needed
    FailingAlertSent,        // Alert sent, still failing
    FailingReminderNeeded,   // Still failing, reminder needed (every 10 min)
    NewPassing,              // Was failing, now passing - send cleared message
    Passing,                 // Everything OK
}

#[derive(Debug, Clone, PartialEq)]
enum PendingAggregation {
    None,
    NewFailure,
    Cleared,
    Reminder,
}

#[derive(Debug, Clone)]
pub struct Alert {
    pub name: String,
    pub message: String,
    failing_since: Option<DateTime<Utc>>,
    last_sent_update: Option<DateTime<Utc>>,
    pending_aggregation: PendingAggregation,
}

impl Alert {
    pub fn new(name: String, message: String) -> Self {
        Alert {
            name,
            message,
            failing_since: None,
            last_sent_update: None,
            pending_aggregation: PendingAggregation::None,
        }
    }

    pub fn mark_failing(&mut self, message: String) {
        self.message = message;
        if self.failing_since.is_none() {
            self.failing_since = Some(Utc::now());
        }
    }

    pub fn mark_passing(&mut self) {
        self.failing_since = None;
    }

    pub fn is_failing(&self) -> bool {
        self.failing_since.is_some()
    }

    pub fn alert_state(&self) -> AlertState {
        let reminder_interval = Duration::minutes(10);
        let now = Utc::now();

        match (self.failing_since, self.last_sent_update) {
            (Some(_), None) => AlertState::NewFailing,
            (Some(_), Some(last_sent)) if now - last_sent >= reminder_interval => {
                AlertState::FailingReminderNeeded
            }
            (Some(_), Some(_)) => AlertState::FailingAlertSent,
            (None, Some(_)) => AlertState::NewPassing,
            (None, None) => AlertState::Passing,
        }
    }

    pub fn register_sent(&mut self) {
        match self.alert_state() {
            AlertState::NewFailing | AlertState::FailingReminderNeeded => {
                self.last_sent_update = Some(Utc::now());
            }
            AlertState::NewPassing => {
                self.last_sent_update = None;
            }
            _ => {}
        }
    }
}

pub struct AlertManager {
    alerts: Arc<RwLock<HashMap<String, Alert>>>,
    slack: Arc<SlackMessageSender>,
    reminder_interval_minutes: i64,
    grace_period_seconds: i64,
}

impl AlertManager {
    pub fn new(slack: Arc<SlackMessageSender>, reminder_interval_minutes: i64, grace_period_seconds: i64) -> Self {
        AlertManager {
            alerts: Arc::new(RwLock::new(HashMap::new())),
            slack,
            reminder_interval_minutes,
            grace_period_seconds,
        }
    }

    pub async fn update_alert(&self, alert_id: String, is_error: bool, message: String) {
        let mut alerts = self.alerts.write().await;
        let alert = alerts.entry(alert_id.clone()).or_insert_with(|| {
            Alert::new(alert_id.clone(), message.clone())
        });

        let previous_state = alert.alert_state();

        if is_error {
            alert.mark_failing(message.clone());
        } else {
            alert.mark_passing();
        }

        let new_state = alert.alert_state();

        // Mark alerts for aggregation instead of sending immediately
        match new_state {
            AlertState::NewFailing if previous_state != AlertState::NewFailing => {
                warn!("New alert (in grace period): {}", message);
            }
            AlertState::NewPassing if previous_state != AlertState::NewPassing => {
                info!("Alert cleared: {}", alert_id);
                alert.pending_aggregation = PendingAggregation::Cleared;
                alert.register_sent();
            }
            _ => {}
        }
    }

    pub async fn process_alerts(&self) {
        let mut alerts = self.alerts.write().await;

        for (_alert_id, alert) in alerts.iter_mut() {
            match alert.alert_state() {
                AlertState::FailingReminderNeeded => {
                    warn!("Alert reminder: {}", alert.message);
                    alert.pending_aggregation = PendingAggregation::Reminder;
                    alert.register_sent();
                }
                _ => {
                    // NewFailing and NewPassing are handled in update_alert()
                    // FailingAlertSent and Passing don't need action
                }
            }
        }
    }

    async fn process_aggregated_alerts(&self) {
        let mut alerts = self.alerts.write().await;
        let now = Utc::now();
        let grace_period = Duration::seconds(self.grace_period_seconds);

        // Collect alerts by pending state
        let mut new_failures = Vec::new();
        let mut clears = Vec::new();
        let mut reminders = Vec::new();

        for alert in alerts.values_mut() {
            match alert.pending_aggregation {
                PendingAggregation::NewFailure => {
                    new_failures.push(alert.message.clone());
                    alert.pending_aggregation = PendingAggregation::None;
                }
                PendingAggregation::Cleared => {
                    clears.push(alert.message.clone());
                    alert.pending_aggregation = PendingAggregation::None;
                }
                PendingAggregation::Reminder => {
                    reminders.push(alert.message.clone());
                    alert.pending_aggregation = PendingAggregation::None;
                }
                PendingAggregation::None => {
                    // Check if this is a new failure that has passed the grace period
                    if let AlertState::NewFailing = alert.alert_state() {
                        if let Some(failing_since) = alert.failing_since {
                            if now - failing_since >= grace_period {
                                error!("Alert passed grace period: {}", alert.message);
                                new_failures.push(alert.message.clone());
                                alert.pending_aggregation = PendingAggregation::None;
                                alert.register_sent();
                            }
                        }
                    }
                }
            }
        }

        // Release the lock before sending messages
        drop(alerts);

        // Send aggregated messages
        if !new_failures.is_empty() {
            let message = if new_failures.len() == 1 {
                format!("*Warning:* _A new issue has been detected!_\n{}", new_failures[0])
            } else {
                let issues = new_failures.iter()
                    .enumerate()
                    .map(|(i, msg)| format!("{}. {}", i + 1, msg))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("*Warning:* _{} new issues detected!_\n{}", new_failures.len(), issues)
            };
            self.slack.send(message).await;
        }

        if !clears.is_empty() {
            let message = if clears.len() == 1 {
                format!("*Success:* _Issue resolved!_\n{}", clears[0])
            } else {
                let issues = clears.iter()
                    .enumerate()
                    .map(|(i, msg)| format!("{}. {}", i + 1, msg))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("*Success:* _{} issues resolved!_\n{}", clears.len(), issues)
            };
            self.slack.send(message).await;
        }

        if !reminders.is_empty() {
            let message = if reminders.len() == 1 {
                format!("*Reminder:* _Issue is still present!_\n{}", reminders[0])
            } else {
                let issues = reminders.iter()
                    .enumerate()
                    .map(|(i, msg)| format!("{}. {}", i + 1, msg))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("*Reminder:* _{} issues still present!_\n{}", reminders.len(), issues)
            };
            self.slack.send(message).await;
        }
    }

    pub async fn start_alert_loop(self: Arc<Self>) {
        info!("Starting alert manager with {}min reminder interval, 30s aggregation window, and {}s grace period",
              self.reminder_interval_minutes, self.grace_period_seconds);

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;

                // Check for reminders and mark them as pending
                self.process_alerts().await;

                // Send all pending aggregated alerts
                self.process_aggregated_alerts().await;
            }
        });
    }
}
