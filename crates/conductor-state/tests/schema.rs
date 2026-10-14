use std::path::Path;
use std::{env, fs};

const REGENERATE: &str = "UPDATE_SCHEMA=1 cargo test -p conductor-state --test schema";

#[test]
fn committed_schema_is_current() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/conductor-state.v1.json");
    let generated = serde_json::to_string_pretty(&conductor_state::json_schema()).unwrap() + "\n";

    if env::var_os("UPDATE_SCHEMA").is_some() {
        fs::write(&path, &generated).unwrap();
        return;
    }

    let committed = fs::read_to_string(&path).unwrap_or_default();
    assert!(
        committed == generated,
        "{} is stale; regenerate it with `{REGENERATE}`",
        path.display()
    );
}

#[test]
fn schema_marker_is_const() {
    let schema = conductor_state::json_schema();
    for def in ["Task", "Decision", "RepoRecord"] {
        assert_eq!(
            schema["$defs"][def]["properties"]["schema"]["const"],
            conductor_state::SCHEMA_ID,
            "{def}"
        );
    }
}
