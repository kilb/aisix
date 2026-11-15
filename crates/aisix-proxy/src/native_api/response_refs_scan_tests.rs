//! 优化前后协议投影的差分控制，不替代真实请求线格式验证。

use super::*;
use serde_json::json;

#[test]
fn readonly_projection_matches_the_existing_formal_slot_review() {
    let nodes = [
        json!({"type":"item_reference","id":"item_one"}),
        json!({"type":"input_file","file_id":"file_one"}),
        json!({"type":"input_image","file_id":"file_one"}),
        json!({"type":"computer_screenshot","file_id":"file_one"}),
        json!({"type":"code_interpreter_call","container_id":"cntr_one"}),
        json!({"type":"container_reference","container_id":"cntr_one"}),
        json!({"type":"skill_reference","skill_id":"skill_one"}),
        json!({"type":"code_interpreter","container":"cntr_one"}),
        json!({"type":"code_interpreter","container":{"type":"auto","file_ids":["file_one",7]}}),
        json!({"type":"file_search","vector_store_ids":["vs_one","vs_one"]}),
        json!({"type":"container_auto","file_ids":["file_one"]}),
        json!({"type":"image_generation","input_image_mask":{"file_id":"file_one"}}),
    ];
    for node in nodes {
        for value in [
            node.clone(),
            json!([node.clone(), node.clone()]),
            json!({"type":"message","content":[node.clone()]}),
            json!({"type":"computer_call_output","output":[node.clone()]}),
            json!({"type":"shell_call","environment":node.clone()}),
            json!({"type":"container_auto","skills":[node.clone()]}),
            json!({"type":"allowed_tools","tools":[node]}),
        ] {
            for field in ["input", "tools", "tool_choice"] {
                let body = json!({(field):value,"conversation":{"id":"conv_one"}});
                let scan = ReferenceScan::collect(&body);
                let mut checked = body.clone();
                let expected = take_references(&mut checked);
                assert!(
                    expected.len() > 1,
                    "corpus must exercise a nested reference: {body}"
                );
                let actual: Vec<_> = scan
                    .references
                    .iter()
                    .map(|&(kind, raw)| (kind, raw.to_owned()))
                    .collect();
                assert_eq!(actual, expected, "{body}");
                assert!(scan.needs_review_copy, "{body}");
            }
        }
    }
    for kind in ["function", "custom", "tool_search_call", "mcp_list_tools"] {
        let body = json!({"input":[{"type":kind,"content":[{
            "type":"input_file","file_id":"opaque_data"
        }]}],"metadata":{"type":"item_reference","id":"opaque_metadata"}});
        let scan = ReferenceScan::collect(&body);
        assert!(scan.references.is_empty());
        assert!(!scan.needs_review_copy);
    }
}

#[test]
fn empty_or_non_string_formal_arrays_keep_the_existing_review_copy() {
    for ids in [json!([]), json!([null,7,{"file_id":"opaque"}])] {
        for tool in [
            json!({"type":"file_search","vector_store_ids":ids.clone()}),
            json!({"type":"code_interpreter","container":{"file_ids":ids.clone()}}),
            json!({"type":"shell","environment":{"type":"container_auto","file_ids":ids}}),
        ] {
            let body = json!({"tools":[tool]});
            let scan = ReferenceScan::collect(&body);
            assert!(scan.references.is_empty());
            assert!(scan.needs_review_copy);
            let mut checked = body.clone();
            assert!(take_references(&mut checked).is_empty());
            assert_ne!(checked, body);
        }
    }
}

#[test]
fn untyped_direct_container_slots_keep_the_existing_seal_review() {
    for item in [
        json!({"container_id":"legacy_container_seal"}),
        json!({"environment":{"container_id":"legacy_container_seal"}}),
    ] {
        let body = json!({"input":[item]});
        let scan = ReferenceScan::collect(&body);
        assert!(scan.references.is_empty());
        assert!(scan.needs_review_copy);
    }
}
