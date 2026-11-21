//! The native Copilot manager contract and parsers, separate from process and settings I/O.

use std::collections::HashSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use serde::de::{self, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::{NameMap, Object, read_json_object};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct NativePlugin {
    pub name: String,
    pub marketplace: String,
    pub version: String,
    pub enabled: bool,
    pub source: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct NativeMarket {
    pub name: String,
    pub source: String,
    #[serde(rename = "isDefault")]
    pub is_default: bool,
    /// A separate proof from settings.json, never inferred from the CLI's source text.
    #[serde(skip)]
    pub origin: Option<MarketOrigin>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketOrigin {
    #[serde(rename = "source")]
    pub kind: String,
    pub repo: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
}

impl NativeMarket {
    #[inline(never)]
    pub fn confirms(&self, market: &str, repo: &str, branch: &str) -> bool {
        !self.is_default
            && !branch.is_empty()
            && self.name == market
            && self
                .source
                .split_at_checked("GitHub: ".len())
                .is_some_and(|(prefix, rest)| prefix.eq_ignore_ascii_case("GitHub: ") && rest.eq_ignore_ascii_case(repo))
            && self.origin.as_ref().is_some_and(|origin| {
                origin.kind == "github" && origin.repo.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(repo)) && origin.reference.as_deref() == Some(branch)
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSnapshot {
    pub home: PathBuf,
    pub plugins: Vec<NativePlugin>,
    pub markets: Vec<NativeMarket>,
}

impl NativeSnapshot {
    pub(crate) fn plugin(&self, name: &str, market: &str) -> Result<Option<&NativePlugin>, ManagerError> {
        unique(self.plugins.iter().filter(|p| p.name.eq_ignore_ascii_case(name) && p.marketplace.eq_ignore_ascii_case(market)))
    }

    pub(crate) fn market(&self, market: &str) -> Result<Option<&NativeMarket>, ManagerError> {
        unique(self.markets.iter().filter(|m| m.name.eq_ignore_ascii_case(market)))
    }
}

fn unique<T>(mut matches: impl Iterator<Item = T>) -> Result<Option<T>, ManagerError> {
    let first = matches.next();
    if matches.next().is_some() { Err(ManagerError::BadOutput) } else { Ok(first) }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManagerError {
    NotFound,
    RefusedUnderIsolation,
    Failed(String),
    TimedOut,
    BadOutput,
    Cancelled,
}

impl fmt::Display for ManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotFound => "GitHub Copilot CLI was not found. Install it to use plugin sources.",
            Self::RefusedUnderIsolation => "Set isolated Copilot home and cache folders before using plugin sources.",
            Self::Failed(output) => {
                let line = output.lines().next().unwrap_or_default().trim();
                if !line.is_empty() {
                    // Do not let a CLI error move the cursor, reorder text, or take over the report.
                    let mut safe = String::new();
                    for ch in line.chars() {
                        if ch.is_ascii() && !ch.is_ascii_control() {
                            safe.push(ch);
                        } else {
                            safe.extend(ch.escape_unicode());
                        }
                        if safe.len() > 240 {
                            safe.truncate(237);
                            safe.push_str("...");
                            break;
                        }
                    }
                    return f.write_str(&safe);
                }
                "GitHub Copilot CLI did not complete the command."
            }
            Self::TimedOut => "GitHub Copilot CLI took too long to respond.",
            Self::BadOutput => "GitHub Copilot CLI returned data that could not be verified.",
            Self::Cancelled => "The Copilot command was stopped.",
        })
    }
}

impl std::error::Error for ManagerError {}

pub trait PluginManager: Send + Sync {
    /// Resolved, absolute Copilot home. This must not launch a process.
    fn home(&self) -> Result<PathBuf, ManagerError>;
    fn list(&self, cancel: &AtomicBool) -> Result<Vec<NativePlugin>, ManagerError>;
    /// Includes the separate origin proof when available from settings.json.
    fn markets(&self, cancel: &AtomicBool) -> Result<Vec<NativeMarket>, ManagerError>;
    fn add_market(&self, repo: &str, branch: &str, cancel: &AtomicBool) -> Result<(), ManagerError>;
    fn install(&self, name_at_market: &str, cancel: &AtomicBool) -> Result<(), ManagerError>;
    fn update(&self, name_at_market: &str, cancel: &AtomicBool) -> Result<(), ManagerError>;
    fn uninstall(&self, name_at_market: &str, cancel: &AtomicBool) -> Result<(), ManagerError>;
}

pub fn parse_plugins(bytes: &[u8]) -> Result<Vec<NativePlugin>, ManagerError> {
    let rows: Vec<Object<NativePlugin>> = serde_json::from_slice(bytes).map_err(|_| ManagerError::BadOutput)?;
    let mut keys = HashSet::new();
    for Object(row) in &rows {
        if row.name.trim().is_empty() || row.source.trim().is_empty() || !keys.insert((row.name.to_ascii_lowercase(), row.marketplace.to_ascii_lowercase())) {
            return Err(ManagerError::BadOutput);
        }
    }
    Ok(rows.into_iter().map(|Object(row)| row).collect())
}

pub fn parse_markets(bytes: &[u8]) -> Result<Vec<NativeMarket>, ManagerError> {
    let rows: Vec<Object<NativeMarket>> = serde_json::from_slice(bytes).map_err(|_| ManagerError::BadOutput)?;
    let mut keys = HashSet::new();
    for Object(row) in &rows {
        if row.name.trim().is_empty() || row.source.trim().is_empty() || !keys.insert(row.name.to_ascii_lowercase()) {
            return Err(ManagerError::BadOutput);
        }
    }
    Ok(rows.into_iter().map(|Object(row)| row).collect())
}

/// Proofs from settings.json, consumed only for exact names from the marketplace list.
pub struct MarketOrigins(NameMap<Object<SettingMarket>>);

impl MarketOrigins {
    pub fn take(&mut self, name: &str) -> Option<MarketOrigin> {
        self.0.get_mut(name)?.0.source.take()?.0
    }
}

#[derive(Deserialize)]
struct SettingMarket {
    source: Option<SettingOrigin>,
}

struct SettingOrigin(Option<MarketOrigin>);

impl<'de> Deserialize<'de> for SettingOrigin {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Source,
            Repo,
            Ref,
            #[serde(other)]
            Other,
        }

        struct SourceFields;
        impl<'de> Visitor<'de> for SourceFields {
            type Value = SettingOrigin;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a marketplace source object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let (mut kind, mut repo, mut reference) = (None, None, None);
                let mut known = true;
                while let Some(field) = map.next_key()? {
                    match field {
                        Field::Source if kind.is_none() => kind = Some(map.next_value()?),
                        Field::Repo if repo.is_none() => repo = Some(map.next_value()?),
                        Field::Ref if reference.is_none() => reference = Some(map.next_value()?),
                        Field::Other => {
                            map.next_value::<IgnoredAny>()?;
                            // Extra fields could change what this source means, not what other markets mean.
                            known = false;
                        }
                        _ => return Err(de::Error::custom("duplicate marketplace source field")),
                    }
                }
                let kind = kind.ok_or_else(|| de::Error::missing_field("source"))?;
                Ok(SettingOrigin(known.then_some(MarketOrigin { kind, repo: repo.flatten(), reference: reference.flatten() })))
            }
        }
        d.deserialize_map(SourceFields)
    }
}

