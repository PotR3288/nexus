//! Config-file machinery shared by xtminer and edgecase.
//!
//! Extracted from the API-parity work both miners carry as near-copies
//! (xtminer `564cb3c`/`12e0b0e`, edgecase `62ad490`/`ce91780`). Semantics are
//! frozen — they are the contract xtminer-gui and Tari Universe clients see:
//!
//! - **Mask token**: GET /config replaces secrets with `"***"`; a PUT carrying
//!   `"***"` means "unchanged" and never clobbers the real value on disk.
//! - **Deep merge**: tables merge key-by-key; scalars and arrays replace
//!   wholesale (send the full `[[pool]]` list, not a fragment).
//! - **RFC 7386 nulls**: object-level `null` in a JSON body deletes the key so
//!   Config defaults re-apply; nulls inside arrays become `""` (TOML has no
//!   null). GET→PUT round-trips are idempotent.
//! - **Validate before write**: the merged document is parsed into the
//!   miner's `Config` (deny_unknown_fields) and any extra validation runs
//!   BEFORE the on-disk file is touched; writes are atomic (temp + rename).

use serde_json::Value;
use std::path::Path;

/// Outcome of loading configuration from a file (identical in both miners).
///
/// Distinguishes "the file is absent" from "the file exists but is broken",
/// so the caller can decide whether a missing file is a loud warning (the
/// default path, where running with built-in defaults is reasonable) or a
/// hard error (an explicitly-supplied path the user expects to exist).
#[derive(Debug)]
pub enum ConfigLoad<T> {
    /// The config file was found and parsed successfully.
    Loaded(T),
    /// The config file does not exist at the given path.
    NotFound,
    /// The file exists but could not be read or parsed.
    Error(String),
}

impl<T> ConfigLoad<T> {
    /// True when a config was loaded (used by the watcher to decide whether
    /// to watch at all).
    pub fn is_loaded(&self) -> bool {
        matches!(self, ConfigLoad::Loaded(_))
    }

    /// The loaded config, if any.
    pub fn ok(self) -> Option<T> {
        match self {
            ConfigLoad::Loaded(c) => Some(c),
            _ => None,
        }
    }
}

/// Parse a config file of type `T` (TOML, `deny_unknown_fields`) with the
/// tri-state semantics both miners use: absent file → `NotFound`, unreadable
/// or unparseable file → `Error`, otherwise `Loaded`.
pub fn load_toml_config<T: serde::de::DeserializeOwned>(path: &str) -> ConfigLoad<T> {
    if !Path::new(path).exists() {
        return ConfigLoad::NotFound;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => return ConfigLoad::Error(format!("Failed to read config file: {e}")),
    };
    match toml::from_str(&content) {
        Ok(config) => ConfigLoad::Loaded(config),
        Err(e) => ConfigLoad::Error(format!("Failed to parse config: {e}")),
    }
}

/// Mask every secret before it can leave the process over loopback. `paths`
/// are dot-separated section paths whose leaf value is either a table
/// (`[pool]` in edgecase) or an array of tables (`[[pool]]` in xtminer);
/// every table under the path gets `key` masked. Empty values stay empty
/// (honest state — nothing to hide), matching both miners' behavior; a
/// masked empty password round-trips back to "keep existing" anyway.
/// The API token always lives at `[api].token` in both miners.
pub fn mask_secrets(v: &mut Value, secrets: &[(&str, &str)]) {
    fn mask_in(obj: &mut Value, key: &str) {
        if let Some(pw) = obj.get(key).and_then(|p| p.as_str()) {
            if !pw.is_empty() {
                if let Some(o) = obj.as_object_mut() {
                    o.insert(key.to_string(), Value::String("***".into()));
                }
            }
        }
    }
    for (path, key) in secrets {
        // Reborrow so `v` stays usable for the [api].token pass below.
        let mut node: Option<&mut Value> = Some(&mut *v);
        for part in path.split('.') {
            node = node.and_then(|n| n.get_mut(part));
        }
        match node {
            Some(Value::Array(items)) => {
                for item in items.iter_mut() {
                    mask_in(item, key);
                }
            }
            Some(node) => mask_in(node, key),
            None => {}
        }
    }
    if let Some(obj) = v.get_mut("api").and_then(|a| a.as_object_mut()) {
        if let Some(tok) = obj.get("token").and_then(|t| t.as_str()) {
            if !tok.is_empty() {
                obj.insert("token".to_string(), Value::String("***".into()));
            }
        }
    }
}

