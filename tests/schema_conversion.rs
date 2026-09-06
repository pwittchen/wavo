//! MCP → OpenAI schema conversion against the shape `demix-mcp` actually emits.
//!
//! FastMCP derives the schema from the Python signature, so `Optional[str]`
//! arrives as an `anyOf` with a null branch and `Literal[...]` as an enum. The
//! conversion has to carry all of that through untouched while removing the two
//! arguments wavo fills in itself (§6.3).

use serde_json::{json, Value};
use wavo::mcp::schema::{accepts, strip_injected_args, to_openai_tool};

/// The `process_audio` schema as FastMCP generates it from demix-mcp's signature.
fn process_audio_schema() -> Value {
    json!({
        "type": "object",
        "title": "process_audioArguments",
        "properties": {
            "file": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null, "title": "File"},
            "url": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null, "title": "Url"},
            "search": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null, "title": "Search"},
            "mode": {
                "enum": ["nosplit", "2stems", "4stems", "5stems"],
                "default": "nosplit",
                "title": "Mode",
                "type": "string"
            },
            "output_dir": {"default": "output", "title": "Output Dir", "type": "string"},
            "tempo": {"default": 1.0, "title": "Tempo", "type": "number"},
            "transpose": {"default": 0, "title": "Transpose", "type": "integer"},
            "target_key": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null},
            "detect_key": {"default": false, "title": "Detect Key", "type": "boolean"},
            "start": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null},
            "end": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null},
            "video": {"default": false, "title": "Video", "type": "boolean"},
            "cwd": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null, "title": "Cwd"}
        }
    })
}

#[test]
fn the_conversion_is_a_rename_plus_the_two_removals() {
    let tool = to_openai_tool(
        "process_audio",
        Some("Process an audio source with demix."),
        &process_audio_schema(),
    );

    assert_eq!(tool.kind, "function");
    assert_eq!(tool.function.name, "process_audio");
    assert_eq!(
        tool.function.description.as_deref(),
        Some("Process an audio source with demix.")
    );

    let properties = tool.function.parameters["properties"].as_object().unwrap();
    assert!(!properties.contains_key("cwd"), "the model can pick a cwd");
    assert!(
        !properties.contains_key("output_dir"),
        "the model can pick where files land"
    );

    // Everything the model legitimately needs survives, defaults and all.
    for expected in [
        "file",
        "url",
        "search",
        "mode",
        "tempo",
        "transpose",
        "target_key",
        "detect_key",
        "start",
        "end",
        "video",
    ] {
        assert!(properties.contains_key(expected), "{expected} was dropped");
    }
    assert_eq!(properties["mode"]["enum"][1], "2stems");
    assert_eq!(properties["mode"]["default"], "nosplit");
    assert_eq!(properties["tempo"]["default"], 1.0);
    assert_eq!(properties["file"]["anyOf"][1]["type"], "null");
}

#[test]
fn the_result_serializes_into_the_openai_tools_array() {
    let tool = to_openai_tool(
        "search_youtube",
        Some("Find a video."),
        &json!({
            "type": "object",
            "properties": {"query": {"type": "string", "title": "Query"}},
            "required": ["query"]
        }),
    );

    let serialized = serde_json::to_value(&tool).unwrap();
    assert_eq!(serialized["type"], "function");
    assert_eq!(serialized["function"]["name"], "search_youtube");
    assert_eq!(serialized["function"]["parameters"]["required"][0], "query");
    assert_eq!(
        serialized["function"]["parameters"]["properties"]["query"]["type"],
        "string"
    );
}

#[test]
fn a_required_injected_argument_is_removed_from_required_too() {
    // A future demix-mcp could make output_dir mandatory; leaving it in `required`
    // would make every call the model writes invalid.
    let schema = json!({
        "type": "object",
        "properties": {"url": {"type": "string"}, "output_dir": {"type": "string"}},
        "required": ["url", "output_dir"]
    });
    let stripped = strip_injected_args(&schema);
    assert_eq!(stripped["required"].as_array().unwrap().len(), 1);
    assert_eq!(stripped["required"][0], "url");
}

#[test]
fn wavo_knows_which_tools_take_the_arguments_it_injects() {
    assert!(accepts(&process_audio_schema(), "cwd"));
    assert!(accepts(&process_audio_schema(), "output_dir"));

    // `detect_key(file)` takes neither, so nothing is injected into it.
    let detect_key =
        json!({"type": "object", "properties": {"file": {"type": "string"}}, "required": ["file"]});
    assert!(!accepts(&detect_key, "cwd"));
    assert!(!accepts(&detect_key, "output_dir"));
    assert_eq!(strip_injected_args(&detect_key), detect_key);
}
