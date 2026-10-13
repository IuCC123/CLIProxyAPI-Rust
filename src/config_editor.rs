//! Structured edits to the original config, retaining compatibility fields and comments.

use std::collections::HashSet;
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};
use serde_yaml::{Mapping, Value as Yaml};
use sha2::{Digest, Sha256};
use yaml_edit::{Document, Mapping as EditMapping, YamlFile, YamlNode};

use crate::config::Config;

const PROVIDERS: &[(&str, &str)] = &[
    ("claude-api-key", "claude"),
    ("codex-api-key", "codex"),
    ("gemini-api-key", "gemini"),
    ("vertex-api-key", "vertex"),
    ("kimi-api-key", "kimi"),
    ("xai-api-key", "xai"),
    ("meta-api-key", "meta"),
    ("openai-compatibility", "openai-compatibility"),
];

pub fn revision(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn yaml(text: &str) -> Result<Yaml> {
    let doc: Yaml = serde_yaml::from_str(text).context("The config file is not valid YAML")?;
    if doc.is_null() {
        return Ok(Yaml::Mapping(Mapping::new()));
    }
    ensure!(doc.is_mapping(), "The config must be a mapping of settings");
    Ok(doc)
}

fn at<'a>(doc: &'a Yaml, path: &[Value]) -> Option<&'a Yaml> {
    path.iter().try_fold(doc, |v, part| match part {
        Value::String(k) => v.as_mapping()?.get(k.as_str()),
        Value::Number(i) => v.as_sequence()?.get(i.as_u64()? as usize),
        _ => None,
    })
}

fn at_mut<'a>(doc: &'a mut Yaml, path: &[Value]) -> Option<&'a mut Yaml> {
    path.iter().try_fold(doc, |v, part| match part {
        Value::String(k) => v.as_mapping_mut()?.get_mut(Yaml::String(k.clone())),
        Value::Number(i) => v.as_sequence_mut()?.get_mut(i.as_u64()? as usize),
        _ => None,
    })
}

fn p(path: &[&str]) -> Vec<Value> {
    path.iter().map(|s| json!(s)).collect()
}

fn v8(doc: &Yaml) -> bool {
    doc["config-version"].as_i64().is_some_and(|v| v >= 8) || doc["api-keys"].is_mapping()
}

fn provider_sources(doc: &Yaml, field: &str, group: &str) -> Vec<Vec<Value>> {
    if let Some(groups) = doc["api-keys"][group].as_sequence() {
        if group == "openai-compatibility" {
            return groups
                .iter()
                .enumerate()
                .filter(|(_, g)| g.is_mapping())
                .map(|(i, _)| vec![json!("api-keys"), json!(group), json!(i)])
                .collect();
        }
        return groups
            .iter()
            .enumerate()
            .flat_map(|(i, g)| {
                g["keys"]
                    .as_sequence()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter(|(_, key)| key.is_mapping())
                    .map(move |(j, _)| vec![json!("api-keys"), json!(group), json!(i), json!("keys"), json!(j)])
            })
            .collect();
    }
    let field = if field == "gemini-api-key" && doc[field].is_null() && doc["generative-language-api-key"].is_sequence()
    {
        "generative-language-api-key"
    } else {
        field
    };
    (0..doc[field].as_sequence().map_or(0, Vec::len)).map(|i| vec![json!(field), json!(i)]).collect()
}

pub fn values(text: &str) -> Result<Value> {
    let doc = yaml(text)?;
    let cfg = Config::parse(text)?;
    let mut values = serde_json::to_value(&cfg)?;
    let obj = values.as_object_mut().unwrap();
    obj.insert("tls".into(), serde_json::to_value(&cfg.tls)?);
    obj.insert("management-allow-remote".into(), json!(cfg.management_allow_remote));
    obj.insert("force-model-prefix".into(), json!(cfg.force_model_prefix));
    obj.insert("oauth-model-alias".into(), json!(cfg.oauth_model_alias));
    obj.insert("oauth-excluded-models".into(), json!(cfg.oauth_excluded_models));
    for (field, group) in PROVIDERS {
        let sources = provider_sources(&doc, field, group);
        for (entry, source) in obj.get_mut(*field).and_then(Value::as_array_mut).into_iter().flatten().zip(sources) {
            entry.as_object_mut().unwrap().insert("_id".into(), json!(source));
        }
        for entry in obj.get_mut(*field).and_then(Value::as_array_mut).into_iter().flatten() {
            if let Some(models) = entry.get_mut("models") {
                mark_rows(models);
            }
        }
    }
    for aliases in obj["oauth-model-alias"].as_object_mut().unwrap().values_mut() {
        mark_rows(aliases);
    }
    Ok(values)
}

fn mark_rows(value: &mut Value) {
    for (i, row) in value.as_array_mut().into_iter().flatten().enumerate() {
        if let Some(row) = row.as_object_mut() {
            row.insert("_row".into(), json!(i));
        }
    }
}

fn without_metadata(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, v)| !(k.as_str() == "_id" && v.is_array() || k.as_str() == "_row" && v.is_number()))
                .map(|(k, v)| (k.clone(), without_metadata(v)))
                .collect(),
        ),
        Value::Array(list) => Value::Array(list.iter().map(without_metadata).collect()),
        _ => value.clone(),
    }
}