/// Deep-merge `overlay` into `base`: tables merge key-by-key, everything else
/// (scalars, arrays) is replaced wholesale. The literal string `"***"` — the
/// GET /config mask token — never overwrites a real value, so masked
/// round-trips can't clobber secrets.
pub fn deep_merge_toml(base: &mut toml::Value, overlay: &toml::Value) {
    let both_tables =
        matches!(base, toml::Value::Table(_)) && matches!(overlay, toml::Value::Table(_));
    let both_equal_arrays = matches!(base, toml::Value::Array(_))
        && matches!(overlay, toml::Value::Array(_))
        && base.as_array().map(|a| a.len()) == overlay.as_array().map(|a| a.len());
    if both_tables {
        // Safe: the match above guarantees both are tables.
        let (b, o) = match (base, overlay) {
            (toml::Value::Table(b), toml::Value::Table(o)) => (b, o),
            _ => unreachable!(),
        };
        for (k, v) in o {
            match b.get_mut(k) {
                Some(existing) => deep_merge_toml(existing, v),
                None => {
                    b.insert(k.clone(), v.clone());
                }
            }
        }
    } else if both_equal_arrays {
        // Equal-length arrays merge ELEMENT-WISE: a masked round-trip PUT of
        // an xmrig-style `[[pool]]` chain (array of tables with `pass = "***"`
        // where secrets were masked) must not wipe the real credentials on
        // disk — with element-wise recursion each masked cell keeps its
        // stored value, exactly like masked scalar fields. Different lengths
        // mean an actual add/remove edit and fall through to replacement.
        // Safe: the guard above guarantees both are same-length arrays.
        let (b, o) = match (base, overlay) {
            (toml::Value::Array(b), toml::Value::Array(o)) => (b, o),
            _ => unreachable!(),
        };
        for (bv, ov) in b.iter_mut().zip(o.iter()) {
            deep_merge_toml(bv, ov);
        }
    } else if matches!(overlay, toml::Value::String(s) if s == "***") {
        // Mask token: keep whatever is on disk.
    } else {
        *base = overlay.clone();
    }
}

/// Convert a JSON document into a TOML value (for PUT /config bodies sent as
/// JSON). Nulls become empty strings — TOML has no null, and an explicit null
/// in a config edit is almost always "clear this field". Use
/// [`collect_null_deletions`] alongside this to get true deletions instead.
pub fn json_to_toml_value(v: &Value) -> toml::Value {
    match v {
        Value::Null => toml::Value::String(String::new()),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                toml::Value::String(n.to_string())
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(a) => toml::Value::Array(a.iter().map(json_to_toml_value).collect()),
        Value::Object(o) => {
            let mut t = toml::map::Map::new();
            for (k, val) in o {
                t.insert(k.clone(), json_to_toml_value(val));
            }
            toml::Value::Table(t)
        }
    }
}

/// Collect object-level `null` paths from a JSON document — the RFC 7386
/// deletions applied after the merge. Nulls inside arrays are not deletions;
/// they convert to "" like any other non-object value.
pub fn collect_null_deletions(v: &Value, prefix: &[String], out: &mut Vec<Vec<String>>) {
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            let mut path = prefix.to_vec();
            path.push(k.clone());
            if val.is_null() {
                out.push(path);
            } else {
                collect_null_deletions(val, &path, out);
            }
        }
    }
}

/// Remove `path` from a TOML value tree; no-op when any segment is missing or
/// not a table.
pub fn delete_toml_path(v: &mut toml::Value, path: &[String]) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if let toml::Value::Table(t) = v {
        if rest.is_empty() {
            t.remove(first);
        } else if let Some(child) = t.get_mut(first) {
            delete_toml_path(child, rest);
        }
    }
}

/// Parse a PUT /config body as TOML first, then JSON; JSON object-level nulls
/// are recorded as RFC 7386 deletions and converted per the rules above.
/// Returns `(merged_document, null_deletion_paths)` or an error message.
pub fn parse_put_body(text: &str) -> Result<(toml::Value, Vec<Vec<String>>), String> {
    let mut null_deletions: Vec<Vec<String>> = Vec::new();
    let doc: toml::Value = match text.parse::<toml::Value>() {
        Ok(v) => v,
        Err(_) => match serde_json::from_str::<serde_json::Value>(text) {
            Ok(jv) => {
                collect_null_deletions(&jv, &[], &mut null_deletions);
                json_to_toml_value(&jv)
            }
            Err(e) => {
                return Err(format!("body is neither valid TOML nor JSON: {e}"));
            }
        },
    };
    Ok((doc, null_deletions))
}

