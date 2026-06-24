use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event_protocol::PlanEventItem;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl PlanStatus {
    pub fn from_str(status: &str) -> Option<Self> {
        match status.trim().to_lowercase().as_str() {
            "pending" => Some(PlanStatus::Pending),
            "in_progress" | "in-progress" | "inprogress" | "running" => {
                Some(PlanStatus::InProgress)
            }
            "completed" | "complete" | "done" => Some(PlanStatus::Completed),
            "cancelled" | "canceled" => Some(PlanStatus::Cancelled),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PlanStatus::Pending => "pending",
            PlanStatus::InProgress => "in_progress",
            PlanStatus::Completed => "completed",
            PlanStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanItem {
    pub id: String,
    pub step: String,
    pub status: PlanStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct PlanStore {
    items: Vec<PlanItem>,
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("only one in_progress plan item is allowed")]
    MultipleInProgress,
    #[error("plan item step cannot be empty")]
    EmptyStep,
}

impl PlanStore {
    pub fn update(&mut self, items: Vec<PlanItem>) -> Result<(), PlanError> {
        if items
            .iter()
            .filter(|item| item.status == PlanStatus::InProgress)
            .count()
            > 1
        {
            return Err(PlanError::MultipleInProgress);
        }
        if items.iter().any(|item| item.step.trim().is_empty()) {
            return Err(PlanError::EmptyStep);
        }
        self.items = items;
        Ok(())
    }

    pub fn has_active_work(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item.status, PlanStatus::Pending | PlanStatus::InProgress))
    }

    pub fn event_items(&self) -> Vec<PlanEventItem> {
        self.items
            .iter()
            .map(|item| PlanEventItem {
                id: item.id.clone(),
                step: item.step.clone(),
                status: item.status.as_str().to_string(),
                priority: item.priority.clone(),
            })
            .collect()
    }

    pub fn items(&self) -> Vec<PlanItem> {
        self.items.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_multiple_in_progress_items() {
        let mut store = PlanStore::default();
        let err = store
            .update(vec![
                PlanItem {
                    id: "1".to_string(),
                    step: "one".to_string(),
                    status: PlanStatus::InProgress,
                    priority: None,
                },
                PlanItem {
                    id: "2".to_string(),
                    step: "two".to_string(),
                    status: PlanStatus::InProgress,
                    priority: None,
                },
            ])
            .unwrap_err();
        assert!(matches!(err, PlanError::MultipleInProgress));
    }

    #[test]
    fn active_work_tracks_pending_or_in_progress() {
        let mut store = PlanStore::default();
        store
            .update(vec![PlanItem {
                id: "1".to_string(),
                step: "done".to_string(),
                status: PlanStatus::Completed,
                priority: None,
            }])
            .unwrap();
        assert!(!store.has_active_work());

        store
            .update(vec![PlanItem {
                id: "2".to_string(),
                step: "next".to_string(),
                status: PlanStatus::Pending,
                priority: None,
            }])
            .unwrap();
        assert!(store.has_active_work());
    }
}