fn setting_path(doc: &Yaml, field: &str) -> (Vec<Value>, bool) {
    let nested: &[&str] = match field {
        "host" => &["server", "host"],
        "port" => &["server", "port"],
        "tls" => &["server", "tls"],
        "api-keys" => &["access", "api-keys"],
        "auth-dir" => &["oauth", "auth-dir"],
        "management-key" => &["management", "secret-key"],
        "management-allow-remote" => &["management", "allow-remote"],
        "request-retry" => &["routing", "retry", "request-retry"],
        "force-model-prefix" => &["routing", "force-model-prefix"],
        "session-affinity" => &["routing", "session-affinity"],
        "five-hour-reserve-percent" => &["routing", "five-hour-reserve-percent"],
        "proxy-url" => &["requests", "proxy-url"],
        "oauth-model-alias" => &["oauth", "model-alias"],
        "oauth-excluded-models" => &["oauth", "excluded-models"],
        "debug" => &["observability", "logs", "debug"],
        _ => &[],
    };
    if field == "routing" {
        return (if doc["routing"].is_mapping() { p(&["routing", "strategy"]) } else { p(&["routing"]) }, false);
    }
    if !nested.is_empty() && at(doc, &p(nested)).is_some_and(|v| !v.is_null()) {
        return (p(nested), false);
    }
    if field.starts_with("management-") {
        let old = p(&["remote-management", if field == "management-key" { "secret-key" } else { "allow-remote" }]);
        if doc[field].is_null() && at(doc, &old).is_some_and(|v| !v.is_null()) {
            return (old, false);
        }
    }
    if field == "claude-cloak" && doc[field].is_null() {
        let nested = p(&["oauth", "providers", "claude", "disable-claude-cloak-mode"]);
        if at(doc, &nested).is_some() || v8(doc) {
            return (nested, true);
        }
        if !doc["disable-claude-cloak-mode"].is_null() {
            return (p(&["disable-claude-cloak-mode"]), true);
        }
    }
    (if v8(doc) && !nested.is_empty() { p(nested) } else { p(&[field]) }, false)
}

fn set_path(doc: &mut Yaml, path: &[Value], value: Yaml) -> Result<()> {
    let (last, parents) = path.split_last().context("Empty setting path")?;
    let mut current = doc;
    for part in parents {
        let k = part.as_str().context("Invalid setting path")?;
        if !current[k].is_mapping() {
            let old_strategy = (k == "routing").then(|| current[k].as_str().map(str::to_owned)).flatten();
            current
                .as_mapping_mut()
                .context("Invalid config section")?
                .insert(Yaml::String(k.into()), Yaml::Mapping(Mapping::new()));
            if let Some(strategy) = old_strategy {
                current[k].as_mapping_mut().unwrap().insert(Yaml::String("strategy".into()), Yaml::String(strategy));
            }
        }
        current = current.as_mapping_mut().unwrap().get_mut(k).unwrap();
    }
    current
        .as_mapping_mut()
        .context("Invalid config section")?
        .insert(Yaml::String(last.as_str().context("Invalid setting name")?.into()), value);
    Ok(())
}

/// Merge just the visible delta, so unknown properties on entries also survive.
fn delta(raw: &mut Yaml, before: &Value, after: &Value) -> Result<()> {
    if before == after {
        return Ok(());
    }
    if let (Some(old), Some(new), Some(target)) = (before.as_object(), after.as_object(), raw.as_mapping_mut()) {
        for (k, val) in new {
            if k == "_row" && val.is_number() {
                continue;
            }
            let old = old.get(k).unwrap_or(&Value::Null);
            if old == val {
                continue;
            }
            let target = target.entry(Yaml::String(k.clone())).or_insert(Yaml::Null);
            delta(target, old, val)?;
        }
        for k in old.keys().filter(|k| k.as_str() != "_row" && !new.contains_key(*k)) {
            target.remove(Yaml::String(k.clone()));
        }
    } else if let (Some(old), Some(new), Some(target)) = (before.as_array(), after.as_array(), raw.as_sequence_mut()) {
        let original = target.clone();
        let mut used = HashSet::new();
        let mut result = Vec::new();
        for (i, val) in new.iter().enumerate() {
            let source = if let Some(id) = val.get("_row") {
                old.iter().enumerate().find(|(j, v)| !used.contains(j) && v.get("_row") == Some(id)).map(|(j, _)| j)
            } else if old.iter().any(|v| v.get("_row").is_some()) {
                None
            } else {
                old.iter()
                    .enumerate()
                    .find(|(j, v)| !used.contains(j) && *v == val)
                    .map(|(j, _)| j)
                    .or_else(|| (i < old.len() && !used.contains(&i)).then_some(i))
            };
            if let Some(j) = source {
                used.insert(j);
                let mut raw = original.get(j).cloned().unwrap_or(Yaml::Null);
                delta(&mut raw, &old[j], val)?;
                result.push(raw);
            } else {
                result.push(serde_yaml::to_value(without_metadata(val))?);
            }
        }
        *target = result;
    } else {
        *raw = serde_yaml::to_value(without_metadata(after))?;
    }
    Ok(())
}

fn append_provider(doc: &mut Yaml, field: &str, group: &str, entry: &Value) -> Result<()> {
    let mut entry = without_metadata(entry);
    entry.as_object_mut().context("Invalid provider entry")?.remove("_id");
    let mut entry = serde_yaml::to_value(entry)?;
    let nested = v8(doc) && group != "kimi";
    if nested {
        if group == "openai-compatibility" {
            let keys = entry.as_mapping_mut().unwrap().remove("api-keys").unwrap_or(Yaml::Sequence(vec![]));
            entry.as_mapping_mut().unwrap().insert(
                Yaml::String("keys".into()),
                Yaml::Sequence(
                    keys.as_sequence()
                        .into_iter()
                        .flatten()
                        .map(|k| Yaml::Mapping(Mapping::from_iter([(Yaml::String("api-key".into()), k.clone())])))
                        .collect(),
                ),
            );
        } else {
            entry = Yaml::Mapping(Mapping::from_iter([(Yaml::String("keys".into()), Yaml::Sequence(vec![entry]))]));
        }
        if !doc["api-keys"].is_mapping() {
            set_path(doc, &p(&["api-keys"]), Yaml::Mapping(Mapping::new()))?;
        }
        if !doc["api-keys"][group].is_sequence() {
            set_path(doc, &p(&["api-keys", group]), Yaml::Sequence(vec![]))?;
        }
        doc["api-keys"][group].as_sequence_mut().unwrap().push(entry);
    } else {
        if !doc[field].is_sequence() {
            set_path(doc, &p(&[field]), Yaml::Sequence(vec![]))?;
        }
        doc[field].as_sequence_mut().unwrap().push(entry);
    }
    Ok(())
}