#[inline(never)]
pub fn parse_market_origins(bytes: &[u8]) -> Result<MarketOrigins, ManagerError> {
    #[derive(Deserialize)]
    struct Settings {
        #[serde(rename = "extraKnownMarketplaces", default)]
        markets: NameMap<Object<SettingMarket>>,
    }
    let settings: Settings = read_json_object(bytes).ok_or(ManagerError::BadOutput)?;
    let mut keys = HashSet::new();
    if settings.markets.keys().any(|name| name.trim().is_empty() || !keys.insert(name.to_ascii_lowercase())) {
        return Err(ManagerError::BadOutput);
    }
    Ok(MarketOrigins(settings.markets))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_native_shapes_allow_unknown_fields_but_origin_proofs_do_not() {
        let plugins = parse_plugins(
            br#"[
            {"name":"browser-use","marketplace":"","version":"builtin","enabled":false,"source":"builtin"},
            {"name":"one","marketplace":"tools","version":"2.0","enabled":true,"source":"installed","installedFrom":{}}
        ]"#,
        )
        .unwrap();
        assert_eq!((&*plugins[0].marketplace, &*plugins[0].source), ("", "builtin"));
        assert!(plugins[1].enabled);
        let mut markets = parse_markets(br#"[{"name":"tools","source":"GitHub: O/r","isDefault":false,"origin":{"repo":"o/r"}}]"#).unwrap();
        let market = &mut markets[0];
        assert!(market.origin.is_none());
        assert!(!market.confirms("tools", "o/r", "release/v2"));
        let mut origins = parse_market_origins(
            br#"{"extraKnownMarketplaces":{
            "tools":{"source":{"source":"github","repo":"o/R","ref":"release/v2"}},
            "future-tools":{"source":{"source":"github","repo":"o/R","ref":"release/v2","future":true}},
            "other":{"source":{"source":"local","path":"C:\\tools"}}
        },"enabledPlugins":{"one@tools":true}}"#,
        )
        .unwrap();
        assert!(origins.take("future-tools").is_none());
        assert!(origins.take("other").is_none());
        market.origin = origins.take("tools");
        assert!(market.confirms("tools", "o/r", "release/v2"));
        assert!(!market.confirms("tools", "o/r", "release/V2"));
        assert!(parse_market_origins(b"{}").unwrap().0.values().all(|row| row.0.source.is_none()));
    }

    #[test]
    fn plugin_origin_does_not_confirm_with_an_extra_path() {
        let bytes = br#"{"extraKnownMarketplaces":{"tools":{"source":{
            "source":"github","repo":"owner/repo","ref":"main","path":"D:\\sandbox\\local-tools"
        }}}}"#;
        let mut origins = parse_market_origins(bytes).unwrap();
        let market = NativeMarket { name: "tools".into(), source: "GitHub: owner/repo".into(), is_default: false, origin: origins.take("tools") };
        assert!(market.origin.is_none());
        assert!(!market.confirms("tools", "owner/repo", "main"));
    }

    #[test]
    fn malformed_native_evidence_is_never_an_empty_list_or_an_origin() {
        for bad in ["{}", "null", r#"[["one","tools","2.0",true,"installed"]]"#, r#"[{"name":"one"}]"#] {
            assert_eq!(parse_plugins(bad.as_bytes()), Err(ManagerError::BadOutput), "{bad}");
        }
        let row = r#"{"name":"one","marketplace":"tools","version":"2.0","enabled":true,"source":"installed"}"#;
        assert!(parse_plugins(format!("[{row},{row}]").as_bytes()).is_err());
        assert!(parse_markets(br#"[{"name":"tools","source":"Local: c:/x","isDefault":"false"}]"#).is_err());
        for bad in [
            "[]",
            r#"{"extraKnownMarketplaces":null}"#,
            r#"{"extraKnownMarketplaces":{"tools":{"source":["github","o/r","main"]}}}"#,
            r#"{"extraKnownMarketplaces":{"tools":{"source":{"source":"github","repo":"o/r","ref":true}}}}"#,
            r#"{"extraKnownMarketplaces":{"tools":{"source":{"source":"github","repo":"o/r","ref":null,"ref":"main"}}}}"#,
            r#"{"extraKnownMarketplaces":{"tools":{},"tools":{}}}"#,
            r#"{"extraKnownMarketplaces":{"tools":{},"Tools":{}}}"#,
        ] {
            assert!(parse_market_origins(bad.as_bytes()).is_err(), "{bad}");
        }
        let mut origins = parse_market_origins(br#"{"extraKnownMarketplaces":{"tools":{"source":{"source":"github","repo":"o/r"}}}}"#).unwrap();
        assert_eq!(origins.take("tools").unwrap().reference, None, "a missing ref cannot prove a branch");
    }

    #[test]
    fn cli_error_shows_only_a_printable_bounded_first_line() {
        let line = ManagerError::Failed(format!("Failed\x1b{}\nnot this", "x".repeat(500))).to_string();
        assert!(line.starts_with("Failed\\u{1b}"));
        assert!(line.ends_with("..."));
        assert_eq!(line.len(), 240);
        assert_eq!(ManagerError::Failed("one\r\ntwo".into()).to_string(), "one");
    }
}
