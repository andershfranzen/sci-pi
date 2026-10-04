//! What each model can do – context window, output limit, effort levels, thinking, fast mode,
//! compaction, prices – learned at runtime from the providers and the models themselves.
//! Nothing here is hard-coded per model. Sources, most authoritative first:
//!
//! 1. Anthropic's Models API (`/v1/models` with capabilities), when there's an Anthropic key;
//! 2. the provider's own `/models` listing (OpenRouter, vLLM, … report context sizes);
//! 3. a CLIProxyAPI server's Codex-format catalog for every model it routes;
//! 4. the models.dev catalog (fetched, cached for a day);
//! 5. limits learned from requests a provider rejected as too long (persisted).

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fast {
    /// Anthropic `speed: "fast"` (beta header).
    AnthropicSpeed,
    /// OpenAI-style `service_tier` (e.g. "priority").
    ServiceTier { tier: String, description: Option<String> },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cost {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// Usable input context, in tokens.
    pub window: Option<u64>,
    pub max_output: Option<u64>,
    /// (value, description), in the model's order.
    pub efforts: Vec<(String, Option<String>)>,
    #[serde(default)]
    pub efforts_known: bool,
    pub default_effort: Option<String>,
    pub adaptive_thinking: Option<bool>,
    pub fast: Option<Fast>,
    #[serde(default)]
    pub fast_known: bool,
    #[serde(default)]
    pub thinking_display: Option<String>,
    #[serde(default)]
    pub fallbacks: Option<bool>,
    pub server_compaction: Option<bool>,
    pub reasoning: Option<bool>,
    /// USD per million tokens.
    pub cost: Option<Cost>,
}

impl ModelInfo {
    /// Fills whatever this one doesn't know yet from a less authoritative source.
    pub fn fill_from(&mut self, other: &ModelInfo) {
        macro_rules! fill {
            ($($f:ident),*) => { $( if self.$f.is_none() { self.$f = other.$f.clone(); } )* };
        }
        fill!(provider, name, description, window, max_output, adaptive_thinking, server_compaction, reasoning, thinking_display, fallbacks);
        if let Some(other_cost) = &other.cost {
            let c = self.cost.get_or_insert_with(Cost::default);
            c.input = c.input.or(other_cost.input);
            c.output = c.output.or(other_cost.output);
            c.cache_read = c.cache_read.or(other_cost.cache_read);
            c.cache_write = c.cache_write.or(other_cost.cache_write);
        }
        if !self.efforts_known && self.efforts.is_empty() {
            self.efforts = other.efforts.clone();
            self.efforts_known = other.efforts_known;
            if self.default_effort.is_none() {
                self.default_effort = other.default_effort.clone();
            }
        }
        if !self.fast_known && self.fast.is_none() {
            self.fast = other.fast.clone();
            self.fast_known = other.fast_known;
        }
    }

    /// One-line summary for model pickers, e.g. "272K context · reasoning · fast".
    pub fn summary(&self) -> Option<String> {
        let mut parts = vec![];
        if let Some(w) = self.window {
            parts.push(format!("{} context", tokens(w)));
        }
        if let Some(w) = self.max_output {
            parts.push(format!("{} output", tokens(w)));
        }
        if self.reasoning == Some(true) || !self.efforts.is_empty() {
            parts.push("reasoning".into());
        }
        if self.fast.is_some() {
            parts.push("fast mode".into());
        }
        if let Some(c) = &self.cost {
            match (c.input, c.output) {
                (Some(input), Some(output)) => parts.push(format!("${}/${} per Mtok", trim(input), trim(output))),
                (Some(input), None) => parts.push(format!("${} input per Mtok", trim(input))),
                (None, Some(output)) => parts.push(format!("${} output per Mtok", trim(output))),
                (None, None) => {}
            }
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

fn tokens(n: u64) -> String {
    if n >= 1_000_000 && n % 1_000_000 == 0 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6).trim_end_matches('0').trim_end_matches('.').to_string()
    } else if n >= 1000 && n % 1000 == 0 {
        format!("{}K", n / 1000)
    } else {
        n.to_string()
    }
}

fn trim(x: f64) -> String {
    let s = format!("{x:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Anthropic's Models API: everything about each model, straight from the source.
pub async fn anthropic_models(http: &reqwest::Client, base_url: &str, api_key: &str, proxy: bool) -> Result<Vec<(String, ModelInfo)>> {
    let url = format!("{}/v1/models", base_url.trim_end_matches('/'));
    let mut after: Option<String> = None;
    let mut models = Vec::new();
    loop {
        let mut req = http.get(&url).query(&[("limit", "1000")]).timeout(Duration::from_secs(10))
            .header("x-api-key", api_key).header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "compact-2026-09-04");
        if proxy {
            req = req.bearer_auth(api_key);
        }
        if let Some(cursor) = &after {
            req = req.query(&[("after_id", cursor)]);
        }
        let res = req.send().await?;
        if !res.status().is_success() {
            bail!("{url}: HTTP {}", res.status());
        }
        let v: Value = res.json().await?;
        models.extend(parse_openai_models(&v));
        if v["has_more"] != true {
            break;
        }
        let next = v["last_id"].as_str().filter(|id| !id.is_empty());
        if next.is_none() || next == after.as_deref() {
            bail!("{url}: pagination has no advancing last_id");
        }
        after = next.map(str::to_string);
    }
    Ok(models)
}

fn supported(v: &Value) -> Option<bool> {
    v.as_bool().or_else(|| v["supported"].as_bool())
}

fn positive(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| *n > 0)
}

fn prices(v: &Value, per_token: bool) -> Option<Cost> {
    let rate = |key: &str| {
        v[key].as_f64().or_else(|| v[key].as_str()?.parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|n| if per_token { n * 1_000_000.0 } else { n })
            .filter(|n| n.is_finite())
    };
    let c = Cost {
        input: rate("input").or_else(|| rate("prompt")),
        output: rate("output").or_else(|| rate("completion")),
        cache_read: rate("cache_read").or_else(|| rate("input_cache_read")),
        cache_write: rate("cache_write").or_else(|| rate("input_cache_write")),
    };
    (c.input.is_some() || c.output.is_some() || c.cache_read.is_some() || c.cache_write.is_some()).then_some(c)
}

/// Parses provider listings, including OpenRouter, Anthropic and Codex catalog metadata.
/// Missing facts remain unknown; an explicit empty capability list is authoritative.
pub fn parse_openai_models(v: &Value) -> Vec<(String, ModelInfo)> {
    v["data"].as_array().or_else(|| v["models"].as_array()).or_else(|| v.as_array())
        .into_iter().flatten().filter_map(|m| {
            let id = m["id"].as_str().or_else(|| m["slug"].as_str())?;
            Some((id.to_string(), parse_info(m)))
        }).collect()
}

fn parse_info(m: &Value) -> ModelInfo {
    let caps = &m["capabilities"];
    let mut info = ModelInfo {
        provider: m["provider"].as_str().map(str::to_string),
        name: m["display_name"].as_str().or_else(|| m["name"].as_str()).map(str::to_string),
        description: m["description"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
        window: positive(&m["max_input_tokens"]).or_else(|| positive(&m["context_window"]))
            .or_else(|| positive(&m["context_length"])).or_else(|| positive(&m["max_model_len"]))
            .or_else(|| positive(&m["limit"]["input"])).or_else(|| positive(&m["limit"]["context"])),
        max_output: positive(&m["max_tokens"]).or_else(|| positive(&m["max_output_tokens"]))
            .or_else(|| positive(&m["top_provider"]["max_completion_tokens"])).or_else(|| positive(&m["limit"]["output"])),
        default_effort: m["default_reasoning_level"].as_str().or_else(|| m["default_effort"].as_str())
            .or_else(|| caps["effort"]["default"].as_str()).map(str::to_string),
        adaptive_thinking: supported(&caps["thinking"]["types"]["adaptive"]).or_else(|| supported(&m["adaptive_thinking"])),
        server_compaction: supported(&caps["compaction"])
            .or_else(|| supported(&caps["context_management"]["compaction"]))
            .or_else(|| supported(&caps["context_management"]["compact_20260112"]))
            .or_else(|| supported(&m["server_compaction"])),
        reasoning: supported(&caps["thinking"]).or_else(|| m["reasoning"].as_bool()),
        thinking_display: m["thinking_display"].as_str().or_else(|| caps["thinking"]["display"].as_str()).map(str::to_string),
        fallbacks: supported(&m["fallbacks"]).or_else(|| supported(&caps["fallbacks"])),
        cost: prices(&m["cost"], false).or_else(|| prices(&m["pricing"], true)),
        ..Default::default()
    };
    if let (Some(window), Some(percent)) = (info.window, m["effective_context_window_percent"].as_u64()) {
        info.window = Some(((window as u128 * percent.min(100) as u128) / 100) as u64);
    }
    if let Some(levels) = m["supported_reasoning_levels"].as_array().or_else(|| m["efforts"].as_array()) {
        info.efforts_known = true;
        info.efforts = levels.iter().filter_map(|l| {
            Some((l.as_str().or_else(|| l["effort"].as_str())?.to_string(), l["description"].as_str().map(str::to_string)))
        }).collect();
        info.reasoning = info.reasoning.or(Some(!info.efforts.is_empty()));
    } else if supported(&caps["effort"]).is_some()
        || caps["effort"].as_object().is_some_and(|levels| levels.values().any(|v| supported(v).is_some()))
    {
        info.efforts_known = true;
        if supported(&caps["effort"]) != Some(false) {
            for (level, capability) in caps["effort"].as_object().into_iter().flatten() {
                if !["supported", "default", "description"].contains(&level.as_str())
                    && supported(capability) == Some(true)
                    && !info.efforts.iter().any(|(known, _)| known == level)
                {
                    info.efforts.push((level.clone(), capability["description"].as_str().map(str::to_string)));
                }
            }
        }
    }
    if let Some(tiers) = m["service_tiers"].as_array() {
        info.fast_known = true;
        info.fast = tiers.iter().find_map(|t| {
            let id = t.as_str().or_else(|| t["id"].as_str())?;
            (id == "priority" || t["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case("fast")))
                .then(|| Fast::ServiceTier { tier: id.to_string(), description: t["description"].as_str().map(str::to_string) })
        });
    } else {
        let fast = [&caps["fast_mode"], &caps["speed"]["fast"], &caps["fast"], &m["fast"]];
        if fast.iter().any(|v| supported(v).is_some()) {
            info.fast_known = true;
            info.fast = fast.iter().any(|v| supported(v) == Some(true)).then_some(Fast::AnthropicSpeed);
        }
    }
    info
}

/// A CLIProxyAPI server's own catalog: the Codex-format listing (`/v1/models?client_version=`)
/// carries windows, effort levels and speed tiers for every model it serves, Claude included;
/// the Anthropic-format listing adds output limits.
pub async fn cliproxy_models(http: &reqwest::Client, base_url: &str, api_key: Option<&str>) -> Result<Vec<(String, ModelInfo)>> {
    let base = base_url.trim_end_matches('/');
    let get = |url: String, anthropic: bool| {
        let mut b = http.get(url).timeout(Duration::from_secs(10));
        if let Some(k) = api_key {
            b = b.bearer_auth(k).header("x-api-key", k);
        }
        if anthropic {
            b = b.header("anthropic-version", "2023-06-01");
        }
        b
    };
    let res = get(format!("{base}/v1/models?client_version="), false).send().await?;
    if !res.status().is_success() {
        bail!("{base}/v1/models: HTTP {}", res.status());
    }
    let mut models = parse_openai_models(&res.json::<Value>().await?);
    if let Ok(res) = get(format!("{base}/v1/models"), true).send().await {
        if res.status().is_success() {
            if let Ok(v) = res.json::<Value>().await {
                let extra: HashMap<String, ModelInfo> = parse_openai_models(&v).into_iter().collect();
                for (id, info) in &mut models {
                    if let Some(other) = extra.get(id) {
                        info.fill_from(other);
                    }
                }
            }
        }
    }
    Ok(models)
}

const CATALOG_URL: &str = "https://models.dev/api.json";
/// First-party providers win when several list the same model id.
const CATALOG_PRIORITY: &[&str] = &["anthropic", "openai", "google", "mistral", "deepseek", "xai", "moonshotai", "alibaba"];

/// models.dev: bare model id → what it knows. Cached on disk for a day; a stale cache is used
/// when the catalog can't be fetched.
pub async fn models_dev(http: &reqwest::Client, cache: &Path) -> HashMap<String, ModelInfo> {
    let read = || std::fs::read(cache).ok().and_then(|b| serde_json::from_slice::<HashMap<String, ModelInfo>>(&b).ok());
    let fresh = std::fs::metadata(cache)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age.as_secs() < 86_400);
    if fresh {
        if let Some(map) = read().filter(|map| map.values().all(|info| info.provider.is_some())) {
            return map;
        }
    }
    let fetched = async {
        let v: Value = http.get(CATALOG_URL).timeout(Duration::from_secs(30)).send().await?.error_for_status()?.json().await?;
        let providers = v.as_object().ok_or_else(|| anyhow::anyhow!("unexpected catalog shape"))?;
        let ordered = CATALOG_PRIORITY
            .iter()
            .filter_map(|p| providers.get(*p).map(|v| (*p, v)))
            .chain(providers.iter().filter(|(k, _)| !CATALOG_PRIORITY.contains(&k.as_str())).map(|(k, v)| (k.as_str(), v)));
        let mut map: HashMap<String, ModelInfo> = HashMap::new();
        for (provider, p) in ordered {
            for (id, m) in p["models"].as_object().into_iter().flatten() {
                if map.contains_key(id) {
                    continue;
                }
                let mut info = parse_info(m);
                info.provider = Some(provider.to_string());
                map.insert(id.clone(), info);
            }
        }
        anyhow::Ok(map)
    }
    .await;
    match fetched {
        Ok(map) => {
            if let Ok(bytes) = serde_json::to_vec(&map) {
                let _ = atomic_write(cache, &bytes);
            }
            map
        }
        Err(_) => read().unwrap_or_default(),
    }
}

/// Context windows learned from overflow errors, persisted so each limit is learned once.
pub fn load_learned(path: &Path) -> HashMap<String, u64> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_learned(path: &Path, learned: &HashMap<String, u64>) {
    if let Ok(bytes) = serde_json::to_vec(learned) {
        let _ = atomic_write(path, &bytes);
    }
}

/// Price of one response, unknown if any used token category has no reported rate.
pub fn cost(info: &ModelInfo, usage: &Value) -> Option<f64> {
    let c = info.cost.as_ref()?;
    let n = |k: &str| usage[k].as_u64().unwrap_or(0) as f64;
    let cached = n("cache_read_input_tokens").max(usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0) as f64);
    let written = n("cache_creation_input_tokens");
    let input = usage["input_tokens"].as_u64().map(|n| n as f64)
        .unwrap_or_else(|| (n("prompt_tokens") - cached).max(0.0));
    let output = usage["output_tokens"].as_u64().map(|n| n as f64).unwrap_or_else(|| n("completion_tokens"));
    let cache_cost = if cached > 0.0 { cached * c.cache_read? } else { 0.0 };
    let write_cost = if written > 0.0 { written * c.cache_write? } else { 0.0 };
    let input_cost = if input > 0.0 { input * c.input? } else { 0.0 };
    let output_cost = if output > 0.0 { output * c.output? } else { 0.0 };
    Some((input_cost + output_cost + cache_cost + write_cost) / 1_000_000.0)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // Same directory ensures rename is atomic; exclusive creation avoids concurrent writers.
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temporary = path.with_file_name(format!(".{name}.{}.{nonce}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

pub fn option(value: &str, info: &ModelInfo, fallback_name: &str) -> Value {
    let description = match (info.description.as_deref(), info.summary()) {
        (Some(prose), Some(summary)) if !prose.contains(&summary) => Some(format!("{prose} · {summary}")),
        (Some(prose), _) => Some(prose.to_string()),
        (None, summary) => summary,
    };
    json!({ "value": value, "name": info.name.clone().unwrap_or_else(|| fallback_name.to_string()), "description": description })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_capability_negatives_block_fallback() {
        let mut provider = parse_openai_models(&json!({"data": [{
            "id": "arbitrary", "supported_reasoning_levels": [], "service_tiers": [],
            "adaptive_thinking": false, "server_compaction": false, "fallbacks": false
        }]})).remove(0).1;
        provider.fill_from(&ModelInfo {
            efforts: vec![("high".into(), None)], default_effort: Some("high".into()),
            fast: Some(Fast::AnthropicSpeed), adaptive_thinking: Some(true),
            server_compaction: Some(true), fallbacks: Some(true), window: Some(123456),
            ..Default::default()
        });
        assert!(provider.efforts.is_empty());
        assert_eq!(provider.default_effort, None);
        assert_eq!(provider.fast, None);
        assert_eq!(provider.adaptive_thinking, Some(false));
        assert_eq!(provider.server_compaction, Some(false));
        assert_eq!(provider.fallbacks, Some(false));
        assert_eq!(provider.window, Some(123456));
    }

    #[test]
    fn catalog_preserves_order_descriptions_and_limits() {
        let info = parse_openai_models(&json!({"models": [{
            "slug": "arbitrary", "context_window": 272000, "effective_context_window_percent": 95,
            "max_tokens": 128000, "description": "Provider prose", "default_reasoning_level": "ultra",
            "supported_reasoning_levels": [{"effort": "ultra", "description": "Deep"}, {"effort": "low"}],
            "service_tiers": [{"id": "priority", "description": "Faster"}]
        }]})).remove(0).1;
        assert_eq!(info.window, Some(258400));
        assert_eq!(info.max_output, Some(128000));
        assert_eq!(info.efforts, vec![("ultra".into(), Some("Deep".into())), ("low".into(), None)]);
        assert_eq!(info.default_effort.as_deref(), Some("ultra"));
        assert_eq!(info.fast, Some(Fast::ServiceTier { tier: "priority".into(), description: Some("Faster".into()) }));
        let description = option("id", &info, "Name")["description"].as_str().unwrap().to_string();
        assert!(description.contains("Provider prose"));
        assert!(description.contains("258400 context"));
        assert!(description.contains("128K output"));
    }

    #[test]
    fn anthropic_supported_objects_and_bools() {
        let info = parse_openai_models(&json!({"data": [{
            "id": "arbitrary", "max_input_tokens": 1000000, "max_tokens": 128000,
            "capabilities": {
                "effort": {"supported": true, "low": true, "high": {"supported": true}, "future": true, "max": false},
                "thinking": {"supported": true, "types": {"adaptive": {"supported": true}}},
                "fast_mode": {"supported": false},
                "context_management": {"compact_20260112": {"supported": true}}
            }
        }]})).remove(0).1;
        assert_eq!(info.window, Some(1000000));
        assert_eq!(info.adaptive_thinking, Some(true));
        assert_eq!(info.server_compaction, Some(true));
        assert!(info.fast_known);
        assert_eq!(info.fast, None);
        let missing = parse_openai_models(&json!({"data": [{"id": "claude-whatever"}]})).remove(0).1;
        assert!(!missing.fast_known && !missing.efforts_known);
        assert_eq!(missing.adaptive_thinking, None);
    }

    #[test]
    fn pricing_requires_rates_for_used_cache_categories() {
        let mut info = parse_openai_models(&json!({"data": [{
            "id": "arbitrary", "pricing": {"prompt": "0.000002", "completion": "0.00001"}
        }]})).remove(0).1;
        assert_eq!(cost(&info, &json!({"input_tokens": 1000000, "output_tokens": 1000000})), Some(12.0));
        assert_eq!(cost(&info, &json!({"input_tokens": 10, "cache_read_input_tokens": 1})), None);
        assert_eq!(cost(&info, &json!({"cache_creation_input_tokens": 1})), None);
        info.cost.as_mut().unwrap().cache_read = Some(0.5);
        assert_eq!(cost(&info, &json!({"prompt_tokens": 1000000, "prompt_tokens_details": {"cached_tokens": 1000000}})), Some(0.5));
        assert_eq!(cost(&ModelInfo::default(), &json!({})), None);
    }

    #[test]
    fn limit_boundaries_do_not_overflow_or_report_zero_as_known() {
        let info = parse_openai_models(&json!({"data": [{
            "id": "arbitrary", "context_window": u64::MAX, "effective_context_window_percent": 100,
            "max_tokens": 0
        }]})).remove(0).1;
        assert_eq!(info.window, Some(u64::MAX));
        assert_eq!(info.max_output, None);
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1000), "1K");
        assert_eq!(tokens(1001), "1001");
        assert_eq!(tokens(1000000), "1M");
    }

    #[test]
    fn partial_prices_fill_individually_without_overwriting_provider_rates() {
        let mut provider = parse_openai_models(&json!({"data": [{
            "id": "arbitrary", "cost": {"input": 2}
        }]})).remove(0).1;
        assert_eq!(cost(&provider, &json!({"output_tokens": 1})), None);
        assert_eq!(cost(&provider, &json!({"input_tokens": 1000000})), Some(2.0));
        provider.fill_from(&ModelInfo {
            cost: Some(Cost { input: Some(99.0), output: Some(10.0), cache_read: Some(0.5), cache_write: None }),
            ..Default::default()
        });
        assert_eq!(cost(&provider, &json!({"input_tokens": 1000000, "output_tokens": 1000000})), Some(12.0));
    }
}

