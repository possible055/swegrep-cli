use super::*;
use crate::executor::InstantContextTiming;
use crate::path_filter::PathFilterConfig;
use crate::protobuf::{ProtobufEncoder, connect_frame_encode, gzip_compress};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[test]
fn decode_unary_response_decompresses_gzip() {
    let data = gzip_compress(b"proto-response").unwrap();
    assert_eq!(
        decode_unary_response(&data, Some("gzip")),
        b"proto-response"
    );
    assert_eq!(decode_unary_response(&data, None), b"proto-response");
}

#[test]
fn trim_messages_keeps_head_bridge_and_tail() {
    let mut messages = vec![
        ChatMessage::simple(5, "system"),
        ChatMessage::simple(1, "user"),
        ChatMessage::simple(2, "thinking 1"),
        ChatMessage::simple(4, "result 1"),
        ChatMessage::simple(2, "thinking 2"),
        ChatMessage::simple(4, "result 2"),
    ];

    assert!(trim_messages(&mut messages));
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[0].content, "system");
    assert_eq!(messages[1].content, "user");
    assert!(messages[2].content.contains("omitted"));
    assert_eq!(messages[3].content, "thinking 2");
    assert_eq!(messages[4].content, "result 2");
}

#[test]
fn build_system_prompt_uses_windsurf_context_subagent_contract() {
    let prompt = build_system_prompt(3, 6, 5);

    assert!(prompt.contains("You are an expert software engineer"));
    assert!(prompt.contains("Tool access: use the restricted_exec tool ONLY"));
    assert!(prompt.contains("/codebase"));
    assert!(prompt.contains("ANSWER FORMAT"));
    assert!(prompt.contains("at most 6 commands"));
    assert!(prompt.contains("at most 3 turns"));
    assert!(prompt.contains("Think step-by-step"));
    assert!(prompt.contains("DO NOT EVER USE MORE THAN 6 commands"));
    assert!(prompt.contains("ls: List files in a directory"));
    assert!(prompt.contains("glob: Find files matching a glob pattern"));
    assert!(prompt.contains("[TOOL_CALLS]restricted_exec[ARGS]"));
    assert!(!prompt.contains("task_boundary"));
    assert!(!prompt.contains("notify_user"));
}

#[test]
fn final_force_answer_prompt_matches_windsurf_source() {
    assert_eq!(
        FINAL_FORCE_ANSWER,
        "You have no turns left. Now you MUST provide your final ANSWER, even if it's not complete."
    );
    assert!(FINAL_FORCE_ANSWER.contains("MUST"));
    assert!(FINAL_FORCE_ANSWER.contains("not complete"));
}

#[test]
fn tool_definitions_expose_only_restricted_exec_and_answer() {
    let defs: serde_json::Value = serde_json::from_str(&get_tool_definitions(8)).unwrap();
    let tools = defs.as_array().unwrap();

    let names = tools
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["restricted_exec", "answer"]);

    let schema = &tools[0]["function"]["parameters"]["properties"];
    assert!(schema.get("command1").is_some());
    assert!(schema.get("command8").is_some());
    assert!(schema.get("command9").is_none());

    let serialized = serde_json::to_string(&defs).unwrap();
    assert!(serialized.contains("\"rg\""));
    assert!(serialized.contains("\"readfile\""));
    assert!(serialized.contains("\"tree\""));
    assert!(serialized.contains("\"ls\""));
    assert!(serialized.contains("\"glob\""));
}

#[test]
fn parse_response_extracts_text_tool_call() {
    let mut encoder = ProtobufEncoder::new();
    encoder.write_string(1, "thinking");
    encoder.write_string(
        2,
        r#"[TOOL_CALLS]restricted_exec[ARGS]{"command1":{"type":"rg","pattern":"main","path":"/codebase/src"}}"#,
    );
    let frame = connect_frame_encode(&encoder.to_bytes(), false);

    let ParsedModelTurn::ToolCalls { thinking, calls } = parse_response(&frame) else {
        panic!("expected tool call");
    };
    assert_eq!(thinking, "thinking");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "restricted_exec");
    assert_eq!(calls[0].args["command1"]["type"], "rg");
    assert_eq!(calls[0].args["command1"]["pattern"], "main");
}

