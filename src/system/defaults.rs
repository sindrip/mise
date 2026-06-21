//! macOS user defaults (preferences) for the `[bootstrap.macos.defaults]` config section.
//!
//! Values are applied via `defaults export`/`defaults import` round-trips:
//! export the current plist, deep-merge declared values, import back.
//! Like `[bootstrap.packages]` they are machine-global, declarative, and
//! only ever applied when explicitly requested with
//! `mise bootstrap macos-defaults apply` or `mise bootstrap`.

use std::io::Cursor;
use std::ops::Deref;
use std::process::Stdio;

use indexmap::IndexMap;

use crate::result::Result;

/// Newtype around `plist::Value` so we can implement `Display`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PlistValue(pub plist::Value);

impl Deref for PlistValue {
    type Target = plist::Value;
    fn deref(&self) -> &plist::Value {
        &self.0
    }
}

impl From<plist::Value> for PlistValue {
    fn from(v: plist::Value) -> Self {
        Self(v)
    }
}

impl std::fmt::Display for PlistValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            plist::Value::Boolean(b) => write!(f, "{b}"),
            plist::Value::Integer(i) => match i.as_signed() {
                Some(v) => write!(f, "{v}"),
                None => write!(f, "{}", i.as_unsigned().unwrap()),
            },
            plist::Value::Real(v) => write!(f, "{v}"),
            plist::Value::String(s) => write!(f, "{s}"),
            plist::Value::Dictionary(_) => write!(f, "{{...}}"),
            plist::Value::Array(_) => write!(f, "[...]"),
            _ => write!(f, "(unsupported)"),
        }
    }
}

/// A single `[bootstrap.macos.defaults.<domain>]` entry: `key = value`
#[derive(Debug, Clone, PartialEq)]
pub struct DefaultsRequest {
    /// preferences domain, e.g. "com.apple.dock" or "NSGlobalDomain"
    pub domain: String,
    pub key: String,
    pub value: PlistValue,
}

impl std::fmt::Display for DefaultsRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} = {}", self.domain, self.key, self.value)
    }
}

/// Convert a TOML value to a plist value via serde.
/// Returns `None` for TOML datetimes.
pub fn toml_to_plist(value: &toml::Value) -> Option<PlistValue> {
    if contains_datetime(value) {
        return None;
    }
    plist::to_value(value).ok().map(PlistValue)
}

fn contains_datetime(value: &toml::Value) -> bool {
    match value {
        toml::Value::Datetime(_) => true,
        toml::Value::Array(values) => values.iter().any(contains_datetime),
        toml::Value::Table(table) => table.values().any(contains_datetime),
        _ => false,
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
                        current: PlistValue(current_plist.clone()).to_string(),
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
                .map(|r| format!("{} = {}", r.key, r.value))
                .collect();
            miseprintln!("defaults import {domain} (merge {})", keys.join(", "));
            continue;
        }
        let mut dict = export_domain(domain).await?.unwrap_or_default();
        for req in reqs {
            match (&req.value.0, dict.get_mut(&req.key)) {
                (
                    plist::Value::Dictionary(overlay),
                    Some(plist::Value::Dictionary(existing)),
                ) => {
                    deep_merge_plist(existing, overlay);
                }
                _ => {
                    dict.insert(req.key.clone(), req.value.0.clone());
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
            Some(plist::Value::Boolean(true).into())
        );
        assert_eq!(
            toml_to_plist(&val("48")),
            Some(plist::Value::Integer(48.into()).into())
        );
        assert_eq!(
            toml_to_plist(&val("1.5")),
            Some(plist::Value::Real(1.5).into())
        );
        assert_eq!(
            toml_to_plist(&val(r#""right""#)),
            Some(plist::Value::String("right".into()).into())
        );
        assert_eq!(
            toml_to_plist(&val("[1, 2]")),
            Some(
                plist::Value::Array(vec![
                    plist::Value::Integer(1.into()),
                    plist::Value::Integer(2.into()),
                ])
                .into()
            )
        );

        let pv = toml_to_plist(&val("{ a = 1 }")).unwrap();
        let inner = match &*pv {
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
        let pv = toml_to_plist(&toml).unwrap();
        let outer = match &*pv {
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
    fn test_toml_to_plist_rejects_nested_datetime() {
        assert_eq!(
            toml_to_plist(&val("{ updated_at = 2024-01-01T00:00:00Z }")),
            None
        );
        assert_eq!(toml_to_plist(&val("[2024-01-01T00:00:00Z]")), None);
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

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// Generate arbitrary toml::Value trees (no Datetime)
        fn arb_toml_value() -> impl Strategy<Value = toml::Value> {
            let leaf = prop_oneof![
                any::<bool>().prop_map(toml::Value::Boolean),
                any::<i64>().prop_map(toml::Value::Integer),
                // avoid NaN/Inf — not representable in JSON or plist
                (-1e10f64..1e10f64).prop_map(toml::Value::Float),
                "[a-z]{0,8}".prop_map(|s| toml::Value::String(s)),
            ];
            leaf.prop_recursive(3, 16, 4, |inner| {
                prop_oneof![
                    prop::collection::vec(inner.clone(), 0..4)
                        .prop_map(toml::Value::Array),
                    prop::collection::btree_map("[a-z]{1,4}", inner, 0..4)
                        .prop_map(|m| toml::Value::Table(m.into_iter().collect())),
                ]
            })
        }

        proptest! {
            /// toml_to_plist always succeeds for non-datetime values
            #[test]
            fn toml_to_plist_never_fails(v in arb_toml_value()) {
                prop_assert!(toml_to_plist(&v).is_some());
            }

            /// the plist serde bridge preserves values: serializing toml and the
            /// resulting plist to JSON produces identical output
            #[test]
            fn toml_plist_json_round_trip(v in arb_toml_value()) {
                let pv = toml_to_plist(&v).unwrap();
                let json_from_toml = serde_json::to_value(&v).unwrap();
                let json_from_plist = serde_json::to_value(&*pv).unwrap();
                prop_assert_eq!(json_from_toml, json_from_plist);
            }

            /// plist_contains is reflexive: a value always contains itself
            #[test]
            fn plist_contains_reflexive(v in arb_toml_value()) {
                let pv = toml_to_plist(&v).unwrap();
                prop_assert!(plist_contains(&pv, &pv));
            }

            /// a dict merged with itself is unchanged
            #[test]
            fn deep_merge_idempotent(v in arb_toml_value()) {
                let pv = toml_to_plist(&v).unwrap();
                if let plist::Value::Dictionary(dict) = &*pv {
                    let mut base = dict.clone();
                    deep_merge_plist(&mut base, dict);
                    prop_assert_eq!(&base, dict);
                }
            }
        }
    }
}
