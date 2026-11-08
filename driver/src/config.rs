//! Per-package type lists, from `[package.metadata.durable]`, on top of
//! built-in defaults for common crates.
//!
//! ```toml
//! [package.metadata.durable]
//! # Treated like std types: their own destructor and internals are trusted,
//! # their type arguments are still checked. Each entry states both facts.
//! trusted = [
//!     { type = "bytes::Bytes", drop = "frees-memory", sharing = "immutable" },
//! ]
//! # Handles valid only in the process that created them: always rejected.
//! process-local = ["my_pool::SlotId"]
//! ```
//!
//! A path is the type's public path (`bytes::Bytes`, not where it is defined),
//! or `crate::*` for every type of a crate. Local types are `<crate>::<path>`.

/// `drop`: the destructor only frees memory (no Effect).
const DROP_FACTS: &[&str] = &["frees-memory"];
/// `sharing`: copying it on resume loses nothing (no Identity). `immutable`:
/// shared data that cannot change; `unshared`: never shared with another value.
const SHARING_FACTS: &[&str] = &["immutable", "unshared"];

const DEFAULTS: &str = r#"
trusted = [
    { type = "bytes::Bytes", drop = "frees-memory", sharing = "immutable" },
    { type = "bytes::BytesMut", drop = "frees-memory", sharing = "unshared" },
    { type = "smallvec::SmallVec", drop = "frees-memory", sharing = "unshared" },
    { type = "arrayvec::ArrayVec", drop = "frees-memory", sharing = "unshared" },
    { type = "arrayvec::ArrayString", drop = "frees-memory", sharing = "unshared" },
    { type = "hashbrown::*", drop = "frees-memory", sharing = "unshared" },
]
process-local = [
    "slotmap::KeyData",
    "generational_arena::Index",
    "thunderdome::Index",
]
"#;

#[derive(Debug, Default)]
pub struct Config {
    trusted: Vec<String>,
    process_local: Vec<String>,
}

fn matches(list: &[String], path: &str) -> bool {
    list.iter().any(|p| match p.strip_suffix("::*") {
        Some(krate) => path.split("::").next() == Some(krate),
        None => p == path,
    })
}

impl Config {
    pub fn trusted(&self, path: &str) -> bool {
        matches(&self.trusted, path)
    }

    pub fn process_local(&self, path: &str) -> bool {
        matches(&self.process_local, path)
    }

    /// Add the lists of one `[package.metadata.durable]` table (or the defaults).
    fn add(&mut self, t: &toml::Table) -> Result<(), String> {
        for (k, _) in t {
            if !["trusted", "process-local"].contains(&k.as_str()) {
                return Err(format!("unknown key `{k}` (expected `trusted`, `process-local`)"));
            }
        }
        let list = |key: &str| t.get(key).map_or(Ok(&[][..]), |v| v.as_array().map(Vec::as_slice).ok_or(format!("`{key}` must be a list")));
        for e in list("trusted")? {
            let ty = e.get("type").and_then(|v| v.as_str()).ok_or(format!("trusted entry {e:?} needs `type`"))?;
            let fact = |name: &str, allowed: &[&str]| -> Result<(), String> {
                match e.get(name).and_then(|v| v.as_str()) {
                    Some(v) if allowed.contains(&v) => Ok(()),
                    Some(v) => Err(format!("trusted `{ty}`: `{name} = \"{v}\"` must be one of {allowed:?}")),
                    None => Err(format!(
                        "trusted `{ty}` must state `{name}` (one of {allowed:?}): a trusted type needs both \
                         `drop` (its destructor only frees memory) and `sharing` (a copy on resume loses nothing)"
                    )),
                }
            };
            fact("drop", DROP_FACTS)?;
            fact("sharing", SHARING_FACTS)?;
            self.trusted.push(ty.to_string());
        }
        for e in list("process-local")? {
            self.process_local.push(e.as_str().ok_or(format!("process-local entry {e:?} must be a type path"))?.to_string());
        }
        Ok(())
    }
}

/// Defaults plus the `[package.metadata.durable]` table of `manifest` (a Cargo.toml).
pub fn parse(manifest: &str) -> Result<Config, String> {
    let mut c = Config::default();
    c.add(&DEFAULTS.parse::<toml::Table>().expect("default config"))?;
    let m: toml::Table = manifest.parse().map_err(|e| format!("Cargo.toml: {e}"))?;
    if let Some(t) = m.get("package").and_then(|p| p.get("metadata")).and_then(|m| m.get("durable")) {
        c.add(t.as_table().ok_or("`package.metadata.durable` must be a table")?)?;
    }
    Ok(c)
}

/// The manifest of the package being compiled.
pub fn manifest() -> Option<String> {
    std::env::var("CARGO_MANIFEST_DIR").ok().map(|dir| format!("{dir}/Cargo.toml"))
}

/// The config of the package being compiled.
pub fn load() -> Result<Config, String> {
    match manifest() {
        Some(path) => {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
            parse(&text).map_err(|e| format!("{path}: [package.metadata.durable]: {e}"))
        }
        None => parse(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_package_entries() {
        let c = parse(
            r#"
            [package.metadata.durable]
            trusted = [{ type = "my::Buf", drop = "frees-memory", sharing = "unshared" }]
            process-local = ["my::Slot"]
            "#,
        )
        .unwrap();
        assert!(c.trusted("bytes::Bytes") && c.trusted("hashbrown::HashTable") && c.trusted("my::Buf"));
        assert!(!c.trusted("my::Other") && !c.trusted("hashbrownx::T"));
        assert!(c.process_local("slotmap::KeyData") && c.process_local("my::Slot"));
    }

    #[test]
    fn trusted_entries_need_both_facts() {
        let only_drop = r#"
            [package.metadata.durable]
            trusted = [{ type = "my::Rc", drop = "frees-memory" }]
            "#;
        assert!(parse(only_drop).unwrap_err().contains("must state `sharing`"));
        let bad = r#"
            [package.metadata.durable]
            trusted = [{ type = "my::Rc", drop = "frees-memory", sharing = "yes" }]
            "#;
        assert!(parse(bad).unwrap_err().contains("must be one of"));
        assert!(parse("[package.metadata.durable]\ntrustd = []").unwrap_err().contains("unknown key"));
    }
}
