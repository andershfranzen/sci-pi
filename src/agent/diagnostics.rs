//! Allowlisted request shape only: never retain payloads, authorization, or error bodies.

use super::llm::{Delta, Request};
use serde_json::{Value, json};

pub(super) fn endpoint(url: &str) -> Option<String> {
    let mut url = reqwest::Url::parse(url).ok()?;
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

fn label(value: &str, secret: Option<&str>) -> String {
    if secret.filter(|key| !key.is_empty()).is_some_and(|key| value.contains(key))
        || ["sk-ant-", "sk-proj-", "github_pat_", "ghp_", "Bearer "].iter().any(|prefix| value.contains(prefix))
    {
        return "[redacted]".into();
    }
    value.chars().filter(|c| !c.is_control()).take(128).collect()
}

pub(super) struct Trace<'a> {
    route: &'static str,
    endpoint: String,
    model: &'a str,
    phase: &'static str,
    window: Option<u64>,
    server_compaction: bool,
    secret: Option<&'a str>,
    on: &'a mut (dyn FnMut(Delta) + Send),
}

impl<'a> Trace<'a> {
    pub fn new(req: &Request<'a>, route: &'static str, url: &str, secret: Option<&'a str>, on: &'a mut (dyn FnMut(Delta) + Send)) -> Self {
        let endpoint = endpoint(url).map(|mut value| {
            if secret.filter(|key| !key.is_empty()).is_some_and(|key| value.contains(key)) {
                "[redacted]".into()
            } else {
                value.truncate(512);
                value
            }
        }).unwrap_or_else(|| "[invalid endpoint]".into());
        Self { route, endpoint, model: req.model, phase: req.phase, window: req.opts.window,
            server_compaction: req.opts.server_compaction, secret, on }
    }

    pub fn start(&mut self, body: &Value, attempt: usize, rejected: &[&str]) -> Value {
        let explicit = body["tools"].as_array().into_iter().flatten()
            .chain(body["system"].as_array().into_iter().flatten())
            .chain(body["messages"].as_array().into_iter().flatten()
                .flat_map(|message| message["content"].as_array().into_iter().flatten()))
            .filter(|block| block.get("cache_control").is_some()).count();
        let automatic = body.get("cache_control").is_some();
        let effort = body["output_config"]["effort"].as_str().or_else(|| body["reasoning_effort"].as_str());
        let fast = body["speed"].as_str().or_else(|| body["service_tier"].as_str());
        let record = json!({
            "id": uuid::Uuid::new_v4().to_string(), "at_ms": crate::config::now_ms(),
            "route": self.route, "endpoint": self.endpoint, "model": label(self.model, self.secret),
            "phase": self.phase, "attempt": attempt, "state": "pending", "http_status": null, "request_id": null,
            "effort": effort.map(|s| label(s, self.secret)), "fast": fast.map(|s| label(s, self.secret)),
            "thinking": body.get("thinking").is_some(), "server_compaction": self.server_compaction,
            "compaction": body.get("compaction").is_some(),
            "cache_owner": if self.route == "anthropic_proxy" { "proxy" } else if automatic || explicit > 0 { "client" } else { "none" },
            "explicit_cache_points": explicit, "automatic_cache": automatic,
            "context_window": self.window, "max_output": body["max_tokens"].as_u64(), "rejected_fields": rejected,
        });
        (self.on)(Delta::Diagnostic(record.clone()));
        record
    }

    pub fn finish(&mut self, mut record: Value, response: Option<&reqwest::Response>) {
        record["state"] = json!(if response.is_some() { "response" } else { "network_error" });
        if let Some(response) = response {
            record["http_status"] = json!(response.status().as_u16());
            let id = ["request-id", "x-request-id"].iter().find_map(|name| response.headers().get(*name))
                .and_then(|id| id.to_str().ok())
                .filter(|id| id.len() <= 128 && id.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)))
                .map(|id| label(id, self.secret));
            record["request_id"] = json!(id);
        }
        (self.on)(Delta::Diagnostic(record));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{llm::Opts, models::ModelInfo};

    #[test]
    fn diagnostics_do_not_retain_payloads_or_credentials() {
        let info = ModelInfo { window: Some(8192), max_output: Some(1024), ..Default::default() };
        let opts = Opts::from_info(&info, "", false);
        let req = Request { model: "runtime-model", system: "private system instructions", messages: &[], tools: &[],
            opts: &opts, session_id: "private-session", phase: "inference" };
        let mut records = vec![];
        let mut on = |delta| if let Delta::Diagnostic(record) = delta { records.push(record); };
        let mut trace = Trace::new(&req, "anthropic_key", "https://user:secret@example.test/v1/messages?api_key=private-key#secret", Some("private-key"), &mut on);
        trace.start(&json!({ "max_tokens": 1024, "system": [{ "type": "text", "text": "private system instructions" }],
            "messages": [{ "content": [{ "type": "text", "text": "private prompt" },
                { "type": "thinking", "thinking": "private thoughts", "signature": "private signature" }] }],
            "output_config": { "effort": "private-key" }, "cache_control": { "type": "ephemeral" } }), 1, &[]);
        drop(trace);
        let serialized = serde_json::to_string(&records).unwrap();
        for secret in ["private-key", "private system instructions", "private prompt", "private thoughts", "private signature", "private-session", "user:secret", "api_key="] {
            assert!(!serialized.contains(secret), "diagnostic leaked {secret}");
        }
        assert_eq!(records[0]["endpoint"], "https://example.test/v1/messages");
        assert_eq!(records[0]["effort"], "[redacted]");
        assert_eq!(records[0]["context_window"], 8192);
    }
}