fn compat_keys(raw: &mut Yaml, before: &Value, after: &Value) -> Result<()> {
    let field = if raw["keys"].is_sequence() {
        "keys"
    } else if raw["api-key-entries"].is_sequence() {
        "api-key-entries"
    } else {
        "api-keys"
    };
    if field == "api-keys" {
        return delta(&mut raw[field], before, after);
    }
    let original = raw[field].as_sequence().cloned().unwrap_or_default();
    let old = before.as_array().context("Invalid API keys")?;
    let new = after.as_array().context("Invalid API keys")?;
    let mut used = HashSet::new();
    let mut keys = Vec::new();
    for (i, key) in new.iter().enumerate() {
        let j = old
            .iter()
            .enumerate()
            .find(|(j, v)| !used.contains(j) && *v == key)
            .map(|(j, _)| j)
            .or_else(|| (i < original.len() && !used.contains(&i)).then_some(i));
        let mut item = j
            .and_then(|j| {
                used.insert(j);
                original.get(j).cloned()
            })
            .unwrap_or_else(|| Yaml::Mapping(Mapping::new()));
        if !item.is_mapping() {
            item = Yaml::Mapping(Mapping::new());
        }
        item.as_mapping_mut().unwrap().insert(Yaml::String("api-key".into()), serde_yaml::to_value(key)?);
        keys.push(item);
    }
    raw.as_mapping_mut().context("Invalid provider")?.insert(Yaml::String(field.into()), Yaml::Sequence(keys));
    Ok(())
}

fn edit_providers(doc: &mut Yaml, field: &str, group: &str, before: &Value, after: &Value) -> Result<()> {
    let old = before.as_array().context("Invalid provider list")?;
    let new = after.as_array().context("Invalid provider list")?;
    // Migrate the ancient scalar-only Gemini layout when provider options are
    // edited. A complete list is retained under its modern legacy spelling.
    if field == "gemini-api-key" && doc[field].is_null() && doc["generative-language-api-key"].is_sequence() {
        for entry in new {
            if let Some(id) = entry.get("_id").filter(|id| !id.is_null()) {
                ensure!(old.iter().any(|previous| previous.get("_id") == Some(id)), "Invalid Gemini key identifier");
            }
        }
        let mut entries = new.clone();
        for entry in &mut entries {
            entry.as_object_mut().context("Invalid Gemini entry")?.remove("_id");
        }
        set_path(doc, &p(&[field]), serde_yaml::to_value(without_metadata(&json!(entries)))?)?;
        doc.as_mapping_mut().unwrap().remove("generative-language-api-key");
        return Ok(());
    }
    let mut seen = HashSet::new();
    for entry in new {
        ensure!(entry.is_object(), "Invalid provider entry");
        let Some(id) = entry.get("_id").filter(|id| !id.is_null()) else { continue };
        ensure!(seen.insert(id.to_string()), "Duplicate provider entry");
        let previous = old
            .iter()
            .find(|e| e.get("_id") == Some(id))
            .context("This provider entry no longer exists; reload the config")?;
        let source = id.as_array().context("Invalid provider identifier")?;
        let inherited = if source.len() == 5 { at(doc, &source[..3]).cloned() } else { None };
        let raw = at_mut(doc, source).context("Provider not found")?;
        for (k, val) in entry.as_object().unwrap() {
            if k.starts_with('_') || previous.get(k) == Some(val) {
                continue;
            }
            if group == "openai-compatibility" && k == "api-keys" {
                compat_keys(raw, &previous[k], val)?;
            } else {
                let inherited_value = inherited.as_ref().map(|g| g[k.as_str()].clone()).unwrap_or(Yaml::Null);
                let target = raw
                    .as_mapping_mut()
                    .context("Invalid provider entry")?
                    .entry(Yaml::String(k.clone()))
                    .or_insert(inherited_value);
                // An explicit empty string overrides a group's inherited URL
                // or prefix; null means inherit in CLIProxyAPI's v8 layout.
                let val = if val.is_null()
                    && previous.get(k).is_some_and(Value::is_string)
                    && matches!(k.as_str(), "base-url" | "proxy-url" | "prefix" | "label")
                {
                    json!("")
                } else {
                    val.clone()
                };
                delta(target, previous.get(k).unwrap_or(&Value::Null), &val)?;
            }
        }
    }
    let mut removed: Vec<_> =
        old.iter().filter_map(|e| e.get("_id")).filter(|id| !seen.contains(&id.to_string())).collect();
    removed.sort_by_key(|id| id.to_string());
    // Indices must be removed from the end, including multi-digit indices.
    removed.sort_by(|a, b| {
        let a = a.as_array().unwrap();
        let b = b.as_array().unwrap();
        a[..a.len() - 1]
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .cmp(&b[..b.len() - 1].iter().map(Value::to_string).collect::<Vec<_>>())
            .then(a.last().unwrap().as_u64().cmp(&b.last().unwrap().as_u64()))
    });
    for id in removed.into_iter().rev() {
        let path = id.as_array().unwrap();
        let (index, parent) = path.split_last().unwrap();
        at_mut(doc, parent)
            .and_then(Yaml::as_sequence_mut)
            .context("Invalid provider list")?
            .remove(index.as_u64().unwrap() as usize);
    }
    for entry in new.iter().filter(|e| e.get("_id").is_none_or(Value::is_null)) {
        append_provider(doc, field, group, entry)?;
    }
    Ok(())
}