#[test]
fn parse_response_extracts_structured_restricted_exec() {
    let frame = connect_frame_encode(
        br#"{"output":"thinking","tool_calls":[{"id":"a","name":"restricted_exec","args":{"command1":{"type":"glob","pattern":"**/*.rs","path":"/codebase","type_filter":"file"},"command2":{"type":"readfile","file":"/codebase/src/lib.rs","start_line":1,"end_line":20}}}]}"#,
        false,
    );

    let ParsedModelTurn::ToolCalls { thinking, calls } = parse_response(&frame) else {
        panic!("expected structured tool calls");
    };

    assert_eq!(thinking, "thinking");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "a");
    assert_eq!(calls[0].name, "restricted_exec");
    assert_eq!(calls[0].args["command1"]["type"], "glob");
    assert_eq!(calls[0].args["command2"]["type"], "readfile");
    assert_eq!(calls[0].args["command2"]["file"], "/codebase/src/lib.rs");
}

#[test]
fn parse_response_extracts_answer_tool_call() {
    let mut encoder = ProtobufEncoder::new();
    encoder.write_string(
        1,
        r#"[TOOL_CALLS]answer[ARGS]{"answer":"<ANSWER></ANSWER>"}"#,
    );
    let frame = connect_frame_encode(&encoder.to_bytes(), false);

    let ParsedModelTurn::ToolCalls { calls, .. } = parse_response(&frame) else {
        panic!("expected answer");
    };
    assert_eq!(calls[0].name, "answer");
    assert_eq!(calls[0].args["answer"], "<ANSWER></ANSWER>");
}

#[test]
fn parse_response_handles_error_frame() {
    let frame = connect_frame_encode(
        br#"{"error":{"code":"TIMEOUT","message":"request timed out"}}"#,
        false,
    );

    assert_eq!(
        parse_response(&frame),
        ParsedModelTurn::Error("[Error] TIMEOUT: request timed out".to_string())
    );
}

#[test]
fn parse_response_returns_text_without_tool_call() {
    let mut encoder = ProtobufEncoder::new();
    encoder.write_string(1, "plain model text");
    let frame = connect_frame_encode(&encoder.to_bytes(), false);

    assert_eq!(
        parse_response(&frame),
        ParsedModelTurn::Text("plain model text".to_string())
    );
}

#[test]
fn parse_answer_filters_path_traversal() {
    let xml = r#"
    Some thoughts first.
    <ANSWER>
      <file path="/codebase/src/main.py">
        <range>10-20</range>
        <range>30-40</range>
      </file>
      <file path="/codebase/tests/test_main.py">
        <range>1-5</range>
      </file>
      <file path="/codebase/../../etc/passwd">
        <range>1-2</range>
      </file>
    </ANSWER>
    "#;
    let tmp = TempDir::new().unwrap();
    let result = parse_answer(xml, tmp.path());
    assert_eq!(result.files.len(), 2);
    assert_eq!(result.files[0].path, "src/main.py");
    assert_eq!(result.files[0].ranges, vec![(10, 20), (30, 40)]);
    assert_eq!(result.files[1].path, "tests/test_main.py");
    assert_eq!(result.files[1].ranges, vec![(1, 5)]);
}

#[test]
fn range_map_merges_ranges_and_rejects_path_traversal() {
    let tmp = TempDir::new().unwrap();
    let mut range_map = RangeMap::default();
    range_map.add_range("/codebase/src/lib.rs", 10, 20);
    range_map.add_range("src/lib.rs", 18, 30);
    range_map.add_range("/codebase/src/lib.rs", 31, 35);
    range_map.add_range("/codebase/../../etc/passwd", 1, 2);
    range_map.add_range("/codebase/src/main.rs", 2, 2);

    let result = range_map.to_result(tmp.path(), 10);

    assert_eq!(result.files.len(), 2);
    assert_eq!(result.files[0].path, "src/lib.rs");
    assert_eq!(result.files[0].ranges, vec![(10, 35)]);
    assert_eq!(result.files[1].path, "src/main.rs");
    assert_eq!(result.files[1].ranges, vec![(2, 2)]);
}

