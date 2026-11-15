use super::*;
use std::sync::Arc;

#[test]
fn cached_metric_order_preserves_total_cmp_and_stable_ties() {
    // 普通浮点比较会把 NaN、正负零混成 tie；缓存排序也不能打乱同键声明顺序。
    for larger in [false, true] {
        let mut names = vec![
            "nan",
            "tie-a",
            "minus-zero",
            "unknown",
            "tie-b",
            "plus-zero",
            "infinity",
            "negative-nan",
        ];
        let mut expected = vec![
            "negative-nan",
            "unknown",
            "minus-zero",
            "plus-zero",
            "tie-a",
            "tie-b",
        ];
        if larger {
            names.extend([
                "nine", "seven", "five", "three", "eight", "six", "four", "two",
            ]);
            expected.extend([
                "two", "three", "four", "five", "six", "seven", "eight", "nine",
            ]);
        }
        expected.extend(["infinity", "nan"]);
        let model: Model = serde_json::from_str(r#"{"display_name":"target","provider":"openai","model_name":"upstream","provider_key_id":"pk"}"#).unwrap();
        let mut attempts: Vec<_> = names
            .iter()
            .map(|name| AttemptModel {
                id: (*name).to_string(),
                model: Arc::new(model.clone()),
                priority: 0,
                weight: 1,
            })
            .collect();
        let mut reads = vec![0usize; names.len()];
        sort_by_metric_key(&mut attempts, |attempt| {
            let index = names.iter().position(|name| *name == attempt.id).unwrap();
            reads[index] += 1;
            match attempt.id.as_str() {
                "negative-nan" => f64::from_bits(0xfff8_0000_0000_0001),
                "unknown" => f64::NEG_INFINITY,
                "minus-zero" => -0.0,
                "plus-zero" => 0.0,
                "tie-a" | "tie-b" => 1.0,
                "two" => 2.0,
                "three" => 3.0,
                "four" => 4.0,
                "five" => 5.0,
                "six" => 6.0,
                "seven" => 7.0,
                "eight" => 8.0,
                "nine" => 9.0,
                "infinity" => f64::INFINITY,
                "nan" => f64::from_bits(0x7ff8_0000_0000_0001),
                _ => unreachable!(),
            }
        });
        assert_eq!(
            attempts.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            expected
        );
        assert!(
            reads.iter().all(|read| *read == 1),
            "metric reads={reads:?}"
        );
    }
}
