//! 短请求的确定性样本检查；通过不证明模型身份、完整能力或长期表现。

use std::fmt;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

pub struct Challenge {
    nonce: String,
    a: i64,
    b: i64,
    c: i64,
    numbers: [i64; 5],
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Checks {
    pub json: Option<bool>,
    pub instruction: Option<bool>,
    pub arithmetic: Option<bool>,
    pub sorting: Option<bool>,
}

impl Challenge {
    pub fn new() -> Self {
        let random = Uuid::new_v4();
        let bytes = random.as_bytes();
        Self {
            nonce: random.simple().to_string()[..16].to_owned(),
            a: 10 + i64::from(bytes[8] % 80),
            b: 2 + i64::from(bytes[9] % 8),
            c: i64::from(bytes[10] % 41) - 20,
            numbers: [
                20 + i64::from(bytes[11] % 40),
                -1 - i64::from(bytes[12] % 20),
                1 + i64::from(bytes[13] % 9),
                0,
                60 + i64::from(bytes[14] % 40),
            ],
        }
    }

    pub fn prompt(&self) -> String {
        format!(
            "Return only one JSON object, no markdown or other text. \
             Use exactly these keys once each: \"nonce\", \"arithmetic\", \"sorting\". \
             \"nonce\" must be the exact string \"{}\". \
             \"arithmetic\" must be the integer result of ({} * {}) + {}. \
             \"sorting\" must be an array of integers sorting {:?} in ascending numeric order.",
            self.nonce, self.a, self.b, self.c, self.numbers,
        )
    }

