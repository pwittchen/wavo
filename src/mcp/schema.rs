//! MCP tool schemas are JSON Schema already, so exposing them to the model is a
//! mechanical rename — plus the one substantive edit of §6.3: the arguments that
//! decide *where files land* are removed from the schema and filled in by wavo.

use serde_json::{json, Map, Value};

use crate::llm::openai::{FunctionDef, ToolDef};

/// Arguments the model never gets to choose (§12). demix resolves output paths
/// relative to `cwd`, so both have to go.
pub const INJECTED_ARGS: [&str; 2] = ["cwd", "output_dir"];

/// Convert one MCP tool description into an OpenAI function definition.
pub fn to_openai_tool(name: &str, description: Option<&str>, input_schema: &Value) -> ToolDef {
    ToolDef {
        kind: "function",
        function: FunctionDef {
            name: name.to_string(),
            description: description.map(str::to_string),
            parameters: strip_injected_args(input_schema),
        },
    }
}

/// The same schema without the injected arguments, in `properties` and in
/// `required` alike. Anything that is not an object schema is passed through
/// unchanged — an MCP server is free to describe a tool that takes nothing.
pub fn strip_injected_args(input_schema: &Value) -> Value {
    let Some(object) = input_schema.as_object() else {
        return json!({ "type": "object", "properties": {} });
    };

    let mut schema: Map<String, Value> = object.clone();

    if let Some(Value::Object(properties)) = schema.get_mut("properties") {
        for name in INJECTED_ARGS {
            properties.remove(name);
        }
    }

    if let Some(Value::Array(required)) = schema.get_mut("required") {
        required.retain(|value| match value.as_str() {
            Some(name) => !INJECTED_ARGS.contains(&name),
            None => true,
        });
    }

    // A tool description without a `type` confuses some OpenAI-compatible servers.
    schema
        .entry("type")
        .or_insert_with(|| Value::String("object".to_string()));
    schema
        .entry("properties")
        .or_insert_with(|| Value::Object(Map::new()));

    Value::Object(schema)
}

/// True when the original schema declared an argument, i.e. when injecting a
/// value for it is meaningful for this tool.
pub fn accepts(input_schema: &Value, argument: &str) -> bool {
    input_schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(argument))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process_audio_schema() -> Value {
        json!({
            "type": "object",
            "title": "process_audioArguments",
            "properties": {
                "url": {"type": "string", "title": "Url"},
                "search": {"type": "string", "title": "Search"},
                "mode": {"enum": ["nosplit", "2stems"], "default": "nosplit"},
                "output_dir": {"type": "string", "default": "output"},
                "cwd": {"type": "string"}
            },
            "required": ["mode", "output_dir", "cwd"]
        })
    }

    #[test]
    fn the_model_never_sees_the_path_arguments() {
        let converted = to_openai_tool(
            "process_audio",
            Some("does things"),
            &process_audio_schema(),
        );

        assert_eq!(converted.kind, "function");
        assert_eq!(converted.function.name, "process_audio");
        assert_eq!(
            converted.function.description.as_deref(),
            Some("does things")
        );

        let properties = converted.function.parameters["properties"]
            .as_object()
            .unwrap();
        assert!(properties.contains_key("url"));
        assert!(properties.contains_key("mode"));
        assert!(!properties.contains_key("cwd"));
        assert!(!properties.contains_key("output_dir"));

        let required = converted.function.parameters["required"]
            .as_array()
            .unwrap();
        assert_eq!(required, &vec![json!("mode")]);
    }

    #[test]
    fn everything_else_about_the_schema_is_preserved() {
        let converted = strip_injected_args(&process_audio_schema());
        assert_eq!(converted["type"], "object");
        assert_eq!(converted["title"], "process_audioArguments");
        assert_eq!(converted["properties"]["mode"]["default"], "nosplit");
    }

    #[test]
    fn a_schema_without_the_injected_arguments_is_untouched() {
        let schema = json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]});
        assert_eq!(strip_injected_args(&schema), schema);
    }

    #[test]
    fn a_missing_or_odd_schema_still_produces_a_valid_object_schema() {
        assert_eq!(
            strip_injected_args(&Value::Null),
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            strip_injected_args(&json!({})),
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn injection_targets_are_recognised_from_the_original_schema() {
        assert!(accepts(&process_audio_schema(), "cwd"));
        assert!(accepts(&process_audio_schema(), "output_dir"));
        assert!(!accepts(&json!({"properties": {"file": {}}}), "cwd"));
    }
}
