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

#[derive(Debug, Clone)]
pub struct Alert {
    pub name: String,
    pub message: String,
    failing_since: Option<DateTime<Utc>>,
    last_sent_update: Option<DateTime<Utc>>,
}

impl Alert {
    pub fn new(name: String, message: String) -> Self {
        Alert {
            name,
            message,
            failing_since: None,
            last_sent_update: None,
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
}

impl AlertManager {
    pub fn new(slack: Arc<SlackMessageSender>, reminder_interval_minutes: i64) -> Self {
        AlertManager {
            alerts: Arc::new(RwLock::new(HashMap::new())),
            slack,
            reminder_interval_minutes,
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

        // Immediately send alert if state changed to NewFailing or NewPassing
        match new_state {
            AlertState::NewFailing if previous_state != AlertState::NewFailing => {
                error!("New alert: {}", message);
                self.slack.send(format!("*Warning:* _A new issue has been detected!_ {}", message)).await;
                alert.register_sent();
            }
            AlertState::NewPassing if previous_state != AlertState::NewPassing => {
                info!("Alert cleared: {}", alert_id);
                self.slack.send(format!("*Success:* *Issue resolved!* {}", message)).await;
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
                    self.slack.send(format!("*Reminder:* _Issue is still present!_ {}", alert.message)).await;
                    alert.register_sent();
                }
                _ => {
                    // NewFailing and NewPassing are handled immediately in update_alert()
                    // FailingAlertSent and Passing don't need action
                }
            }
        }
    }

    pub async fn start_alert_loop(self: Arc<Self>) {
        info!("Starting alert manager with {}min reminder interval", self.reminder_interval_minutes);

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                self.process_alerts().await;
            }
        });
    }
}