fn check_url(value: &str, proxy: bool, label: &str) -> Result<()> {
    if value.is_empty() {
        return Ok(());
    }
    let url = url::Url::parse(value).with_context(|| format!("{label}: enter a complete URL"))?;
    ensure!(
        url.host_str().is_some()
            && if proxy {
                matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
            } else {
                matches!(url.scheme(), "http" | "https")
            },
        "{label}: use {}",
        if proxy { "an HTTP, HTTPS or SOCKS5 URL" } else { "an HTTP or HTTPS URL" }
    );
    Ok(())
}

fn validate(values: &Value, changes: &Map<String, Value>) -> Result<()> {
    for (field, value) in changes {
        ensure!(values.get(field).is_some(), "Unknown setting: {field}");
        match field.as_str() {
            "port" => {
                ensure!(value.as_u64().is_some_and(|p| (1..=65535).contains(&p)), "Port must be between 1 and 65535")
            }
            "request-retry" => ensure!(
                value.as_u64().is_some_and(|n| n <= u32::MAX as u64),
                "Account attempts must be a nonnegative whole number"
            ),
            "routing" => ensure!(
                matches!(
                    value.as_str(),
                    Some("least-used" | "smart-quota" | "soonest-reset" | "round-robin" | "fill-first")
                ),
                "Choose a valid routing strategy"
            ),
            "five-hour-reserve-percent" => ensure!(
                value.as_u64().is_some_and(|n| n <= 100),
                "5-hour reserve must be a whole percentage between 0 and 100"
            ),
            "host" => ensure!(
                value.as_str().is_some_and(|h| h.is_empty() || h.parse::<std::net::IpAddr>().is_ok()),
                "Bind address must be an IPv4 or IPv6 address"
            ),
            "auth-dir" => {
                ensure!(value.as_str().is_some_and(|s| !s.trim().is_empty()), "Credentials directory cannot be empty")
            }
            "management-allow-remote" => {
                ensure!(value.is_null() || value.is_boolean(), "Remote access must be enabled or disabled")
            }
            "session-affinity-idle-seconds" => {
                ensure!(value.as_u64().is_some_and(|n| n >= 60), "Forget idle sessions after at least 60 seconds")
            }
            "codex-websockets" | "claude-cloak" | "banked-resets" | "session-affinity" | "force-model-prefix"
            | "debug" => {
                ensure!(value.is_boolean(), "{field}: use a boolean")
            }
            _ => {}
        }
    }
    // Check types before Config's deliberately lenient YAML deserializers run.
    let mut merged = values.clone();
    for (k, v) in changes {
        merged[k] = v.clone();
    }
    let cfg = Config::parse(&serde_yaml::to_string(&merged)?)?;
    if changes.contains_key("proxy-url") {
        check_url(&cfg.proxy_url, true, "Upstream proxy")?;
    }
    if changes.contains_key("tls") && cfg.tls.enable {
        ensure!(!cfg.tls.cert.trim().is_empty(), "HTTPS: certificate path is required");
        ensure!(!cfg.tls.key.trim().is_empty(), "HTTPS: private key path is required");
    }
    for (field, _) in PROVIDERS.iter().filter(|(f, _)| changes.contains_key(*f)) {
        for entry in merged[*field].as_array().context("Invalid provider list")? {
            if *field == "openai-compatibility" {
                ensure!(
                    entry["name"].as_str().is_some_and(|s| !s.trim().is_empty()),
                    "Compatible provider: name is required"
                );
                ensure!(
                    entry["base-url"].as_str().is_some_and(|s| !s.is_empty()),
                    "Compatible provider: base URL is required"
                );
            } else {
                ensure!(
                    entry["api-key"].as_str().is_some_and(|s| !s.trim().is_empty()),
                    "Provider API key cannot be empty"
                );
            }
            if let Some(url) = entry["base-url"].as_str() {
                check_url(url, false, "Provider base URL")?;
            }
            if let Some(url) = entry["proxy-url"].as_str() {
                check_url(url, true, "Provider proxy")?;
            }
            for model in entry["models"].as_array().into_iter().flatten() {
                ensure!(model["name"].as_str().is_some_and(|s| !s.trim().is_empty()), "Model name cannot be empty");
            }
            for (name, value) in entry["headers"].as_object().into_iter().flatten() {
                ensure!(
                    axum::http::HeaderName::from_bytes(name.as_bytes()).is_ok(),
                    "Invalid HTTP header name: {name}"
                );
                ensure!(
                    value.as_str().is_some_and(|s| axum::http::HeaderValue::from_str(s).is_ok()),
                    "Invalid HTTP header value: {name}"
                );
            }
        }
    }
    if changes.contains_key("oauth-model-alias") {
        for aliases in cfg.oauth_model_alias.values() {
            for alias in aliases {
                ensure!(
                    !alias.name.trim().is_empty() && !alias.alias.trim().is_empty(),
                    "OAuth aliases need an upstream name and an alias"
                );
            }
        }
    }
    Ok(())
}

/// Identity is used only for retaining comments when list entries move or disappear.
fn identity(value: &Yaml) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(format!("s:{s}"));
    }
    for field in ["api-key", "name"] {
        if let Some(s) = value[field].as_str() {
            return Some(format!("{field}:{s}"));
        }
    }
    None
}