/// Serialize `merged` to TOML, validate it by deserializing into `T` (the
/// miner's `Config`, `deny_unknown_fields`), run the miner's extra
/// validation, then atomically replace `path` (temp file in the same
/// directory + rename). On any failure nothing is written.
///
/// `extra_validate` runs after the type-level parse so a bad edit can never
/// leave an unparseable file behind (the watcher would keep the old config
/// anyway, but then GET /config and the file would disagree).
pub fn validate_and_write<T: serde::de::DeserializeOwned>(
    path: &str,
    merged: &toml::Value,
    extra_validate: impl Fn(&T) -> Result<(), String>,
) -> Result<(), String> {
    // Validate the MERGED result before touching disk.
    let cfg: T = match merged.clone().try_into() {
        Ok(c) => c,
        Err(e) => return Err(format!("rejected before write — invalid config after merge: {e}")),
    };
    extra_validate(&cfg)?;

    // Canonical form, no comments (same trade-off as any programmatic
    // rewrite; the file is the source of truth for values, not formatting).
    let out = toml::to_string_pretty(merged)
        .map_err(|e| format!("failed serializing merged config: {e}"))?;

    // Atomic replace: temp file in the same directory + rename. The watcher's
    // mtime poll picks it up within ~2s.
    let dir = Path::new(path)
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".config.toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, out).map_err(|e| format!("failed writing temp config: {e}"))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("failed replacing config file: {e}"));
    }
    log::info!("API: config PUT applied to {path} — hot-reload watcher will pick it up");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[test]
    fn deep_merge_preserves_mask_token_and_missing_keys() {
        let mut base: toml::Value = "password = \"real-secret\"\nother = 7\n".parse().unwrap();
        let overlay: toml::Value = "password = \"***\"\nnew_key = true\n".parse().unwrap();
        deep_merge_toml(&mut base, &overlay);
        assert_eq!(
            base["password"],
            toml::Value::String("real-secret".to_string())
        );
        assert_eq!(base["other"], toml::Value::Integer(7));
        assert_eq!(base["new_key"], toml::Value::Boolean(true));

        // A real (non-mask) value DOES overwrite.
        let mut base2: toml::Value = "password = \"old\"\n".parse().unwrap();
        let overlay2: toml::Value = "password = \"new\"\n".parse().unwrap();
        deep_merge_toml(&mut base2, &overlay2);
        assert_eq!(base2["password"], toml::Value::String("new".to_string()));

        // Nested tables merge key-by-key.
        let mut base3: toml::Value = "[a]\nx = 1\n".parse().unwrap();
        let overlay3: toml::Value = "[a]\ny = 2\n".parse().unwrap();
        deep_merge_toml(&mut base3, &overlay3);
        assert_eq!(base3["a"]["x"], toml::Value::Integer(1));
        assert_eq!(base3["a"]["y"], toml::Value::Integer(2));

        // Arrays replace wholesale.
        let mut base4: toml::Value = "list = [1, 2]\n".parse().unwrap();
        let overlay4: toml::Value = "list = [9]\n".parse().unwrap();
        deep_merge_toml(&mut base4, &overlay4);
        assert_eq!(base4["list"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn deep_merge_equal_length_arrays_of_tables_merge_element_wise() {
        // The `[[pool]]` masked-round-trip case: a GET /config (pass masked
        // to "***") PUT back must keep the real password on disk while
        // applying any real edits. Equal-length arrays recurse per element;
        // per-cell mask tokens keep the stored value.
        let mut base: toml::Value = r#"
[[pool]]
url = "a:3333"
pass = "real-a"

[[pool]]
url = "b:3333"
pass = "real-b"
"#
        .parse()
        .unwrap();
        let overlay: toml::Value = r#"
[[pool]]
url = "a:3333"
pass = "***"

[[pool]]
url = "CHANGED:3333"
pass = "***"
"#
        .parse()
        .unwrap();
        deep_merge_toml(&mut base, &overlay);
        let pools = base["pool"].as_array().unwrap();
        assert_eq!(pools[0]["pass"], toml::Value::String("real-a".to_string()));
        assert_eq!(pools[1]["pass"], toml::Value::String("real-b".to_string()));
        assert_eq!(
            pools[1]["url"],
            toml::Value::String("CHANGED:3333".to_string())
        );

        // A length change is an add/remove edit → wholesale replacement.
        let mut base2: toml::Value = "[[pool]]\nurl = \"a\"\n".parse().unwrap();
        let overlay2: toml::Value =
            "[[pool]]\nurl = \"a\"\n\n[[pool]]\nurl = \"b\"\n".parse().unwrap();
        deep_merge_toml(&mut base2, &overlay2);
        assert_eq!(base2["pool"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn json_to_toml_value_round_trips_scalars() {
        let jv: serde_json::Value = serde_json::from_str(
            r#"{"n":5,"f":1.5,"s":"x","b":true,"a":[1,2],"z":null}"#,
        )
        .unwrap();
        let tv = json_to_toml_value(&jv);
        assert_eq!(tv["n"], toml::Value::Integer(5));
        assert_eq!(tv["f"], toml::Value::Float(1.5));
        assert_eq!(tv["s"], toml::Value::String("x".to_string()));
        assert_eq!(tv["b"], toml::Value::Boolean(true));
        assert_eq!(tv["a"].as_array().unwrap().len(), 2);
        // Null → empty string (TOML has no null).
        assert_eq!(tv["z"], toml::Value::String(String::new()));
    }

    #[test]
    fn json_null_deletions_are_collected_by_path() {
        let jv: serde_json::Value =
            serde_json::from_str(r#"{"a":{"b":null,"c":1},"d":[null,2],"e":null}"#).unwrap();
        let mut out = Vec::new();
        collect_null_deletions(&jv, &[], &mut out);
        let paths: Vec<Vec<&str>> = out
            .iter()
            .map(|p| p.iter().map(String::as_str).collect())
            .collect();
        assert_eq!(paths, vec![vec!["a", "b"], vec!["e"]]);
    }

    #[test]
    fn delete_toml_path_removes_only_exact_paths() {
        let mut v: toml::Value = "[a]\nx = 1\ny = 2\n[b]\nz = 3\n".parse().unwrap();
        delete_toml_path(
            &mut v,
            &["a".to_string(), "x".to_string()],
        );
        assert!(v["a"].get("x").is_none());
        assert_eq!(v["a"]["y"], toml::Value::Integer(2));
        assert_eq!(v["b"]["z"], toml::Value::Integer(3));
        // Missing path is a no-op.
        delete_toml_path(&mut v, &["nope".to_string(), "x".to_string()]);
    }

    #[test]
    fn parse_put_body_accepts_toml_and_json_and_records_deletions() {
        let (doc, dels) = parse_put_body("[mining]\nwallet = \"w\"\n").unwrap();
        assert_eq!(doc["mining"]["wallet"], toml::Value::String("w".into()));
        assert!(dels.is_empty());

        let (doc, dels) = parse_put_body(r#"{"solver":{"ntrims":null,"threads":4}}"#).unwrap();
        assert_eq!(doc["solver"]["threads"], toml::Value::Integer(4));
        assert_eq!(dels, vec![vec!["solver".to_string(), "ntrims".to_string()]]);

        assert!(parse_put_body("this is { not valid").is_err());
    }

    #[test]
    fn mask_secrets_masks_nonempty_and_leaves_empty() {
        let mut v = serde_json::json!({
            "pool": {"pass": "secret"},
            "email": {"password": ""},
            "api": {"token": "tok123"}
        });
        mask_secrets(&mut v, &[("pool", "pass"), ("email", "password")]);
        assert_eq!(v["pool"]["pass"], "***");
        assert_eq!(v["email"]["password"], ""); // empty stays empty
        assert_eq!(v["api"]["token"], "***");
    }

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        #[serde(default = "default_threads")]
        threads: i32,
    }
    fn default_threads() -> i32 {
        50
    }

    #[test]
    fn validate_and_write_rejects_bad_and_writes_good_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "threads = 10\n").unwrap();

        // Bad value → rejected, file untouched.
        let merged: toml::Value = "threads = \"oops\"".parse().unwrap();
        let err = validate_and_write::<TestConfig>(
            path.to_str().unwrap(),
            &merged,
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(err.contains("rejected before write"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "threads = 10\n");

        // Extra validation can reject too.
        let merged: toml::Value = "threads = 7".parse().unwrap();
        let err = validate_and_write::<TestConfig>(
            path.to_str().unwrap(),
            &merged,
            |_| Err("threads must be >= 10".into()),
        )
        .unwrap_err();
        assert!(err.contains("threads must be >= 10"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "threads = 10\n");

        // Good value → written canonically, no temp leftovers.
        let merged: toml::Value = "threads = 20".parse().unwrap();
        validate_and_write::<TestConfig>(path.to_str().unwrap(), &merged, |_| Ok(()))
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("threads = 20"), "{written}");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn load_toml_config_tri_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        assert!(matches!(
            load_toml_config::<TestConfig>(path.to_str().unwrap()),
            ConfigLoad::NotFound
        ));
        std::fs::write(&path, "threads = 7\n").unwrap();
        let loaded = load_toml_config::<TestConfig>(path.to_str().unwrap());
        assert!(loaded.is_loaded());
        assert_eq!(loaded.ok().unwrap().threads, 7);
        std::fs::write(&path, "threads = \"not a number\"\n").unwrap();
        assert!(matches!(
            load_toml_config::<TestConfig>(path.to_str().unwrap()),
            ConfigLoad::Error(_)
        ));
    }
}
