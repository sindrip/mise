//! macOS user defaults (preferences) for the `[bootstrap.macos.defaults]` config section.
//!
//! Values are applied via `defaults export`/`defaults import` round-trips:
//! export the current plist, deep-merge declared values, import back.
//! Like `[bootstrap.packages]` they are machine-global, declarative, and
//! only ever applied when explicitly requested with
//! `mise bootstrap macos-defaults apply` or `mise bootstrap`.

use std::io::Cursor;
use std::process::Stdio;

use indexmap::IndexMap;

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

/// Supported TOML value types. Maps to plist types for export/import.
#[derive(Debug, Clone, PartialEq)]
pub enum DefaultsValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Dict(IndexMap<String, DefaultsValue>),
    Array(Vec<DefaultsValue>),
}

impl DefaultsValue {
    pub fn from_toml(value: &toml::Value) -> Option<Self> {
        match value {
            toml::Value::Boolean(b) => Some(Self::Bool(*b)),
            toml::Value::Integer(i) => Some(Self::Int(*i)),
            toml::Value::Float(f) => Some(Self::Float(*f)),
            toml::Value::String(s) => Some(Self::Str(s.clone())),
            toml::Value::Table(t) => {
                let mut map = IndexMap::new();
                for (k, v) in t {
                    map.insert(k.clone(), Self::from_toml(v)?);
                }
                Some(Self::Dict(map))
            }
            toml::Value::Array(a) => {
                let items: Option<Vec<_>> = a.iter().map(Self::from_toml).collect();
                Some(Self::Array(items?))
            }
            _ => None,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Bool(b) => (*b).into(),
            Self::Int(i) => (*i).into(),
            Self::Float(f) => (*f).into(),
            Self::Str(s) => s.clone().into(),
            Self::Dict(map) => {
                let obj: serde_json::Map<String, serde_json::Value> =
                    map.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
                serde_json::Value::Object(obj)
            }
            Self::Array(items) => {
                serde_json::Value::Array(items.iter().map(|v| v.to_json()).collect())
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
            Self::Dict(_) => write!(f, "{{...}}"),
            Self::Array(_) => write!(f, "[...]"),
        }
    }
}

impl DefaultsValue {
    pub fn to_plist(&self) -> plist::Value {
        match self {
            Self::Bool(b) => plist::Value::Boolean(*b),
            Self::Int(i) => plist::Value::Integer((*i).into()),
            Self::Float(f) => plist::Value::Real(*f),
            Self::Str(s) => plist::Value::String(s.clone()),
            Self::Dict(map) => {
                let mut dict = plist::Dictionary::new();
                for (k, v) in map {
                    dict.insert(k.clone(), v.to_plist());
                }
                plist::Value::Dictionary(dict)
            }
            Self::Array(items) => {
                plist::Value::Array(items.iter().map(|v| v.to_plist()).collect())
            }
        }
    }