#[test]
fn parse_range_map_answer_merges_final_xml_and_limits_results() {
    let tmp = TempDir::new().unwrap();
    let mut range_map = RangeMap::default();
    range_map.add_range("/codebase/src/a.rs", 1, 3);
    range_map.add_range("/codebase/src/b.rs", 5, 6);

    let result = parse_range_map_answer(
        r#"<ANSWER><file path="/codebase/src/a.rs"><range>4-8</range></file><file path="/codebase/src/c.rs"><range>1-1</range></file></ANSWER>"#,
        tmp.path(),
        &range_map,
        2,
    );

    assert_eq!(result.files.len(), 2);
    assert_eq!(result.files[0].path, "src/a.rs");
    assert_eq!(result.files[0].ranges, vec![(1, 8)]);
    assert_eq!(result.files[1].path, "src/b.rs");
}

#[test]
fn build_user_content_includes_compact_scope_snapshot() {
    let repo_map = RepoMap {
        tree: "/codebase\nCargo.toml\nsrc/\nsrc/lib.rs".to_string(),
        depth: 2,
        size_bytes: 35,
        fell_back: false,
    };

    let content = build_user_content("find parser", &repo_map);

    assert!(content.contains("Workspace scope snapshot (depth 2):"));
    assert!(content.contains("/codebase"));
    assert!(content.contains("Cargo.toml"));
    assert!(content.contains("src/"));
    assert!(content.contains(r#"Please find the code context for the query: "(find parser)""#));
    assert!(content.contains(r#"Constrain search to directory: "(/codebase)""#));
}

#[test]
fn get_repo_map_uses_untruncated_manifest() {
    let tmp = TempDir::new().unwrap();
    for i in 0..60 {
        fs::write(tmp.path().join(format!("file_{i:03}.txt")), "").unwrap();
    }

    let result = get_repo_map(tmp.path(), 1, &PathFilterConfig::default());
    assert!(!result.tree.contains("... (lines truncated) ..."));
    assert!(result.tree.contains("file_059.txt"));
    assert_eq!(result.size_bytes, result.tree.len());
}

#[test]
fn get_repo_map_falls_back_to_compact_scope_snapshot() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join("src")).unwrap();
    for i in 0..6_000 {
        fs::write(
            tmp.path().join(format!(
                "very_long_file_name_for_scope_snapshot_padding_{i:05}.txt"
            )),
            "",
        )
        .unwrap();
    }
    fs::write(tmp.path().join("src").join("lib.rs"), "").unwrap();

    let result = get_repo_map(tmp.path(), 4, &PathFilterConfig::default());

    assert!(result.size_bytes <= MAX_TREE_BYTES);
    assert!(result.fell_back);
    assert!(result.tree.starts_with("/codebase"));
    assert!(result.tree.lines().any(|line| line == "src/"));
}

#[test]
fn get_repo_map_uses_manifest_path_format() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join("src")).unwrap();
    fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
    fs::write(tmp.path().join("src").join("main.rs"), "").unwrap();

    let result = get_repo_map(tmp.path(), 2, &PathFilterConfig::default());
    let lines = result.tree.lines().collect::<Vec<_>>();

    assert_eq!(lines[0], "/codebase");
    assert!(lines.contains(&"Cargo.toml"));
    assert!(lines.contains(&"src/"));
    assert!(lines.contains(&"src/main.rs"));
    assert!(!lines.contains(&"/codebase/src/main.rs"));
}

