use std::collections::BTreeMap;

use super::*;
use serde_json::json;

fn source(path: &str) -> RepoConfig {
    RepoConfig { id: "source".into(), repo: "owner/repo".into(), branch: "main".into(), path: path.into(), kind: SourceKind::Plugins, ..Default::default() }
}

fn listing(files: &[(&str, &[u8])]) -> Snapshot {
    let mut entries = BTreeMap::new();
    for (path, data) in files {
        for (i, _) in path.match_indices('/') {
            let dir = &path[..i];
            entries.entry(dir).or_insert_with(|| TreeEntry {
                path: dir.into(),
                kind: EntryKind::Tree,
                mode: "040000".into(),
                sha: hash::blob_hex(dir.as_bytes()),
                size: None,
            });
        }
        entries.insert(
            path,
            TreeEntry { path: (*path).into(), kind: EntryKind::Blob, mode: "100644".into(), sha: hash::blob_hex(data), size: Some(data.len() as u64) },
        );
    }
    Snapshot { sha: "a".repeat(40), truncated: false, entries: entries.into_values().collect() }
}

#[test]
fn selection_uses_cli_precedence_exact_git_scope_and_native_capability_paths() {
    let market = br#"{"name":"work-tools","owner":{"name":"Org"},"plugins":[
        {"name":"one","source":"./tools/one","version":"1","description":"Entry details"},
        {"name":"two","source":"tools/nested/two","version":"3"},
        {"name":"not-tools","source":"toolsmith/not-tools"},
        {"name":"elsewhere","source":{"source":"url","url":"https://example.invalid/repo"}},
        {"name":"other-url","source":"https://example.invalid/repo"}
    ]}"#;
    let one = br#"{"name":"one","version":"2","author":{"name":"Team"},"mcpServers":{}}"#;
    let two = br#"{"name":"two","description":"Other details"}"#;
    let files: &[(&str, &[u8])] = &[
        ("marketplace.json", market),
        (".plugin/marketplace.json", b"{}"),
        ("tools/one/.plugin/plugin.json", one),
        ("tools/one/plugin.json", b"{}"),
        ("tools/one/com.github.copilot/hooks/hooks.json", b"{}"),
        ("tools/one/.github/lsp.json", b"{}"),
        ("tools/nested/two/.claude-plugin/plugin.json", two),
    ];
    let snapshot = listing(files);
    let tree = CatalogTree::new(&snapshot).unwrap();
    assert_eq!(tree.marketplace_manifest().unwrap().path, "marketplace.json");
    let pending = tree.match_marketplace(&source("tools"), market).unwrap();
    assert_eq!(
        pending.manifests().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        ["tools/one/.plugin/plugin.json", "tools/nested/two/.claude-plugin/plugin.json"]
    );
    let bytes = files.iter().map(|(p, b)| ((*p).into(), b.to_vec())).collect();
    let catalog = pending.finish(&bytes);
    assert_eq!((catalog.market(), catalog.owner()), ("work-tools", Some("Org")));
    assert!(catalog.rejected().is_empty());
    assert_eq!(catalog.hosted_elsewhere(), ["elsewhere", "other-url"]);
    let first = &catalog.plugins()["one"];
    assert_eq!((first.version.as_deref(), first.publisher.as_deref(), first.description.as_deref()), (Some("2"), Some("Team"), Some("Entry details")));
    assert_eq!(first.capabilities, Capabilities { hooks: true, mcp: true, lsp: true });
    let second = &catalog.plugins()["two"];
    assert_eq!((second.version.as_deref(), second.publisher.as_deref(), second.description.as_deref()), (Some("3"), Some("Org"), Some("Other details")));
    assert_eq!(second.capabilities, Capabilities::default());
    let record = catalog.approved_record(&source("tools"), Path::new(r"D:\isolated\copilot")).unwrap();
    assert_eq!(record.approved.keys().collect::<Vec<_>>(), ["one", "two"]);
    assert!(record.plugins.is_empty());
}

