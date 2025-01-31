use chrono::{DateTime, Duration, Utc};
use tracing::{trace, warn};

// this function will be rather internal and likely cannot be reused in many places

pub enum AlertState {
    NewFailing, // alert needed
    FailingAlertSent,
    FailingReminderNeeded,
    NewPassing, // alert needed
    Passing
}

#[derive(Debug, Clone)]
pub struct NominalValue {
    name: String,
    current_value: f32,
    acceptable_range: (f32, f32),
    failing: Option<DateTime<Utc>>, // once outside of range, a message will be sent and will not be re-sent
    last_sent_update: Option<DateTime<Utc>> // keeps us from sending an alert for every single updated value
}

impl NominalValue {
    pub fn new(name: String, acceptable_range: (f32, f32)) -> Self {
        NominalValue { name, current_value: 0.0, acceptable_range, failing: None, last_sent_update: None }
    }

    pub fn update(&mut self, new_value: f32) {
        if new_value < self.acceptable_range.0 || new_value > self.acceptable_range.1 {
            self.failing = Some(Utc::now());
            warn!("Value is outside acceptable range: {}", self.get_alert_string());
        } else {
            self.current_value = new_value;
            self.failing = None;
            trace!("Value is within acceptable range: {}", self.get_passing_string());
        }
        self.current_value = new_value;
    }

    pub fn get_alert_string(&self) -> String {
        let mut adj_string = format!("(Acceptable range `{:.2}`-`{:.2}`)", self.acceptable_range.0, self.acceptable_range.1);
        if self.acceptable_range.0 == f32::NEG_INFINITY {
            adj_string = format!("(Value must be under `{:.2}`)", self.acceptable_range.1)
        } else if self.acceptable_range.1 == f32::INFINITY {
            adj_string = format!("(Value must be over `{:.2}`)", self.acceptable_range.0)
        }
        format!("{} is currently `{:.02}` {}", self.name, self.current_value, adj_string)
    }

    pub fn get_passing_string(&self) -> String {
        format!("{} is currently `{:.02}`", self.name, self.current_value)
    }

    pub fn alert_needed(&self) -> AlertState {
        let acceptable_time = Utc::now() - Duration::minutes(10);
        if self.failing.is_some() && self.last_sent_update.is_none() { // we have a new failure
            AlertState::NewFailing
        } else if self.failing.is_some() && self.last_sent_update.is_some() && self.last_sent_update.unwrap() > acceptable_time { // we have a new failure, but a recent alert
            AlertState::FailingAlertSent
        } else if self.failing.is_some() && self.last_sent_update.is_some() && self.last_sent_update.unwrap() <= acceptable_time { // we have a current failure, but no recent alert
            AlertState::FailingReminderNeeded
        } else if self.failing.is_none() && self.last_sent_update.is_some() { // it is now passing, we need to notify it is now passing
            AlertState::NewPassing
        } else { // everything should be good now!
            AlertState::Passing
        }
    }

    pub fn register_sent_alert(&mut self) {
        match self.alert_needed() {
            
            AlertState::NewFailing | AlertState::FailingAlertSent | AlertState::FailingReminderNeeded => {
                self.last_sent_update = Some(Utc::now());
            },
            AlertState::NewPassing => {
                self.last_sent_update = None
            }
            AlertState::Passing => {
                warn!("An alert was sent when it should'n't have been sent. This is a bug outside of this struct.")
            }
        }
    }
}