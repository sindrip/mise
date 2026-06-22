//! macOS user defaults (preferences) for the `[bootstrap.macos.defaults]` config section.
//!
//! Current state is read with `defaults export` (plist serde) and changes are
//! written with `defaults export` → merge → `defaults import`. Like
//! `[bootstrap.packages]` they are machine-global, declarative, and only ever
//! applied when explicitly requested with `mise bootstrap macos-defaults apply`
//! or `mise bootstrap`.

use std::process::Stdio;

use indexmap::IndexMap;
use itertools::Itertools;

use crate::result::Result;

/// A single `[bootstrap.macos.defaults.<domain>]` entry: `key = value`
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultsRequest {
    /// preferences domain, e.g. "com.apple.dock" or "NSGlobalDomain"
    pub domain: String,
    pub key: String,
    pub value: DefaultsValue,
}

impl std::fmt::Display for DefaultsRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} = {}", self.domain, self.key, self.value)
    }
}

/// Supported plist types for `defaults export`/`defaults import`.
/// Unsupported plist types (date, data) fail at the `TryFrom` boundary.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum DefaultsValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<DefaultsValue>),
    Dict(IndexMap<String, DefaultsValue>),
}

impl TryFrom<&toml::Value> for DefaultsValue {
    type Error = plist::Error;

    fn try_from(value: &toml::Value) -> std::result::Result<Self, Self::Error> {
        let pv = plist::to_value(value)?;
        plist::from_value(&pv)
    }
}

impl DefaultsValue {
    /// RFC 7396 JSON Merge Patch semantics: dicts merge recursively,
    /// everything else (including arrays) is replaced.
    pub fn merge(&mut self, patch: DefaultsValue) {
        match (self, patch) {
            (Self::Dict(current), Self::Dict(patch)) => {
                for (key, value) in patch {
                    match current.get_mut(&key) {
                        Some(existing) => existing.merge(value),
                        None => {
                            current.insert(key, value);
                        }
                    }
                }
            }
            (this, patch) => {
                *this = patch;
            }
        }
    }
}