fn node(value: &Yaml) -> Result<YamlNode> {
    // Flow collections can be inserted at any indentation. Existing block
    // collections are edited recursively, retaining their original formatting.
    let doc = Document::from_str(&serde_json::to_string(value)?).context("Could not format setting")?;
    doc.as_mapping()
        .map(YamlNode::Mapping)
        .or_else(|| doc.as_sequence().map(YamlNode::Sequence))
        .or_else(|| doc.as_scalar().map(YamlNode::Scalar))
        .context("Could not format setting")
}

fn sync_mapping(mapping: &EditMapping, before: &Mapping, after: &Mapping) -> Result<()> {
    for (key, value) in after {
        let key = key.as_str().context("Config setting names must be strings")?;
        let old = before.get(key);
        if old == Some(value) {
            continue;
        }
        if let (Some(existing), Some(old)) = (mapping.get(key), old)
            && sync_node(&existing, old, value)?
        {
            continue;
        }
        mapping.set(key, node(value)?);
    }
    for key in before.keys().filter(|k| !after.contains_key(*k)) {
        if let Some(key) = key.as_str() {
            mapping.remove(key);
        }
    }
    Ok(())
}

fn sync_node(existing: &YamlNode, before: &Yaml, after: &Yaml) -> Result<bool> {
    if let (Some(mapping), Some(old), Some(new)) = (existing.as_mapping(), before.as_mapping(), after.as_mapping()) {
        if new.is_empty() {
            return Ok(false);
        }
        sync_mapping(mapping, old, new)?;
        return Ok(true);
    }
    if let (Some(seq), Some(old), Some(new)) = (existing.as_sequence(), before.as_sequence(), after.as_sequence()) {
        if new.is_empty() {
            return Ok(false);
        } // The lossless editor must emit [] rather than an empty block.
        let mut current = old.clone();
        for (i, value) in new.iter().enumerate() {
            if current.get(i) == Some(value) {
                continue;
            }
            let found = current
                .iter()
                .enumerate()
                .skip(i)
                .find(|(_, v)| *v == value || identity(v).is_some_and(|id| Some(id) == identity(value)))
                .map(|(j, _)| j);
            if let Some(j) = found {
                for _ in i..j {
                    seq.remove(i);
                    current.remove(i);
                }
            } else if i < current.len()
                && new[i + 1..]
                    .iter()
                    .any(|v| v == &current[i] || identity(v).is_some_and(|id| Some(id) == identity(&current[i])))
            {
                seq.insert(i, node(value)?);
                current.insert(i, value.clone());
                continue;
            }
            if let Some(old) = current.get(i) {
                let item = seq.get(i).context("Could not edit list entry")?;
                if !sync_node(&item, old, value)? {
                    seq.set(i, node(value)?);
                }
                current[i] = value.clone();
            } else {
                seq.push(node(value)?);
                current.push(value.clone());
            }
        }
        while seq.len() > new.len() {
            seq.pop();
        }
        return Ok(true);
    }
    Ok(false)
}

pub fn preserve(text: &str, before: &Yaml, after: &Yaml) -> Result<String> {
    if before == after {
        return Ok(text.into());
    }
    let source = if text.trim().is_empty() { "{}\n" } else { text };
    let file = YamlFile::from_str(source).context("Could not edit this YAML without losing formatting")?;
    let doc = file.document().context("The config must contain one document")?;
    let mapping = doc.as_mapping().context("The config must be a mapping")?;
    sync_mapping(
        &mapping,
        before.as_mapping().context("Invalid config")?,
        after.as_mapping().context("Invalid config")?,
    )?;
    let output = file.to_string();
    // Never write malformed output or a lossless edit that changed unrelated values.
    ensure!(yaml(&output)? == *after, "Could not preserve this config's formatting; the file was not changed");
    Ok(output)
}

/// Lossless when the file's layout allows it; otherwise a plain rewrite that drops
/// comments and layout (the second value is true). Some valid YAML, such as lists
/// written without indentation, can't be edited in place.
pub fn render(text: &str, before: &Yaml, after: &Yaml) -> Result<(String, bool)> {
    match preserve(text, before, after) {
        Ok(out) => Ok((out, false)),
        Err(e) => {
            tracing::warn!("rewriting config.yaml without its formatting: {e:#}");
            let out = serde_yaml::to_string(after)?;
            ensure!(yaml(&out)? == *after, "Could not write this config");
            Ok((out, true))
        }
    }
}

pub fn apply(text: &str, changes: &Map<String, Value>) -> Result<(String, Config, bool)> {
    let before_values = values(text)?;
    validate(&before_values, changes)?;
    let before = yaml(text)?;
    let mut after = before.clone();
    for (field, value) in changes {
        if before_values.get(field) == Some(value) {
            continue;
        }
        if let Some((_, group)) = PROVIDERS.iter().find(|(f, _)| *f == field) {
            edit_providers(&mut after, field, group, &before_values[field], value)?;
        } else {
            let (path, inverse) = setting_path(&after, field);
            let value = if inverse { json!(!value.as_bool().context("Invalid cloak setting")?) } else { value.clone() };
            let mut raw = at(&after, &path).cloned().unwrap_or(Yaml::Null);
            let old =
                if inverse { json!(!before_values[field].as_bool().unwrap()) } else { before_values[field].clone() };
            delta(&mut raw, &old, &value)?;
            set_path(&mut after, &path, raw)?;
        }
    }
    let (output, rewritten) = render(text, &before, &after)?;
    let cfg = Config::parse(&output)?;
    Ok((output, cfg, rewritten))
}