#[test]
fn restricted_exec_results_hide_internal_status_timing_and_range_map() {
    let updates = vec![
        InstantContextToolUpdate {
            step_id: "step-1".to_string(),
            tool_call_id: "command1".to_string(),
            tool_name: "command1".to_string(),
            command: serde_json::json!({"type": "rg", "pattern": "main", "path": "/codebase"}),
            status: ToolExecutionStatus::Completed,
            output: "/codebase/src/main.rs:1:fn main() {}".to_string(),
            timing: InstantContextTiming { duration_ms: 12 },
        },
        InstantContextToolUpdate {
            step_id: "step-1".to_string(),
            tool_call_id: "command2".to_string(),
            tool_name: "command2".to_string(),
            command: serde_json::json!({"type": "readfile", "file": "/codebase/src/lib.rs"}),
            status: ToolExecutionStatus::TimedOut,
            output: "Error: tool timed out".to_string(),
            timing: InstantContextTiming { duration_ms: 1_000 },
        },
    ];

    let output = format_restricted_exec_results(&updates);

    assert!(output.contains("command1_result:\n/codebase/src/main.rs:1:fn main() {}"));
    assert!(output.contains("command2_result:\nError: tool timed out"));
    assert!(!output.contains("status="));
    assert!(!output.contains("duration_ms="));
    assert!(!output.contains("range_map:"));
    assert!(!output.contains("<range_map>"));
}

#[test]
fn jwt_exp_decodes_payload() {
    let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":12345}"#);
    assert_eq!(get_jwt_exp(&format!("header.{payload}.sig")), 12345.0);
    assert_eq!(get_jwt_exp("not-a-jwt"), 0.0);
}

#[tokio::test]
async fn search_loop_success_with_mock_streaming() {
    let mut t1_encoder = ProtobufEncoder::new();
    t1_encoder.write_string(1, "thinking about doing search");
    t1_encoder.write_string(
        2,
        r#"[TOOL_CALLS]restricted_exec[ARGS]{"command1":{"type":"readfile","file":"/codebase/test.txt"}}"#,
    );
    let t1_frame = connect_frame_encode(&t1_encoder.to_bytes(), false);

    let mut t2_encoder = ProtobufEncoder::new();
    t2_encoder.write_string(1, "found answer");
    t2_encoder.write_string(
        2,
        r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>1-10</range></file></ANSWER>"}"#,
    );
    let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

    let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, t2_frame])));
    let call_count = Arc::new(AtomicUsize::new(0));
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("test.txt"), "line1\nline2").unwrap();

    let mut options = SearchOptions::new("find main", tmp.path());
    options.api_key = Some("sk-ws-01-key".to_string());
    options.jwt = Some("mocked.jwt.token".to_string());
    options.max_turns = 2;

    let result = search_with_streaming(options, None, {
        let responses = Arc::clone(&responses);
        let call_count = Arc::clone(&call_count);
        move |_, _, _, _| {
            let responses = Arc::clone(&responses);
            let call_count = Arc::clone(&call_count);
            async move {
                call_count.fetch_add(1, Ordering::SeqCst);
                Ok(responses.lock().unwrap().pop_front().unwrap())
            }
        }
    })
    .await;

    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, "test.txt");
    assert_eq!(result.files[0].ranges, vec![(1, 10)]);
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn search_loop_supports_restricted_exec_then_answer() {
    let t1_frame = connect_frame_encode(
        br#"{"output":"find candidates","tool_calls":[{"id":"q1","name":"restricted_exec","args":{"command1":{"type":"glob","pattern":"test.txt","path":"/codebase","type_filter":"file"},"command2":{"type":"readfile","file":"/codebase/test.txt","start_line":1,"end_line":1}}}]}"#,
        false,
    );

    let mut t2_encoder = ProtobufEncoder::new();
    t2_encoder.write_string(
        1,
        r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER><file path=\"/codebase/test.txt\"><range>1-1</range></file></ANSWER>"}"#,
    );
    let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

    let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, t2_frame])));
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("test.txt"), "line1").unwrap();

    let mut options = SearchOptions::new("find test", tmp.path());
    options.api_key = Some("sk-ws-01-key".to_string());
    options.jwt = Some("mocked.jwt.token".to_string());
    options.max_turns = 2;

    let result = search_with_streaming(options, None, {
        let responses = Arc::clone(&responses);
        move |_, _, _, _| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().unwrap().pop_front().unwrap()) }
        }
    })
    .await;

    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, "test.txt");
    assert_eq!(result.files[0].ranges, vec![(1, 1)]);
}

