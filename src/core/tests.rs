use super::*;
use crate::path_filter::PathFilterConfig;
use crate::protobuf::{ProtobufEncoder, connect_frame_encode, gzip_compress};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;
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
fn limit_tool_args_enforces_max_commands() {
    let tool_args = json!({
        "command3": {"type": "rg"},
        "command1": {"type": "tree"},
        "command2": {"type": "readfile"},
        "command10": {"type": "ls"}
    });

    assert_eq!(
        limit_tool_args(&tool_args, 2),
        json!({
            "command1": {"type": "tree"},
            "command2": {"type": "readfile"}
        })
    );
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
fn get_repo_map_uses_untruncated_tree() {
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
        r#"[TOOL_CALLS]restricted_exec[ARGS]{"command1": {"type": "readfile", "file": "/codebase/test.txt"}}"#,
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

#[test]
fn check_auth_success_with_explicit_key() {
    let result = check_auth(Some("fake-api-key"), None, None);
    assert!(result.ok);
    assert_eq!(result.jwt_source, "api-key");
}