/// Report changed startup-only settings, including notification secret paths and trust permissions.
pub fn restart_fields(startup: &Config, current: &Config) -> Vec<&'static str> {
    let mut fields = Vec::new();
    if startup.host != current.host {
        fields.push("bind address");
    }
    if startup.port != current.port {
        fields.push("port");
    }
    if startup.tls.enable != current.tls.enable
        || startup.tls.cert != current.tls.cert
        || startup.tls.key != current.tls.key
    {
        fields.push("HTTPS");
    }
    if startup.debug != current.debug {
        fields.push("debug logging");
    }
    let old = serde_json::to_value(&startup.notifications).unwrap_or_default();
    let new = serde_json::to_value(&current.notifications).unwrap_or_default();
    if ["secrets-dir", "private-endpoints", "ca-file", "credential-proxy-cidrs"]
        .iter()
        .any(|key| old[*key] != new[*key])
    {
        fields.push("notification credential and network permissions");
    }
    if startup.auth_dir != current.auth_dir {
        fields.push("notification state directory");
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(text: &str, changes: Value) -> (String, Config) {
        let (out, cfg, rewritten) = apply(text, changes.as_object().unwrap()).unwrap();
        assert!(!rewritten, "expected a lossless edit");
        (out, cfg)
    }

    #[test]
    /// Verify that notification edits preserve layout and operator permissions.
    fn notification_edits_preserve_layout_and_operator_permissions() {
        for prefix in ["", "config-version: 8\n"] {
            let source = format!(
                "{prefix}# Keep this deployment note\nnotifications:\n  enabled: false # operator default\n  secrets-dir: /run/notification-secrets\n  private-endpoints:\n    - host: chat.example.net\n      port: 443\n      cidrs: [10.1.2.3/32]\n  destinations: []\nplugin-setting: keep-me\n"
            );
            let mut notifications = values(&source).unwrap()["notifications"].clone();
            notifications["enabled"] = json!(true);
            notifications["time-zone"] = json!("America/Denver");
            notifications["provider-logos"] = json!(false);
            notifications["credential-ui-enabled"] = json!(true);
            notifications["credential-public-url"] = json!("https://dashboard.example.test/");
            notifications["destinations"] = json!([{"id":"ops-discord","format":"discord","enabled":true}]);
            let (out, cfg) = edit(&source, json!({"notifications": notifications}));
            assert!(out.contains("# Keep this deployment note"));
            assert!(out.contains("# operator default"));
            assert_eq!(yaml(&out).unwrap()["plugin-setting"], "keep-me");
            assert!(cfg.notifications.enabled);
            assert_eq!(cfg.notifications.time_zone, "America/Denver");
            assert!(!cfg.notifications.provider_logos);
            assert!(cfg.notifications.credential_ui_enabled);
            assert_eq!(cfg.notifications.credential_public_url, "https://dashboard.example.test/");
            assert_eq!(cfg.notifications.destinations[0].id, "ops-discord");
            assert_eq!(
                yaml(&out).unwrap()["notifications"]["private-endpoints"],
                yaml(&source).unwrap()["notifications"]["private-endpoints"]
            );
            assert!(restart_fields(&Config::parse(&source).unwrap(), &cfg).is_empty());
        }
    }

    #[test]
    /// Verify that notification config rejects inline credentials and marks trust changes for restart.
    fn notification_config_rejects_inline_credentials_and_marks_trust_changes_for_restart() {
        let changes = json!({"notifications":{"enabled":true,"destinations":[{"id":"ops","format":"discord","url":"https://example.net/secret-sentinel"}]}});
        assert!(apply("", changes.as_object().unwrap()).is_err());
        let old = Config::default();
        let mut current = old.clone();
        current.notifications.secrets_dir = Some("/run/notification-secrets".into());
        assert!(restart_fields(&old, &current).contains(&"notification credential and network permissions"));
        current = old.clone();
        current.notifications.credential_proxy_cidrs = vec!["192.0.2.10/32".into()];
        assert!(restart_fields(&old, &current).contains(&"notification credential and network permissions"));
    }

    #[test]
    fn layouts_the_lossless_editor_cannot_follow_are_rewritten() {
        for source in [
            "# indentless list\nport: 8317\nclaude-api-key:\n- api-key: first\n",
            "---\n# explicit document\nport: 8317\nclaude-api-key:\n  - api-key: first\n...\n",
        ] {
            let mut keys = values(source).unwrap()["claude-api-key"].clone();
            keys.as_array_mut().unwrap().push(json!({"api-key": "second", "models": [], "headers": {}}));
            let (out, cfg, rewritten) =
                apply(source, json!({"claude-api-key": keys, "port": 9000}).as_object().unwrap()).unwrap();
            assert!(rewritten);
            assert_eq!(cfg.port, 9000);
            assert_eq!(cfg.claude_api_key.len(), 2);
            assert_eq!(Config::parse(&out).unwrap().claude_api_key[1].api_key, "second");
        }
    }

    #[test]
    fn smart_quota_round_trips_in_both_config_layouts() {
        for source in
            ["routing: least-used\n", "config-version: 8\nrouting:\n  strategy: least-used\n  session-affinity: true\n"]
        {
            let (out, cfg) = edit(source, json!({"routing": "smart-quota", "five-hour-reserve-percent": 40}));
            assert_eq!(cfg.routing, crate::config::Routing::SmartQuota);
            assert_eq!(cfg.five_hour_reserve_percent, 40);
            assert_eq!(Config::parse(&out).unwrap().five_hour_reserve_percent, 40);
            if source.contains("config-version") {
                assert_eq!(yaml(&out).unwrap()["routing"]["five-hour-reserve-percent"].as_u64(), Some(40));
            }
            assert_eq!(Config::parse(&out).unwrap().routing, cfg.routing);
            assert_eq!(values(&out).unwrap()["routing"], "smart-quota");
            assert!(cfg.session_affinity);
        }
    }

    #[test]
    fn smart_quota_alias_defaults_and_reserve_validation() {
        assert_eq!(Config::default().five_hour_reserve_percent, 30);
        for source in ["routing: soonest-reset\n", "routing:\n  strategy: soonest-reset\n"] {
            let cfg = Config::parse(source).unwrap();
            assert_eq!(cfg.routing, crate::config::Routing::SmartQuota);
            assert_eq!(values(source).unwrap()["routing"], "smart-quota");
        }
        for invalid in [json!(-1), json!(101), json!(30.5), json!("30"), Value::Null] {
            assert!(apply("", json!({"five-hour-reserve-percent": invalid}).as_object().unwrap()).is_err());
            assert!(Config::parse(&format!("five-hour-reserve-percent: {invalid}\n")).is_err());
            assert!(
                Config::parse(&format!("routing:\n  strategy: smart-quota\n  five-hour-reserve-percent: {invalid}\n"))
                    .is_err()
            );
        }
        for value in [0, 30, 100] {
            let (_, cfg) = edit("", json!({"five-hour-reserve-percent": value}));
            assert_eq!(cfg.five_hour_reserve_percent, value);
        }
    }

    #[test]
    fn session_affinity_settings_land_where_each_layout_keeps_them() {
        let (out, cfg) =
            edit("port: 8317\n", json!({"session-affinity": false, "session-affinity-idle-seconds": 3600}));
        assert!(!cfg.session_affinity);
        assert_eq!(cfg.session_affinity_idle_seconds, 3600);
        assert_eq!(yaml(&out).unwrap()["session-affinity"].as_bool(), Some(false));
        let (out, cfg) =
            edit("config-version: 8\nrouting:\n  strategy: fill-first\n", json!({"session-affinity": false}));
        assert!(!cfg.session_affinity);
        assert_eq!(yaml(&out).unwrap()["routing"]["session-affinity"].as_bool(), Some(false));
        assert!(apply("", json!({"session-affinity-idle-seconds": 5}).as_object().unwrap()).is_err());
    }

    #[test]
    fn edits_legacy_and_nested_layouts_without_dropping_comments() {
        for source in [
            "# heading\nport: 8317 # port note\nrouting: least-used\ncustom: { untouched: yes }\n",
            "# heading\nconfig-version: 8\nserver:\n  port: 8317 # port note\nrouting:\n  strategy: least-used\n  session-affinity: true\ncustom: { untouched: yes }\n",
        ] {
            let (out, cfg) = edit(source, json!({"port": 9000, "routing": "fill-first", "request-retry": 5}));
            assert_eq!(cfg.port, 9000);
            assert_eq!(cfg.request_retry, 5);
            assert!(out.contains("# heading"));
            assert!(out.contains("# port note"));
            assert!(out.contains("custom: { untouched: yes }"));
            if source.contains("config-version") {
                assert!(out.contains("session-affinity: true"));
                assert_eq!(yaml(&out).unwrap()["server"]["port"].as_u64(), Some(9000));
            }
        }
    }

    #[test]
    fn grouped_keys_retain_inheritance_and_unknown_settings() {
        let source = "config-version: 8\napi-keys:\n  claude:\n    - name: shared\n      base-url: https://example.com\n      custom: keep\n      keys:\n        - api-key: first # first key\n          weight: 8\n        - api-key: second # second key\n          weight: 9\n";
        let mut keys = values(source).unwrap()["claude-api-key"].clone();
        keys[0]["api-key"] = json!("replacement");
        keys[0]["prefix"] = json!("team");
        let (out, cfg) = edit(source, json!({"claude-api-key": keys}));
        assert_eq!(cfg.claude_api_key[0].api_key, "replacement");
        assert_eq!(cfg.claude_api_key[0].base_url.as_deref(), Some("https://example.com"));
        assert!(out.contains("weight: 8"));
        assert!(out.contains("custom: keep"));
        assert!(out.contains("# first key"));
        let mut keys = values(&out).unwrap()["claude-api-key"].clone();
        keys.as_array_mut().unwrap().remove(0);
        let (out, cfg) = edit(&out, json!({"claude-api-key": keys}));
        assert_eq!(cfg.claude_api_key.len(), 1);
        assert_eq!(cfg.claude_api_key[0].api_key, "second");
        assert!(out.contains("# second key"));
        assert!(out.contains("weight: 9"));
    }

    #[test]
    fn compatible_keys_keep_per_key_properties() {
        let source = "openai-compatibility:\n  - name: local\n    base-url: https://example.com/v1\n    api-key-entries:\n      - api-key: first\n        proxy-url: socks5://localhost:1080\n        weight: 9\n      - api-key: second\n        weight: 3\n    models: []\n";
        let mut entries = values(source).unwrap()["openai-compatibility"].clone();
        entries[0]["api-keys"] = json!(["replacement", "second"]);
        let (out, cfg) = edit(source, json!({"openai-compatibility": entries}));
        assert_eq!(cfg.openai_compatibility[0].api_keys, ["replacement", "second"]);
        assert!(out.contains("weight: 9"));
        assert!(out.contains("proxy-url: socks5://localhost:1080"));
    }

    #[test]
    fn adding_and_clearing_nested_settings_round_trips() {
        let source = "# Empty v8 config\nconfig-version: 8\n";
        let (out, cfg) = edit(
            source,
            json!({"api-keys": ["client"], "force-model-prefix": true, "claude-cloak": false,
            "oauth-model-alias": {"claude": [{"name": "original", "alias": "friendly", "fork": true}]},
            "claude-api-key": [{"api-key": "upstream", "models": [], "headers": {"X-Test": "true"}}],
            "openai-compatibility": [{"name": "local", "base-url": "http://localhost:11434/v1", "api-keys": [], "models": []}]}),
        );
        assert_eq!(cfg.api_keys, ["client"]);
        assert!(cfg.force_model_prefix);
        assert!(!cfg.claude_cloak);
        assert_eq!(cfg.claude_api_key[0].headers["X-Test"], "true");
        assert_eq!(cfg.openai_compatibility.len(), 1);
        let (out, cfg) = edit(&out, json!({"api-keys": [], "claude-api-key": [], "oauth-model-alias": {}}));
        assert!(cfg.api_keys.is_empty());
        assert!(cfg.claude_api_key.is_empty());
        assert!(cfg.oauth_model_alias.is_empty());
        assert!(out.contains("# Empty v8 config"));
    }

    #[test]
    fn rejects_invalid_fields_and_forged_provider_ids() {
        for change in [
            json!({"port": 70000}),
            json!({"request-retry": -1}),
            json!({"routing": "unknown"}),
            json!({"proxy-url": "file:///tmp/file"}),
            json!({"tls": {"enable": true}}),
            json!({"unknown": true}),
            json!({"claude-api-key": [{"api-key": "x", "_id": ["management-key"]}]}),
        ] {
            assert!(apply("", change.as_object().unwrap()).is_err());
        }
    }

    #[test]
    fn restart_status_compares_with_startup_even_after_repeated_saves() {
        let startup = Config::default();
        let mut current = startup.clone();
        current.port = 9000;
        current.tls.enable = true;
        assert_eq!(restart_fields(&startup, &current), ["port", "HTTPS"]);
        current.debug = true;
        assert!(restart_fields(&startup, &current).contains(&"debug logging"));
        assert!(restart_fields(&startup, &startup).is_empty());
    }

    #[test]
    fn inherited_options_can_be_cleared_and_model_properties_survive() {
        let source = "config-version: 8\napi-keys:\n  claude:\n    - base-url: https://example.com\n      prefix: team\n      models:\n        - name: upstream\n          alias: old\n          vendor-option: retained\n      keys:\n        - api-key: first\n        - api-key: second\n";
        let mut keys = values(source).unwrap()["claude-api-key"].clone();
        keys[0]["base-url"] = Value::Null;
        keys[0]["prefix"] = Value::Null;
        keys[0]["models"][0]["alias"] = json!("new");
        let (out, cfg) = edit(source, json!({"claude-api-key": keys}));
        assert_eq!(cfg.claude_api_key[0].base_url.as_deref(), Some(""));
        assert_eq!(cfg.claude_api_key[0].prefix.as_deref(), Some(""));
        assert_eq!(cfg.claude_api_key[0].models[0].alias.as_deref(), Some("new"));
        assert_eq!(cfg.claude_api_key[1].models[0].alias.as_deref(), Some("old"));
        let doc = yaml(&out).unwrap();
        assert_eq!(doc["api-keys"]["claude"][0]["keys"][0]["models"][0]["vendor-option"].as_str(), Some("retained"));
    }

    #[test]
    fn template_comments_survive_and_noop_does_not_add_defaults() {
        let source = crate::config::template();
        let (out, cfg) =
            edit(&source, json!({"request-retry": 7, "tls": {"enable": false, "cert": "cert.pem", "key": "key.pem"}}));
        assert_eq!(cfg.request_retry, 7);
        assert!(out.contains("# API keys (optional)."));
        assert!(out.contains("# Keys your clients must send."));
        let (out, _) = edit("# Minimal\nport: 8317\n", json!({"request-retry": 3}));
        assert_eq!(out, "# Minimal\nport: 8317\n");
    }

    #[test]
    fn ancient_gemini_keys_can_be_edited_with_the_gui() {
        let source = "# Legacy Gemini keys\ngenerative-language-api-key: [first, second]\n";
        let mut keys = values(source).unwrap()["gemini-api-key"].clone();
        keys[0]["prefix"] = json!("team");
        keys[1]["api-key"] = json!("replacement");
        let (out, cfg) = edit(source, json!({"gemini-api-key": keys}));
        assert_eq!(cfg.gemini_api_key[0].prefix.as_deref(), Some("team"));
        assert_eq!(cfg.gemini_api_key[1].api_key, "replacement");
        assert!(out.contains("# Legacy Gemini keys"));
        assert!(yaml(&out).unwrap()["generative-language-api-key"].is_null());
    }

    #[test]
    fn last_header_and_model_can_be_removed_from_block_collections() {
        let source = "claude-api-key:\n  - api-key: upstream\n    headers: # keep header note\n      X-Test: example\n    models:\n      - name: original\n";
        let mut keys = values(source).unwrap()["claude-api-key"].clone();
        keys[0]["headers"] = json!({});
        keys[0]["models"] = json!([]);
        let (out, cfg) = edit(source, json!({"claude-api-key": keys}));
        assert!(cfg.claude_api_key[0].headers.is_empty());
        assert!(cfg.claude_api_key[0].models.is_empty());
        assert!(out.contains("# keep header note"));
    }

    #[test]
    fn model_rows_keep_their_hidden_properties_when_other_rows_are_removed() {
        let source = "claude-api-key:\n  - api-key: upstream\n    models:\n      - name: first\n        vendor-option: one\n      - name: second\n        vendor-option: two\n";
        let mut keys = values(source).unwrap()["claude-api-key"].clone();
        keys[0]["models"].as_array_mut().unwrap().remove(0);
        keys[0]["models"][0]["name"] = json!("renamed");
        keys[0]["models"].as_array_mut().unwrap().push(json!({"name": "new"}));
        let (out, cfg) = edit(source, json!({"claude-api-key": keys}));
        assert_eq!(cfg.claude_api_key[0].models[0].name, "renamed");
        let doc = yaml(&out).unwrap();
        assert_eq!(doc["claude-api-key"][0]["models"][0]["vendor-option"].as_str(), Some("two"));
        assert!(doc["claude-api-key"][0]["models"][1]["vendor-option"].is_null());
        assert!(!out.contains("_row"));
    }
}
