use std::borrow::Cow;

use schemars::generate::SchemaSettings;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Unexpected, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Config, Decision, Event, RepoRecord, Task};

/// The schema marker written into every top-level state file.
pub const SCHEMA_ID: &str = "conductor-state.v1";

/// The `"schema": "conductor-state.v1"` marker. Serializes as [`SCHEMA_ID`] and refuses to
/// deserialize anything else, so a reader never misreads a file from an incompatible version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SchemaV1;

impl Serialize for SchemaV1 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(SCHEMA_ID)
    }
}

impl<'de> Deserialize<'de> for SchemaV1 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MarkerVisitor;

        impl Visitor<'_> for MarkerVisitor {
            type Value = SchemaV1;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "the string {SCHEMA_ID:?}")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<SchemaV1, E> {
                if v == SCHEMA_ID {
                    Ok(SchemaV1)
                } else {
                    Err(E::invalid_value(Unexpected::Str(v), &self))
                }
            }
        }

        deserializer.deserialize_str(MarkerVisitor)
    }
}

impl JsonSchema for SchemaV1 {
    fn schema_name() -> Cow<'static, str> {
        "SchemaV1".into()
    }

    fn inline_schema() -> bool {
        true
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Schema version marker for top-level state files.",
            "type": "string",
            "const": SCHEMA_ID,
        })
    }
}

/// Generates the JSON Schema covering every state type. `Task`, `Event`, `Decision` and
/// `RepoRecord` are the root alternatives; `Config` is only under `$defs` since `config.toml`
/// isn't JSON.
#[must_use]
pub fn json_schema() -> serde_json::Value {
    let mut generator = SchemaSettings::draft2020_12().into_generator();
    let roots = [
        generator.subschema_for::<Task>(),
        generator.subschema_for::<Event>(),
        generator.subschema_for::<Decision>(),
        generator.subschema_for::<RepoRecord>(),
    ];
    generator.subschema_for::<Config>();

    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$id": SCHEMA_ID,
        "title": SCHEMA_ID,
        "description": "conductor's on-disk state: task.json, events.jsonl lines, decisions/<key>.json, repo.json and config.toml.",
        "anyOf": roots,
        "$defs": generator.take_definitions(true),
    })
}