    pub fn from_plist(value: &plist::Value) -> Option<Self> {
        match value {
            plist::Value::Boolean(b) => Some(Self::Bool(*b)),
            plist::Value::Integer(i) => i.as_signed().map(Self::Int),
            plist::Value::Real(f) => Some(Self::Float(*f)),
            plist::Value::String(s) => Some(Self::Str(s.clone())),
            plist::Value::Dictionary(dict) => {
                let mut map = IndexMap::new();
                for (k, v) in dict {
                    map.insert(k.clone(), Self::from_plist(v)?);
                }
                Some(Self::Dict(map))
            }
            plist::Value::Array(items) => {
                let vals: Option<Vec<_>> = items.iter().map(Self::from_plist).collect();
                Some(Self::Array(vals?))
            }
            _ => None,
        }
    }
}

fn deep_merge_plist(base: &mut plist::Dictionary, overlay: &plist::Dictionary) {
    for (key, overlay_val) in overlay {
        match (base.get_mut(key), overlay_val) {
            (Some(plist::Value::Dictionary(base_dict)), plist::Value::Dictionary(overlay_dict)) => {
                deep_merge_plist(base_dict, overlay_dict);
            }
            _ => {
                base.insert(key.clone(), overlay_val.clone());
            }
        }
    }
}

async fn export_domain(domain: &str) -> Result<Option<plist::Dictionary>> {
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
    match plist::Value::from_reader_xml(Cursor::new(&output.stdout))? {
        plist::Value::Dictionary(dict) => Ok(Some(dict)),
        _ => eyre::bail!("`defaults export {domain} -` did not return a dictionary"),
    }
}

async fn import_domain(domain: &str, dict: &plist::Dictionary) -> Result<()> {
    let mut xml = vec![];
    plist::to_writer_xml(&mut xml, &plist::Value::Dictionary(dict.clone()))?;
    let mut child = tokio::process::Command::new("defaults")
        .args(["import", domain, "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        tokio::io::AsyncWriteExt::write_all(&mut stdin, &xml).await?;
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

/// Check if declared values are a subset of the current plist.
/// For dicts, only declared keys are compared (undeclared keys are ignored).
/// For scalars and arrays, values must match exactly.
fn plist_contains(current: &plist::Value, declared: &plist::Value) -> bool {
    match (current, declared) {
        (plist::Value::Dictionary(cur), plist::Value::Dictionary(decl)) => {
            decl.iter()
                .all(|(k, v)| cur.get(k).is_some_and(|cv| plist_contains(cv, v)))
        }
        _ => current == declared,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DefaultsState {
    /// current value matches the config
    Set,
    /// a value exists but differs from the config (in value or type)
    Differs { current: String },
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

    let mut domain_exports: IndexMap<String, Option<plist::Dictionary>> = IndexMap::new();
    for req in requests {
        if !domain_exports.contains_key(&req.domain) {
            domain_exports.insert(req.domain.clone(), export_domain(&req.domain).await?);
        }
    }

    for req in requests {
        let exported = domain_exports.get(&req.domain).and_then(|d| d.as_ref());
        let state = match exported.and_then(|dict| dict.get(&req.key)) {
            Some(current_plist) => {
                let declared_plist = req.value.to_plist();
                if plist_contains(current_plist, &declared_plist) {
                    DefaultsState::Set
                } else {
                    let current = DefaultsValue::from_plist(current_plist)
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "(unsupported plist type)".to_string());
                    DefaultsState::Differs { current }
                }
            }
            None => DefaultsState::Unset,
        };
        out.push(DefaultsStatus {
            request: req.clone(),
            state,
        });
    }
    Ok(out)
}

/// Write the given entries (already filtered to unset/differing ones)
pub async fn apply(requests: &[DefaultsRequest], dry_run: bool) -> Result<()> {
    let mut by_domain: IndexMap<String, Vec<&DefaultsRequest>> = IndexMap::new();
    for req in requests {
        by_domain.entry(req.domain.clone()).or_default().push(req);
    }

    for (domain, reqs) in &by_domain {
        if dry_run {
            let keys: Vec<_> = reqs.iter().map(|r| format!("{} = {}", r.key, r.value)).collect();
            miseprintln!("defaults import {domain} (merge {})", keys.join(", "));
            continue;
        }
        let mut dict = export_domain(domain).await?.unwrap_or_default();
        for req in reqs {
            let plist_val = req.value.to_plist();
            match (&plist_val, dict.get_mut(&req.key)) {
                (
                    plist::Value::Dictionary(overlay),
                    Some(plist::Value::Dictionary(existing)),
                ) => {
                    deep_merge_plist(existing, overlay);
                }
                _ => {
                    dict.insert(req.key.clone(), plist_val);
                }
            }
        }
        debug!("$ defaults import {domain} -");
        import_domain(domain, &dict).await?;
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
    fn test_from_toml() {
        assert_eq!(
            DefaultsValue::from_toml(&val("true")),
            Some(DefaultsValue::Bool(true))
        );
        assert_eq!(
            DefaultsValue::from_toml(&val("48")),
            Some(DefaultsValue::Int(48))
        );
        assert_eq!(
            DefaultsValue::from_toml(&val("1.5")),
            Some(DefaultsValue::Float(1.5))
        );
        assert_eq!(
            DefaultsValue::from_toml(&val(r#""right""#)),
            Some(DefaultsValue::Str("right".into()))
        );

        assert_eq!(
            DefaultsValue::from_toml(&val("[1, 2]")),
            Some(DefaultsValue::Array(vec![
                DefaultsValue::Int(1),
                DefaultsValue::Int(2),
            ]))
        );
        assert_eq!(
            DefaultsValue::from_toml(&val("{ a = 1 }")),
            Some(DefaultsValue::Dict(IndexMap::from([(
                "a".to_string(),
                DefaultsValue::Int(1),
            )])))
        );
    }

    #[test]
    fn test_from_toml_nested() {
        let toml: toml::Value = toml::from_str(
            r#"
            [inner]
            enabled = false
            "#,
        )
        .unwrap();
        let val = DefaultsValue::from_toml(&toml).unwrap();
        assert_eq!(
            val,
            DefaultsValue::Dict(IndexMap::from([(
                "inner".to_string(),
                DefaultsValue::Dict(IndexMap::from([(
                    "enabled".to_string(),
                    DefaultsValue::Bool(false),
                )])),
            )]))
        );
    }

    #[test]
    fn test_plist_round_trip() {
        let val = DefaultsValue::Dict(IndexMap::from([
            ("enabled".to_string(), DefaultsValue::Bool(false)),
            (
                "value".to_string(),
                DefaultsValue::Dict(IndexMap::from([
                    (
                        "parameters".to_string(),
                        DefaultsValue::Array(vec![
                            DefaultsValue::Int(32),
                            DefaultsValue::Int(49),
                            DefaultsValue::Int(262144),
                        ]),
                    ),
                    ("type".to_string(), DefaultsValue::Str("standard".into())),
                ])),
            ),
        ]));
        let plist_val = val.to_plist();
        let round_tripped = DefaultsValue::from_plist(&plist_val).unwrap();
        assert_eq!(val, round_tripped);
    }

    #[test]
    fn test_deep_merge_plist() {
        let mut base = plist::Dictionary::new();
        base.insert("keep".into(), plist::Value::String("original".into()));
        let mut inner = plist::Dictionary::new();
        inner.insert("a".into(), plist::Value::Integer(1.into()));
        inner.insert("b".into(), plist::Value::Integer(2.into()));
        base.insert("nested".into(), plist::Value::Dictionary(inner));

        let mut overlay = plist::Dictionary::new();
        overlay.insert("new_key".into(), plist::Value::Boolean(true));
        let mut inner_overlay = plist::Dictionary::new();
        inner_overlay.insert("b".into(), plist::Value::Integer(99.into()));
        inner_overlay.insert("c".into(), plist::Value::Integer(3.into()));
        overlay.insert("nested".into(), plist::Value::Dictionary(inner_overlay));

        deep_merge_plist(&mut base, &overlay);

        // preserves unmentioned keys
        assert_eq!(
            base.get("keep"),
            Some(&plist::Value::String("original".into()))
        );
        // adds new keys
        assert_eq!(base.get("new_key"), Some(&plist::Value::Boolean(true)));
        // recurses into nested dicts
        let nested = match base.get("nested") {
            Some(plist::Value::Dictionary(d)) => d,
            _ => panic!("expected dict"),
        };
        assert_eq!(nested.get("a"), Some(&plist::Value::Integer(1.into()))); // preserved
        assert_eq!(nested.get("b"), Some(&plist::Value::Integer(99.into()))); // replaced
        assert_eq!(nested.get("c"), Some(&plist::Value::Integer(3.into()))); // added
    }
}