    pub fn evaluate(&self, output: &str) -> Checks {
        let Ok(UniqueJson(Value::Object(object))) = serde_json::from_str(output) else {
            return Checks {
                json: Some(false),
                ..Checks::default()
            };
        };
        let arithmetic = object.get("arithmetic").and_then(Value::as_i64);
        let sorting = object
            .get("sorting")
            .and_then(Value::as_array)
            .and_then(|values| values.iter().map(Value::as_i64).collect::<Option<Vec<_>>>());
        let mut expected_sorting = self.numbers;
        expected_sorting.sort_unstable();
        Checks {
            json: Some(true),
            instruction: Some(
                object.len() == 3
                    && object.get("nonce").and_then(Value::as_str) == Some(self.nonce.as_str())
                    && arithmetic.is_some()
                    && sorting.is_some(),
            ),
            arithmetic: arithmetic.map(|answer| answer == self.a * self.b + self.c),
            sorting: sorting.map(|answer| answer == expected_sorting),
        }
    }
}

impl Default for Challenge {
    fn default() -> Self {
        Self::new()
    }
}

impl Checks {
    pub fn all_passed(&self) -> bool {
        [self.json, self.instruction, self.arithmetic, self.sorting]
            .iter()
            .all(|check| *check == Some(true))
    }
}

// Value 会吞掉重复键；递归构造值，让嵌套对象也遵守同一检查。
struct UniqueJson(Value);

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct JsonVisitor;

        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = UniqueJson;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON with unique object keys")
            }

            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Bool(value)))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::from(value)))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::from(value)))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::from(value)))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(value.to_owned())))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(UniqueJson(value)) = access.next_element()? {
                    values.push(value);
                }
                Ok(UniqueJson(Value::Array(values)))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut object = Map::new();
                while let Some((key, UniqueJson(value))) =
                    access.next_entry::<String, UniqueJson>()?
                {
                    if object.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate object key"));
                    }
                }
                Ok(UniqueJson(Value::Object(object)))
            }
        }

        deserializer.deserialize_any(JsonVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn challenge() -> Challenge {
        Challenge {
            nonce: "ab01cd23ef45ab67".to_string(),
            a: 17,
            b: 6,
            c: -9,
            numbers: [21, -7, 3, 0, 65],
        }
    }

    const CORRECT: &str =
        r#"{"nonce":"ab01cd23ef45ab67","arithmetic":93,"sorting":[-7,0,3,21,65]}"#;

    #[test]
    fn correct_answer_passes_every_check() {
        let checks = challenge().evaluate(CORRECT);
        assert_eq!(
            checks,
            Checks {
                json: Some(true),
                instruction: Some(true),
                arithmetic: Some(true),
                sorting: Some(true),
            }
        );
        assert!(checks.all_passed());
    }

    #[test]
    fn nonce_must_match_exactly() {
        for nonce in [
            "other",
            "prefix-ab01cd23ef45ab67-suffix",
            "AB01CD23EF45AB67",
        ] {
            let answer = json!({"nonce": nonce, "arithmetic": 93, "sorting": [-7, 0, 3, 21, 65]});
            let checks = challenge().evaluate(&answer.to_string());
            assert_eq!(checks.json, Some(true));
            assert_eq!(checks.instruction, Some(false));
            assert_eq!(checks.arithmetic, Some(true));
            assert_eq!(checks.sorting, Some(true));
            assert!(!checks.all_passed());
        }
    }

    #[test]
    fn wrong_arithmetic_does_not_fail_other_checks() {
        let checks = challenge().evaluate(&CORRECT.replace(":93,", ":94,"));
        assert_eq!(checks.json, Some(true));
        assert_eq!(checks.instruction, Some(true));
        assert_eq!(checks.arithmetic, Some(false));
        assert_eq!(checks.sorting, Some(true));
        assert!(!checks.all_passed());
    }

    #[test]
    fn sorting_requires_exact_multiset_and_numeric_order() {
        for numbers in [
            vec![65, 21, 3, 0, -7],
            vec![-7, 0, 21, 3, 65],
            vec![-7, 0, 3, 21],
            vec![-7, 0, 3, 21, 65, 65],
            vec![-7, 0, 3, 21, 64],
        ] {
            let answer = json!({"nonce": "ab01cd23ef45ab67", "arithmetic": 93, "sorting": numbers});
            let checks = challenge().evaluate(&answer.to_string());
            assert_eq!(checks.json, Some(true));
            assert_eq!(checks.instruction, Some(true));
            assert_eq!(checks.arithmetic, Some(true));
            assert_eq!(checks.sorting, Some(false));
            assert!(!checks.all_passed());
        }
    }

    #[test]
    fn invalid_json_or_non_object_cannot_be_evaluated() {
        for output in [
            String::new(),
            format!("```json\n{CORRECT}\n```"),
            format!("answer: {CORRECT}"),
            format!("{CORRECT} done"),
            format!("{CORRECT}{CORRECT}"),
            "[]".to_string(),
            "null".to_string(),
            "true".to_string(),
            CORRECT.replace(":93,", ":93,,"),
        ] {
            assert_eq!(
                challenge().evaluate(&output),
                Checks {
                    json: Some(false),
                    ..Checks::default()
                },
                "{output}"
            );
        }
    }

    #[test]
    fn duplicate_keys_are_rejected_even_when_last_answer_is_correct() {
        for (field, value) in [
            ("nonce", r#""wrong""#),
            ("arithmetic", "94"),
            ("sorting", "[65,21,3,0,-7]"),
        ] {
            let output = CORRECT.replacen('{', &format!("{{\"{field}\":{value},"), 1);
            assert_eq!(
                challenge().evaluate(&output),
                Checks {
                    json: Some(false),
                    ..Checks::default()
                },
                "{output}"
            );
        }
    }

    #[test]
    fn duplicate_keys_are_rejected_inside_nested_objects_and_arrays() {
        for output in [
            r#"{"extra":{"same":1,"same":2}}"#,
            r#"{"sorting":[{"same":1,"same":2}]}"#,
            r#"{"extra":[[{"same":1,"same":2}]]}"#,
            r#"{"nonce":"wrong","\u006eonce":"ab01cd23ef45ab67","arithmetic":93,"sorting":[-7,0,3,21,65]}"#,
        ] {
            assert_eq!(
                challenge().evaluate(output),
                Checks {
                    json: Some(false),
                    ..Checks::default()
                },
                "{output}"
            );
        }
    }

    #[test]
    fn extra_field_fails_instruction_without_hiding_task_results() {
        let output = CORRECT.replacen('{', r#"{"extra":true,"#, 1);
        let checks = challenge().evaluate(&output);
        assert_eq!(checks.json, Some(true));
        assert_eq!(checks.instruction, Some(false));
        assert_eq!(checks.arithmetic, Some(true));
        assert_eq!(checks.sorting, Some(true));
        assert!(!checks.all_passed());
    }

    #[test]
    fn missing_or_wrong_type_fields_are_independently_unevaluable() {
        let cases = [
            (json!({}), None, None),
            (
                json!({"nonce": "ab01cd23ef45ab67", "sorting": [-7, 0, 3, 21, 65]}),
                None,
                Some(true),
            ),
            (
                json!({"nonce": "ab01cd23ef45ab67", "arithmetic": 93}),
                Some(true),
                None,
            ),
            (
                json!({"arithmetic": 93, "sorting": [-7, 0, 3, 21, 65]}),
                Some(true),
                Some(true),
            ),
            (
                json!({"nonce": 123, "arithmetic": 93, "sorting": [-7, 0, 3, 21, 65]}),
                Some(true),
                Some(true),
            ),
            (
                json!({"nonce": "ab01cd23ef45ab67", "arithmetic": "93", "sorting": [-7, 0, 3, 21, 65]}),
                None,
                Some(true),
            ),
            (
                json!({"nonce": "ab01cd23ef45ab67", "arithmetic": 93.0, "sorting": [-7, 0, 3, 21, 65]}),
                None,
                Some(true),
            ),
            (
                json!({"nonce": "ab01cd23ef45ab67", "arithmetic": null, "sorting": "[-7,0,3,21,65]"}),
                None,
                None,
            ),
            (
                json!({"nonce": "ab01cd23ef45ab67", "arithmetic": 93, "sorting": ["-7", "0", "3", "21", "65"]}),
                Some(true),
                None,
            ),
            (
                json!({"nonce": "ab01cd23ef45ab67", "arithmetic": 93, "sorting": [-7, 0, 3.0, 21, 65]}),
                Some(true),
                None,
            ),
            (
                json!({"nonce": "ab01cd23ef45ab67", "arithmetic": 93, "sorting": [-7, 0, null, 21, 65]}),
                Some(true),
                None,
            ),
        ];
        for (answer, arithmetic, sorting) in cases {
            let checks = challenge().evaluate(&answer.to_string());
            assert_eq!(checks.json, Some(true), "{answer}");
            assert_eq!(checks.instruction, Some(false), "{answer}");
            assert_eq!(checks.arithmetic, arithmetic, "{answer}");
            assert_eq!(checks.sorting, sorting, "{answer}");
            assert!(!checks.all_passed());
        }
    }

    #[test]
    fn json_whitespace_and_field_order_are_allowed() {
        let output =
            " \n {\"sorting\":[-7,0,3,21,65],\"arithmetic\":93,\"nonce\":\"ab01cd23ef45ab67\"}\t";
        assert!(challenge().evaluate(output).all_passed());
    }

    #[test]
    fn prompt_contains_the_actual_challenge_without_supplying_answers() {
        let prompt = challenge().prompt();
        assert!(prompt.contains("ab01cd23ef45ab67"));
        assert!(prompt.contains("(17 * 6) + -9"));
        assert!(prompt.contains("[21, -7, 3, 0, 65]"));
        assert!(!prompt.contains("93"));
        assert!(!prompt.contains("[-7, 0, 3, 21, 65]"));
    }

    #[test]
    fn generated_challenges_have_distinct_short_nonces_and_bounded_tasks() {
        let first = Challenge::new();
        let second = Challenge::new();
        assert_eq!(first.nonce.len(), 16);
        assert!(first.nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first.nonce, second.nonce);
        assert!((10..90).contains(&first.a));
        assert!((2..10).contains(&first.b));
        assert!((-20..=20).contains(&first.c));
        assert!(first.numbers.iter().all(|value| (-20..100).contains(value)));
        assert!(first.numbers.windows(2).any(|pair| pair[0] > pair[1]));
    }

    #[test]
    fn unevaluated_checks_serialize_explicit_nulls_and_cannot_pass() {
        assert_eq!(
            serde_json::to_value(Checks::default()).unwrap(),
            json!({"json": null, "instruction": null, "arithmetic": null, "sorting": null})
        );
        assert!(!Checks::default().all_passed());
        for index in 0..4 {
            let mut values = [Some(true); 4];
            for missing in [None, Some(false)] {
                values[index] = missing;
                assert!(!Checks {
                    json: values[0],
                    instruction: values[1],
                    arithmetic: values[2],
                    sorting: values[3],
                }
                .all_passed());
            }
        }
    }
}