#[tokio::test]
async fn search_loop_keeps_range_map_internal_but_merges_final_result() {
    let t1_frame = connect_frame_encode(
        br#"{"output":"read context","tool_calls":[{"id":"q1","name":"restricted_exec","args":{"command1":{"type":"readfile","file":"/codebase/test.txt","start_line":1,"end_line":2}}}]}"#,
        false,
    );

    let mut t2_encoder = ProtobufEncoder::new();
    t2_encoder.write_string(
        1,
        r#"[TOOL_CALLS]answer[ARGS]{"answer": "<ANSWER></ANSWER>"}"#,
    );
    let t2_frame = connect_frame_encode(&t2_encoder.to_bytes(), false);

    let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame, t2_frame])));
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("test.txt"), "line1\nline2\nline3").unwrap();

    let mut options = SearchOptions::new("find test", tmp.path());
    options.api_key = Some("sk-ws-01-key".to_string());
    options.jwt = Some("mocked.jwt.token".to_string());
    options.max_turns = 2;

    let result = search_with_streaming(options, None, {
        let responses = Arc::clone(&responses);
        let requests = Arc::clone(&requests);
        move |proto, _, _, _| {
            let responses = Arc::clone(&responses);
            let requests = Arc::clone(&requests);
            async move {
                requests
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&proto).to_string());
                Ok(responses.lock().unwrap().pop_front().unwrap())
            }
        }
    })
    .await;

    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, "test.txt");
    assert_eq!(result.files[0].ranges, vec![(1, 2)]);

    let requests = requests.lock().unwrap();
    assert!(requests[0].contains("Workspace scope snapshot (depth "));
    assert!(requests[0].contains("/codebase"));
    assert!(requests[0].contains("test.txt"));
    assert!(requests[1].contains("command1_result:"));
    assert!(!requests[1].contains("status="));
    assert!(!requests[1].contains("duration_ms="));
    assert!(!requests[1].contains("range_map:"));
    assert!(!requests[1].contains("<range_map>"));
}

#[tokio::test]
async fn search_loop_accepts_plain_text_answer() {
    let mut t1_encoder = ProtobufEncoder::new();
    t1_encoder.write_string(
        1,
        r#"<ANSWER><file path="/codebase/test.txt"><range>1-1</range></file></ANSWER>"#,
    );
    let t1_frame = connect_frame_encode(&t1_encoder.to_bytes(), false);

    let responses = Arc::new(Mutex::new(VecDeque::from([t1_frame])));
    let call_count = Arc::new(AtomicUsize::new(0));
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("test.txt"), "line1").unwrap();

    let mut options = SearchOptions::new("find main", tmp.path());
    options.api_key = Some("sk-ws-01-key".to_string());
    options.jwt = Some("mocked.jwt.token".to_string());
    options.max_turns = 2;

    let result = search_with_streaming(options, None, {
        let responses = Arc::clone(&responses);
        let call_count = Arc::clone(&call_count);
        move |_, _, _, _| {
            let responses = Arc::clone(&responses);
            let call_count = Arc::clone(&call_count);
            async move {
                call_count.fetch_add(1, Ordering::SeqCst);
                Ok(responses.lock().unwrap().pop_front().unwrap())
            }
        }
    })
    .await;

    assert_eq!(result.files.len(), 1);
    assert_eq!(result.files[0].path, "test.txt");
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[test]
fn check_auth_success_with_explicit_key() {
    let result = check_auth(Some("fake-api-key"), None, None);
    assert!(result.ok);
    assert_eq!(result.jwt_source, "api-key");
}
