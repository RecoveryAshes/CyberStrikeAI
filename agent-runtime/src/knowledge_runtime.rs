use serde_json::{json, Map, Value};
use thiserror::Error;

#[derive(Debug, Default, Clone)]
pub struct KnowledgeRuntime {
    snippets: Vec<KnowledgeSnippet>,
}

#[derive(Debug, Clone)]
pub struct KnowledgeSnippet {
    pub id: String,
    pub title: String,
    pub category: String,
    pub content: String,
    pub score: Option<f64>,
}

#[derive(Debug, Error)]
pub enum KnowledgeError {
    #[error("invalid knowledge_search arguments: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("knowledge_search query is required")]
    MissingQuery,
}

impl KnowledgeRuntime {
    pub fn from_context(context: &Map<String, Value>) -> Self {
        let enabled = context
            .get("knowledge_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !enabled {
            return Self::default();
        }
        let snippets = context
            .get("knowledge_snippets")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_snippet).collect())
            .unwrap_or_default();
        Self { snippets }
    }

    pub fn context_snippets(&self) -> Vec<String> {
        self.snippets
            .iter()
            .take(5)
            .map(KnowledgeSnippet::to_prompt_snippet)
            .collect()
    }

    pub fn execute_call(&self, arguments: &str) -> Result<String, KnowledgeError> {
        let args: Value = serde_json::from_str(arguments)?;
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or(KnowledgeError::MissingQuery)?;
        let q = query.to_lowercase();
        let limit = args.get("top_k").and_then(Value::as_u64).unwrap_or(5) as usize;
        let mut matches: Vec<&KnowledgeSnippet> = self
            .snippets
            .iter()
            .filter(|snippet| snippet.matches(&q))
            .collect();
        if matches.is_empty() {
            matches = self.snippets.iter().collect();
        }
        matches.truncate(limit.max(1));
        Ok(json!({
            "tool": "knowledge_search",
            "query": query,
            "results": matches.into_iter().map(KnowledgeSnippet::to_json).collect::<Vec<_>>()
        })
        .to_string())
    }
}

impl KnowledgeSnippet {
    fn matches(&self, q: &str) -> bool {
        self.title.to_lowercase().contains(q)
            || self.category.to_lowercase().contains(q)
            || self.content.to_lowercase().contains(q)
    }

    fn to_prompt_snippet(&self) -> String {
        format!(
            "[{}] {} / {}: {}",
            self.id,
            self.category,
            self.title,
            truncate(&self.content, 700)
        )
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "title": self.title,
            "category": self.category,
            "content": self.content,
            "score": self.score
        })
    }
}

fn parse_snippet(value: &Value) -> Option<KnowledgeSnippet> {
    let obj = value.as_object()?;
    Some(KnowledgeSnippet {
        id: obj
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: obj
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        category: obj
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        content: obj
            .get("content")
            .or_else(|| obj.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        score: obj.get("score").and_then(Value::as_f64),
    })
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Map};

    #[test]
    fn loads_snippets_and_searches_by_query() {
        let mut context = Map::new();
        context.insert("knowledge_enabled".to_string(), Value::Bool(true));
        context.insert(
            "knowledge_snippets".to_string(),
            json!([
                {"id": "k1", "title": "SQL Injection", "category": "web", "content": "union select payloads", "score": 0.91},
                {"id": "k2", "title": "XSS", "category": "web", "content": "script sink", "score": 0.82}
            ]),
        );
        let runtime = KnowledgeRuntime::from_context(&context);
        assert_eq!(runtime.context_snippets().len(), 2);

        let result = runtime
            .execute_call(r#"{"query":"union","top_k":1}"#)
            .unwrap();
        assert!(result.contains("\"id\":\"k1\""));
        assert!(!result.contains("\"id\":\"k2\""));
    }
}