#[test]
fn malformed_duplicates_wrong_paths_and_unreadable_manifests_are_not_download_candidates() {
    let market = br#"{"name":"tools","plugins":[
        {"name":"wrong","source":"tools/wrong"},
        {"name":"mismatch","source":"tools/mismatch"},
        {"name":"tool","source":"tools/wrong"},
        {"name":"tool","source":{"source":"github","repo":"other/repo"}},
        {"name":"renamed","source":"tools/mismatch"},
        {"name":"Renamed","source":"elsewhere/path"},
        {"name":"alias","source":"tools/alias"},
        {"name":"oversize","source":"tools/oversize"},
        {"name":"missing","source":"tools/missing"},
        {"name":"unsafe","source":"tools/../outside"},
        {"name":"absolute","source":"C:/folder"},
        {"name":"backslash","source":"tools\\outside"},
        {"name":"device","source":"tools/CON.txt"},
        {"name":"unnamed-source"}
    ]}"#;
    let mismatch = br#"{"name":"another"}"#;
    let large = vec![b' '; MAX_MANIFEST_BYTES as usize + 1];
    let snapshot = listing(&[
        (".github/plugin/marketplace.json", market),
        ("tools/wrong/plugin.json", b"not JSON"),
        ("tools/mismatch/plugin.json", mismatch),
        ("tools/oversize/plugin.json", &large),
        ("tools/oversize/.github/plugin/plugin.json", b"{}"),
        ("tools/alias/.plugin/Plugin.json", b"{}"),
        ("tools/alias/plugin.json", b"{}"),
        ("tools/missing/readme.md", b""),
    ]);
    let tree = CatalogTree::new(&snapshot).unwrap();
    assert_eq!(tree.marketplace_manifest().unwrap().path, ".github/plugin/marketplace.json");
    let pending = tree.match_marketplace(&source("tools"), market).unwrap();
    assert_eq!(pending.manifests().map(|f| f.path.as_str()).collect::<Vec<_>>(), ["tools/wrong/plugin.json", "tools/mismatch/plugin.json"]);
    let catalog = pending.finish(&[("tools/wrong/plugin.json".into(), b"not JSON".to_vec()), ("tools/mismatch/plugin.json".into(), mismatch.to_vec())].into());
    assert!(catalog.plugins().is_empty());
    let rejected: BTreeMap<_, _> = catalog.rejected().iter().map(|r| (r.name.as_str(), r.reason.as_str())).collect();
    assert_eq!(rejected.len(), 12);
    assert!(rejected["tool"].contains("Duplicate"));
    assert!(rejected["renamed"].contains("Duplicate"));
    assert!(rejected["alias"].contains("Ambiguous"));
    assert!(rejected["oversize"].contains("256 KiB"));
    assert!(rejected["mismatch"].contains("does not match"));
    assert!(rejected["wrong"].contains("JSON"));
}

#[test]
fn incomplete_listings_and_bad_marketplaces_never_make_an_empty_catalog() {
    let missing = listing(&[("tools/marketplace.json", b"{}")]);
    assert!(CatalogTree::new(&missing).unwrap().marketplace_manifest().unwrap_err().contains("top of this branch"));
    let alias = listing(&[("Marketplace.json", b"{}"), (".plugin/marketplace.json", b"{}")]);
    assert!(CatalogTree::new(&alias).unwrap().marketplace_manifest().is_err());
    let truncated = Snapshot { truncated: true, ..missing };
    assert!(CatalogTree::new(&truncated).is_err());
    for market in [json!({"name":"Bad Name!!","plugins":[]}), json!({"name":"valid"}), json!({"name":"valid","plugins":null})] {
        let bytes = serde_json::to_vec(&market).unwrap();
        let malformed = listing(&[("marketplace.json", &bytes)]);
        assert!(CatalogTree::new(&malformed).unwrap().match_marketplace(&source(""), &bytes).is_err());
    }
}

fn one_plugin(market: Value, manifest: Value) -> Catalog {
    let market = serde_json::to_vec(&market).unwrap();
    let manifest = serde_json::to_vec(&manifest).unwrap();
    let file = "plugins/tool/plugin.json";
    let snapshot = listing(&[("marketplace.json", &market), (file, &manifest)]);
    let tree = CatalogTree::new(&snapshot).unwrap();
    tree.match_marketplace(&source(""), &market).unwrap().finish(&[(file.into(), manifest)].into())
}

fn metadata(publisher: &str, version: &str, from_market: bool) -> Catalog {
    if from_market {
        one_plugin(
            json!({"name":"tools", "owner":{"name":publisher}, "plugins":[{"name":"tool", "source":"plugins/tool", "version":version}]}),
            json!({"name":"tool"}),
        )
    } else {
        one_plugin(
            json!({"name":"tools", "plugins":[{"name":"tool", "source":"plugins/tool"}]}),
            json!({"name":"tool", "author":{"name":publisher}, "version":version}),
        )
    }
}

