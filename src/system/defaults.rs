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
    pub value: plist::Value,
}

impl std::fmt::Display for DefaultsRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} = {}", self.domain, self.key, display_plist(&self.value))
    }
}

/// Convert a TOML value to a plist value via serde.
/// Returns `None` for TOML datetimes (no plist equivalent).
pub fn toml_to_plist(value: &toml::Value) -> Option<plist::Value> {
    if matches!(value, toml::Value::Datetime(_)) {
        return None;
    }
    let json = serde_json::to_value(value).ok()?;
    serde_json::from_value(json).ok()
}

/// Compact display of a plist value for status tables.
pub fn display_plist(value: &plist::Value) -> String {
    match value {
        plist::Value::Boolean(b) => b.to_string(),
        plist::Value::Integer(i) => i
            .as_signed()
            .map(|v| v.to_string())
            .unwrap_or_else(|| i.as_unsigned().unwrap().to_string()),
        plist::Value::Real(f) => f.to_string(),
        plist::Value::String(s) => s.clone(),
        plist::Value::Dictionary(_) => "{...}".to_string(),
        plist::Value::Array(_) => "[...]".to_string(),
        plist::Value::Data(_) => "(data)".to_string(),
        plist::Value::Date(_) => "(date)".to_string(),
        plist::Value::Uid(_) => "(uid)".to_string(),
        _ => "(unknown)".to_string(),
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
                if plist_contains(current_plist, &req.value) {
                    DefaultsState::Set
                } else {
                    DefaultsState::Differs {
                        current: display_plist(current_plist),
                    }
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
            let keys: Vec<_> = reqs
                .iter()
                .map(|r| format!("{} = {}", r.key, display_plist(&r.value)))
                .collect();
            miseprintln!("defaults import {domain} (merge {})", keys.join(", "));
            continue;
        }
        let mut dict = export_domain(domain).await?.unwrap_or_default();
        for req in reqs {
            match (&req.value, dict.get_mut(&req.key)) {
                (
                    plist::Value::Dictionary(overlay),
                    Some(plist::Value::Dictionary(existing)),
                ) => {
                    deep_merge_plist(existing, overlay);
                }
                _ => {
                    dict.insert(req.key.clone(), req.value.clone());
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
    fn test_toml_to_plist() {
        assert_eq!(
            toml_to_plist(&val("true")),
            Some(plist::Value::Boolean(true))
        );
        assert_eq!(
            toml_to_plist(&val("48")),
            Some(plist::Value::Integer(48.into()))
        );
        assert_eq!(
            toml_to_plist(&val("1.5")),
            Some(plist::Value::Real(1.5))
        );
        assert_eq!(
            toml_to_plist(&val(r#""right""#)),
            Some(plist::Value::String("right".into()))
        );
        assert_eq!(
            toml_to_plist(&val("[1, 2]")),
            Some(plist::Value::Array(vec![
                plist::Value::Integer(1.into()),
                plist::Value::Integer(2.into()),
            ]))
        );

        let dict = toml_to_plist(&val("{ a = 1 }")).unwrap();
        let inner = match &dict {
            plist::Value::Dictionary(d) => d,
            _ => panic!("expected dict"),
        };
        assert_eq!(inner.get("a"), Some(&plist::Value::Integer(1.into())));
    }

    #[test]
    fn test_toml_to_plist_nested() {
        let toml: toml::Value = toml::from_str(
            r#"
            [inner]
            enabled = false
            "#,
        )
        .unwrap();
        let val = toml_to_plist(&toml).unwrap();
        let outer = match &val {
            plist::Value::Dictionary(d) => d,
            _ => panic!("expected dict"),
        };
        let inner = match outer.get("inner") {
            Some(plist::Value::Dictionary(d)) => d,
            _ => panic!("expected inner dict"),
        };
        assert_eq!(inner.get("enabled"), Some(&plist::Value::Boolean(false)));
    }

    #[test]
    fn test_toml_to_plist_rejects_datetime() {
        assert_eq!(toml_to_plist(&val("2024-01-01T00:00:00Z")), None);
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
