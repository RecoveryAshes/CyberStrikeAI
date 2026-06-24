use serde_json::{json, Value};
use thiserror::Error;

use crate::model_stream::ModelToolCall;
use crate::plan_store::{PlanItem, PlanStatus, PlanStore};

#[derive(Debug, Default)]
pub struct ToolRuntime;

#[derive(Debug)]
pub enum ToolOutcome {
    PlanUpdated(String),
    Text(String),
    FailedText(String),
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid JSON arguments for {tool_name}: {source}")]
    InvalidArguments {
        tool_name: String,
        source: serde_json::Error,
    },
    #[error("unsupported runtime tool: {0}")]
    UnsupportedTool(String),
    #[error("invalid plan item at index {index}: {reason}")]
    InvalidPlanItem { index: usize, reason: String },
    #[error("plan update rejected: {0}")]
    PlanRejected(#[from] crate::plan_store::PlanError),
    #[error("skill tool failed: {0}")]
    Skill(String),
    #[error("mcp tool failed: {0}")]
    Mcp(String),
    #[error("knowledge tool failed: {0}")]
    Knowledge(String),
    #[error("filesystem tool failed: {0}")]
    Filesystem(String),
}

impl ToolRuntime {
    pub fn execute(
        &self,
        call: &ModelToolCall,
        plan: &mut PlanStore,
    ) -> Result<ToolOutcome, ToolError> {
        let tool_name = call.function.name.trim();
        let args: Value = serde_json::from_str(&call.function.arguments).map_err(|source| {
            ToolError::InvalidArguments {
                tool_name: tool_name.to_string(),
                source,
            }
        })?;

        match tool_name {
            "update_plan" => self.update_plan(tool_name, &args, plan),
            "todowrite" => self.update_plan(tool_name, &args, plan),
            "runtime_echo" => Ok(ToolOutcome::Text(format!(
                "runtime_echo: {}",
                args.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
            ))),
            other => Err(ToolError::UnsupportedTool(other.to_string())),
        }
    }

    fn update_plan(
        &self,
        tool_name: &str,
        args: &Value,
        plan: &mut PlanStore,
    ) -> Result<ToolOutcome, ToolError> {
        let raw_items = args
            .get("items")
            .or_else(|| args.get("plan"))
            .or_else(|| args.get("todos"))
            .and_then(Value::as_array)
            .ok_or_else(|| ToolError::InvalidPlanItem {
                index: 0,
                reason: "expected items, plan, or todos array".to_string(),
            })?;

        let mut items = Vec::with_capacity(raw_items.len());
        for (index, raw) in raw_items.iter().enumerate() {
            items.push(parse_plan_item(index, raw)?);
        }
        plan.update(items)?;
        Ok(ToolOutcome::PlanUpdated(
            json!({
                "tool": tool_name,
                "items": plan.event_items()
            })
            .to_string(),
        ))
    }
}

fn parse_plan_item(index: usize, raw: &Value) -> Result<PlanItem, ToolError> {
    let status = raw
        .get("status")
        .and_then(Value::as_str)
        .and_then(PlanStatus::from_str)
        .ok_or_else(|| ToolError::InvalidPlanItem {
            index,
            reason: "missing or invalid status".to_string(),
        })?;
    let step = raw
        .get("step")
        .or_else(|| raw.get("content"))
        .or_else(|| raw.get("title"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if step.is_empty() {
        return Err(ToolError::InvalidPlanItem {
            index,
            reason: "step/content cannot be empty".to_string(),
        });
    }

    let id = raw
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("item_{}", index + 1));
    let priority = raw
        .get("priority")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|priority| !priority.is_empty())
        .map(|priority| priority.to_lowercase())
        .map(|priority| match priority.as_str() {
            "high" | "medium" | "low" => Ok(priority),
            _ => Err(ToolError::InvalidPlanItem {
                index,
                reason: "priority must be high, medium, or low".to_string(),
            }),
        })
        .transpose()?;

    Ok(PlanItem {
        id,
        step,
        status,
        priority,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_stream::{ModelToolCall, ModelToolFunction};

    #[test]
    fn update_plan_tool_updates_store() {
        let call = ModelToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: ModelToolFunction {
                name: "update_plan".to_string(),
                arguments: json!({
                    "items": [
                        {"id": "a", "step": "A", "status": "completed"},
                        {"id": "b", "step": "B", "status": "in_progress", "priority": "high"}
                    ]
                })
                .to_string(),
            },
        };
        let mut plan = PlanStore::default();
        let outcome = ToolRuntime::default().execute(&call, &mut plan).unwrap();
        assert!(matches!(outcome, ToolOutcome::PlanUpdated(_)));
        assert!(plan.has_active_work());
        assert_eq!(plan.items()[1].priority.as_deref(), Some("high"));
    }
}