fn rejects_field(catalog: &Catalog, field: &str, reason: &str) {
    assert!(catalog.plugins().is_empty(), "accepted a plugin with an invalid {field}");
    assert_eq!(catalog.rejected().len(), 1);
    let rejected = &catalog.rejected()[0];
    assert_eq!(rejected.name, "tool");
    assert!(rejected.reason.contains(field) && rejected.reason.contains(reason), "{}", rejected.reason);
}

#[test]
fn plugin_catalog_bounds_publisher_and_version_by_characters() {
    let fits = "\u{e9}".repeat(64);
    let long = format!("{fits}a");
    for from_market in [false, true] {
        let catalog = metadata(&fits, &fits, from_market);
        assert_eq!(catalog.plugins()["tool"].publisher.as_deref(), Some(fits.as_str()));
        assert_eq!(catalog.plugins()["tool"].version.as_deref(), Some(fits.as_str()));
        rejects_field(&metadata(&long, "1", from_market), "publisher", "longer than 64 characters");
        rejects_field(&metadata("Team", &long, from_market), "version", "longer than 64 characters");
    }
}

#[test]
fn plugin_catalog_refuses_invisible_metadata() {
    let singles = [0x00ad, 0x061c, 0x180e, 0xfeff];
    let ranges = (0x200b..=0x200f).chain(0x202a..=0x202e).chain(0x2060..=0x2064).chain(0x2066..=0x2069);
    for code in singles.into_iter().chain(ranges) {
        let text = format!("a{}b", char::from_u32(code).unwrap());
        for from_market in [false, true] {
            rejects_field(&metadata(&text, "1", from_market), "publisher", "invisible formatting character");
            rejects_field(&metadata("Team", &text, from_market), "version", "invisible formatting character");
        }
    }
}

#[test]
fn plugin_catalog_refuses_control_characters_in_metadata() {
    for text in ["a\0b", "a\nb", "a\u{7f}b", "a\u{9b}b"] {
        for from_market in [false, true] {
            rejects_field(&metadata(text, "1", from_market), "publisher", "control");
            rejects_field(&metadata("Team", text, from_market), "version", "control");
        }
    }
}

#[test]
fn plugin_catalog_refuses_non_ascii_names() {
    let name = "caf\u{e9}";
    let catalog = one_plugin(json!({"name":"tools", "plugins":[{"name":name, "source":"plugins/tool"}]}), json!({"name":name}));
    assert!(catalog.plugins().is_empty());
    assert_eq!(catalog.rejected().len(), 1);
    assert_eq!(catalog.rejected()[0].name, name);
    assert!(catalog.rejected()[0].reason.contains("normal form"));
}

#[test]
fn fingerprint_tracks_entry_tree_and_manifest_not_json_whitespace_or_other_plugins() {
    let calculate = |description: &str, change_tree: bool, plugin: &[u8], pretty: bool| {
        let doc = json!({"name":"market","plugins":[{"name":"tool","source":"plugins/tool","description":description}]});
        let market = if pretty { serde_json::to_vec_pretty(&doc).unwrap() } else { serde_json::to_vec(&doc).unwrap() };
        let mut snapshot = listing(&[("marketplace.json", &market), ("plugins/tool/plugin.json", plugin)]);
        if change_tree {
            snapshot.entries.iter_mut().find(|e| e.path == "plugins/tool").unwrap().sha = "d".repeat(40);
        }
        let tree = CatalogTree::new(&snapshot).unwrap();
        let pending = tree.match_marketplace(&source(""), &market).unwrap();
        pending.finish(&[("plugins/tool/plugin.json".into(), plugin.to_vec())].into()).plugins()["tool"].fingerprint.clone()
    };
    let a = br#"{"name":"tool"}"#;
    let b = br#"{"name":"tool","x":true}"#;
    let original = calculate("A", false, a, false);
    assert_eq!(original, calculate("A", false, a, true));
    assert_ne!(original, calculate("B", false, a, false));
    assert_ne!(original, calculate("A", true, a, false));
    assert_ne!(original, calculate("A", false, b, false));
}