impl std::fmt::Display for DefaultsValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(i) => write!(f, "{i}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Str(s) => write!(f, "{s}"),
            Self::Array(arr) => write!(f, "[{}]", arr.iter().join(", ")),
            Self::Dict(dict) => write!(
                f,
                "{{{}}}",
                dict.iter().map(|(k, v)| format!("{k} = {v}")).join(", ")
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DefaultsState {
    /// current value matches the config
    Set,
    /// a value exists but differs from the config (in value or type)
    Differs { current: DefaultsValue },
    /// the key is not set in this domain
    Unset,
}

#[derive(Debug, Clone)]
pub struct DefaultsStatus {
    pub request: DefaultsRequest,
    pub state: DefaultsState,
}

pub fn is_available() -> bool {
    cfg!(target_os = "macos") && crate::file::which("defaults").is_some()
}

pub fn unavailable_reason() -> String {
    if cfg!(target_os = "macos") {
        "`defaults` not found".to_string()
    } else {
        "only available on macos".to_string()
    }
}

/// Query the current state of each entry. Side-effect free.
pub async fn status(requests: &[DefaultsRequest]) -> Result<Vec<DefaultsStatus>> {
    let mut out = vec![];
    for req in requests {
        let current = export_domain(&req.domain)
            .await?
            .and_then(|map| map.get(&req.key).cloned());

        let state = match current {
            None => DefaultsState::Unset,
            Some(current) => {
                let mut merged = current.clone();
                merged.merge(req.value.clone());
                if merged == current {
                    DefaultsState::Set
                } else {
                    DefaultsState::Differs { current }
                }
            }
        };

        out.push(DefaultsStatus {
            request: req.clone(),
            state,
        });
    }
    Ok(out)
}

/// Write the given entries (already filtered to unset/differing ones).
/// Uses `defaults export` → merge → `defaults import` per request.
/// The read-modify-write cycle is non-atomic; the race window is
/// negligible for an interactive bootstrap tool.
pub async fn apply(requests: &[DefaultsRequest], dry_run: bool) -> Result<()> {
    for req in requests {
        let current = export_domain(&req.domain).await?;

        if dry_run {
            let display_current = current
                .as_ref()
                .and_then(|m| m.get(&req.key))
                .map_or("unset".to_string(), |v| v.to_string());

            miseprintln!(
                "{} {}: {} → {}",
                req.domain,
                req.key,
                display_current,
                req.value
            );

            continue;
        }
        let mut merged = current.unwrap_or_default();
        match merged.get_mut(&req.key) {
            Some(existing) => existing.merge(req.value.clone()),
            None => {
                merged.insert(req.key.clone(), req.value.clone());
            }
        }
        debug!("defaults import {} (setting {})", req.domain, req.key);
        import_domain(&req.domain, &merged).await?;
    }
    Ok(())
}

/// Export all keys for a domain via `defaults export <domain> -`.
/// Returns `None` when the domain does not exist.
async fn export_domain(domain: &str) -> Result<Option<IndexMap<String, DefaultsValue>>> {
    debug!("$ defaults export {domain} -");
    let output = tokio::process::Command::new("defaults")
        .args(["export", domain, "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not exist") {
            return Ok(None);
        }
        eyre::bail!("`defaults export {domain} -` failed: {}", stderr.trim());
    }

    let map = plist::from_reader(std::io::Cursor::new(&output.stdout))?;
    Ok(Some(map))
}

/// Import a full domain via `defaults import <domain> -`.
async fn import_domain(domain: &str, entries: &IndexMap<String, DefaultsValue>) -> Result<()> {
    let mut plist_bytes = vec![];
    plist::to_writer_xml(&mut plist_bytes, entries)?;
    let mut child = tokio::process::Command::new("defaults")
        .args(["import", domain, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        tokio::io::AsyncWriteExt::write_all(&mut stdin, &plist_bytes).await?;
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        eyre::bail!(
            "`defaults import {domain} -` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(s: &str) -> toml::Value {
        s.parse().unwrap()
    }

    #[test]
    fn test_try_from_toml() {
        assert_eq!(
            DefaultsValue::try_from(&val("true")).unwrap(),
            DefaultsValue::Bool(true)
        );
        assert_eq!(
            DefaultsValue::try_from(&val("48")).unwrap(),
            DefaultsValue::Int(48)
        );
        assert_eq!(
            DefaultsValue::try_from(&val("1.5")).unwrap(),
            DefaultsValue::Float(1.5)
        );
        assert_eq!(
            DefaultsValue::try_from(&val(r#""right""#)).unwrap(),
            DefaultsValue::Str("right".into())
        );
        assert_eq!(
            DefaultsValue::try_from(&val("[1, 2]")).unwrap(),
            DefaultsValue::Array(vec![DefaultsValue::Int(1), DefaultsValue::Int(2)])
        );
        assert_eq!(
            DefaultsValue::try_from(&val("{ a = 1 }")).unwrap(),
            DefaultsValue::Dict(IndexMap::from([("a".into(), DefaultsValue::Int(1))]))
        );
    }

    #[test]
    fn test_deep_merge_dict() {
        let mut current = DefaultsValue::Dict(IndexMap::from([
            ("a".into(), DefaultsValue::Int(1)),
            ("b".into(), DefaultsValue::Int(2)),
            ("c".into(), DefaultsValue::Int(3)),
            (
                "nested".into(),
                DefaultsValue::Dict(IndexMap::from([
                    ("x".into(), DefaultsValue::Int(1)),
                    ("y".into(), DefaultsValue::Int(2)),
                ])),
            ),
        ]));
        let patch = DefaultsValue::Dict(IndexMap::from([
            ("a".into(), DefaultsValue::Int(99)),
            (
                "nested".into(),
                DefaultsValue::Dict(IndexMap::from([("x".into(), DefaultsValue::Int(99))])),
            ),
        ]));
        current.merge(patch);
        assert_eq!(
            current,
            DefaultsValue::Dict(IndexMap::from([
                ("a".into(), DefaultsValue::Int(99)),
                ("b".into(), DefaultsValue::Int(2)),
                ("c".into(), DefaultsValue::Int(3)),
                (
                    "nested".into(),
                    DefaultsValue::Dict(IndexMap::from([
                        ("x".into(), DefaultsValue::Int(99)),
                        ("y".into(), DefaultsValue::Int(2)),
                    ])),
                ),
            ]))
        );
    }

    #[test]
    fn test_comparison() {
        let current = DefaultsValue::Dict(IndexMap::from([
            ("a".into(), DefaultsValue::Int(1)),
            ("b".into(), DefaultsValue::Int(2)),
            ("c".into(), DefaultsValue::Int(3)),
        ]));

        // partial config — all specified keys match
        let desired = DefaultsValue::Dict(IndexMap::from([("a".into(), DefaultsValue::Int(1))]));
        let mut merged = current.clone();
        merged.merge(desired);
        assert_eq!(merged, current, "extra keys should not cause a diff");

        // partial config — one key differs
        let desired =
            DefaultsValue::Dict(IndexMap::from([("a".into(), DefaultsValue::Int(99))]));
        let mut merged = current.clone();
        merged.merge(desired);
        assert_ne!(merged, current, "changed key should cause a diff");
    }

    #[test]
    fn test_merge_array_replaces() {
        let mut current = DefaultsValue::Array(vec![
            DefaultsValue::Int(1),
            DefaultsValue::Int(2),
            DefaultsValue::Int(3),
        ]);
        let patch = DefaultsValue::Array(vec![DefaultsValue::Int(4), DefaultsValue::Int(5)]);
        current.merge(patch);
        assert_eq!(
            current,
            DefaultsValue::Array(vec![DefaultsValue::Int(4), DefaultsValue::Int(5),])
        );
    }
}
