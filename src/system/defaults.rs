//! macOS user defaults (preferences) for the `[bootstrap.macos.defaults]` config section.
//!
//! Entries are written with `defaults write <domain> <key> <-type> <value>`
//! and checked with `defaults read-type`/`defaults read`. Like
//! `[bootstrap.packages]` they are machine-global, declarative, and only ever
//! applied when explicitly requested with `mise bootstrap macos-defaults apply`
//! or `mise bootstrap`.

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

/// The value types `defaults write` can set and mise can verify. Other plist
/// types (arrays, dicts, dates, data) are not supported — config entries with
/// those TOML types warn and are skipped.
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

    /// type+value arguments for `defaults write <domain> <key> ...`
    /// Returns `None` for nested values (Dict/Array) which use the
    /// export/import path instead.
    pub fn write_args(&self) -> Option<Vec<String>> {
        match self {
            Self::Bool(b) => Some(vec!["-bool".into(), b.to_string()]),
            Self::Int(i) => Some(vec!["-int".into(), i.to_string()]),
            Self::Float(f) => Some(vec!["-float".into(), f.to_string()]),
            Self::Str(s) => Some(vec!["-string".into(), s.clone()]),
            Self::Dict(_) | Self::Array(_) => None,
        }
    }

    pub fn is_nested(&self) -> bool {
        matches!(self, Self::Dict(_) | Self::Array(_))
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

    /// Does the pair from `defaults read-type` ("boolean", "integer", ...)
    /// and `defaults read` (raw value; booleans print as 1/0) match this
    /// value? Types are compared strictly: an integer 1 does not satisfy a
    /// configured `true` — `mise bootstrap macos-defaults apply` converges it to the typed
    /// value.
    fn matches(&self, read_type: &str, raw: &str) -> bool {
        match self {
            Self::Bool(b) => read_type == "boolean" && raw == if *b { "1" } else { "0" },
            Self::Int(i) => read_type == "integer" && raw.parse::<i64>() == Ok(*i),
            Self::Float(f) => {
                read_type == "float" && raw.parse::<f64>().is_ok_and(|v| (v - f).abs() < 1e-9)
            }
            Self::Str(s) => read_type == "string" && raw == s,
            Self::Dict(_) | Self::Array(_) => false,
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

    let has_nested: IndexMap<&str, bool> = {
        let mut m = IndexMap::new();
        for req in requests {
            let entry = m.entry(req.domain.as_str()).or_insert(false);
            if req.value.is_nested() {
                *entry = true;
            }
        }
        m
    };

    let mut domain_exports: IndexMap<String, Option<plist::Dictionary>> = IndexMap::new();
    for (domain, nested) in &has_nested {
        if *nested {
            domain_exports.insert(domain.to_string(), export_domain(domain).await?);
        }
    }

    for req in requests {
        let state = if has_nested.get(req.domain.as_str()).copied().unwrap_or(false) {
            let exported = domain_exports.get(&req.domain).and_then(|d| d.as_ref());
            match exported.and_then(|dict| dict.get(&req.key)) {
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
            }
        } else {
            match read(&req.domain, &req.key).await? {
                Some((read_type, raw)) => {
                    if req.value.matches(&read_type, &raw) {
                        DefaultsState::Set
                    } else {
                        let current = if raw == req.value.to_string() {
                            format!("{raw} ({read_type})")
                        } else {
                            raw
                        };
                        DefaultsState::Differs { current }
                    }
                }
                None => DefaultsState::Unset,
            }
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
    let has_nested: IndexMap<&str, bool> = {
        let mut m = IndexMap::new();
        for req in requests {
            let entry = m.entry(req.domain.as_str()).or_insert(false);
            if req.value.is_nested() {
                *entry = true;
            }
        }
        m
    };

    // Domains with any nested values use export/merge/import
    let mut nested_domains: IndexMap<String, Vec<&DefaultsRequest>> = IndexMap::new();

    for req in requests {
        if has_nested.get(req.domain.as_str()).copied().unwrap_or(false) {
            nested_domains
                .entry(req.domain.clone())
                .or_default()
                .push(req);
            continue;
        }
        let args = req.value.write_args().expect("scalar value has write_args");
        let mut cmd_args = vec!["write".to_string(), req.domain.clone(), req.key.clone()];
        cmd_args.extend(args);
        let display = shell_words::join(&cmd_args);
        if dry_run {
            miseprintln!("defaults {display}");
            continue;
        }
        debug!("$ defaults {display}");
        let output = tokio::process::Command::new("defaults")
            .args(&cmd_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        if !output.status.success() {
            eyre::bail!(
                "`defaults {display}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    for (domain, reqs) in &nested_domains {
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
        if dry_run {
            let keys: Vec<_> = reqs.iter().map(|r| format!("{} = {}", r.key, r.value)).collect();
            miseprintln!("defaults import {domain} (merge {})", keys.join(", "));
            continue;
        }
        debug!("$ defaults import {domain} -");
        import_domain(domain, &dict).await?;
    }
    Ok(())
}

/// `defaults read-type` + `defaults read` for one key. Returns
/// `(type, raw value)`, or None when the key (or domain) does not exist —
/// both commands exit non-zero for that, which is not an error here.
async fn read(domain: &str, key: &str) -> Result<Option<(String, String)>> {
    let Some(read_type) = defaults_cmd(&["read-type", domain, key]).await? else {
        return Ok(None);
    };
    // "Type is boolean" -> "boolean"
    let read_type = read_type
        .strip_prefix("Type is ")
        .unwrap_or(&read_type)
        .to_string();
    let Some(raw) = defaults_cmd(&["read", domain, key]).await? else {
        return Ok(None);
    };
    Ok(Some((read_type, raw)))
}

async fn defaults_cmd(args: &[&str]) -> Result<Option<String>> {
    debug!("$ defaults {}", shell_words::join(args));
    let output = tokio::process::Command::new("defaults")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !output.status.success() {
        // "does not exist" is the expected missing-key/-domain answer; any
        // other failure (cfprefsd unavailable, managed domain, ...) must not
        // masquerade as Unset
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not exist") {
            return Ok(None);
        }
        eyre::bail!(
            "`defaults {}` failed: {}",
            shell_words::join(args),
            stderr.trim()
        );
    }
    // strip only the trailing newline — leading/trailing spaces can be
    // significant in string values
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(Some(stdout.trim_end_matches(['\r', '\n']).to_string()))
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
    fn test_write_args() {
        assert_eq!(
            DefaultsValue::Bool(true).write_args(),
            Some(vec!["-bool".to_string(), "true".to_string()])
        );
        assert_eq!(
            DefaultsValue::Bool(false).write_args(),
            Some(vec!["-bool".to_string(), "false".to_string()])
        );
        assert_eq!(
            DefaultsValue::Int(2).write_args(),
            Some(vec!["-int".to_string(), "2".to_string()])
        );
        assert_eq!(
            DefaultsValue::Float(0.5).write_args(),
            Some(vec!["-float".to_string(), "0.5".to_string()])
        );
        assert_eq!(
            DefaultsValue::Str("left".into()).write_args(),
            Some(vec!["-string".to_string(), "left".to_string()])
        );
        assert_eq!(
            DefaultsValue::Dict(IndexMap::new()).write_args(),
            None
        );
        assert_eq!(DefaultsValue::Array(vec![]).write_args(), None);
    }

    #[test]
    fn test_matches() {
        // booleans read back as 1/0
        assert!(DefaultsValue::Bool(true).matches("boolean", "1"));
        assert!(DefaultsValue::Bool(false).matches("boolean", "0"));
        assert!(!DefaultsValue::Bool(true).matches("boolean", "0"));
        // strict typing: integer 1 does not satisfy `true`
        assert!(!DefaultsValue::Bool(true).matches("integer", "1"));

        assert!(DefaultsValue::Int(2).matches("integer", "2"));
        assert!(!DefaultsValue::Int(2).matches("integer", "3"));
        assert!(!DefaultsValue::Int(2).matches("float", "2"));

        // `defaults read` may print floats without a fraction
        assert!(DefaultsValue::Float(48.0).matches("float", "48"));
        assert!(DefaultsValue::Float(0.5).matches("float", "0.5"));
        assert!(!DefaultsValue::Float(0.5).matches("float", "0.6"));

        assert!(DefaultsValue::Str("left".into()).matches("string", "left"));
        assert!(!DefaultsValue::Str("left".into()).matches("string", "right"));

        // nested values always return false (use plist path)
        assert!(!DefaultsValue::Dict(IndexMap::new()).matches("dictionary", "{}"));
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
