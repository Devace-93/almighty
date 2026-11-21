//! Every manifest shipped in `plugins/` must be accepted by the parser.

use almighty_server::plugins::manifest::Manifest;

#[test]
fn shipped_manifests_parse() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
    let mut found = 0;
    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().is_some_and(|n| n != "target" && n != "src") {
                dirs.push(path);
            } else if path.file_name().is_some_and(|n| n == "almighty-plugin.toml") {
                let text = std::fs::read_to_string(&path).unwrap();
                Manifest::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                found += 1;
            }
        }
    }
    assert!(found >= 9, "found {found} manifests");
}
