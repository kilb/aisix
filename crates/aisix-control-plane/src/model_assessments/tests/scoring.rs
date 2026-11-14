use super::*;
use serde_json::json;
use uuid::Uuid;

fn context() -> RunEvaluationContext {
    RunEvaluationContext {
        mode: Mode::Full,
        plan: super::super::suite::build(Mode::Full, "statistics-fixture"),
        state: RunState::Completed,
        completion_reason: Some(CompletionReason::AllScheduled),
        configuration_comparable: true,
        independence_confirmed: true,
        ever_stopped: false,
    }
}

fn samples(
    context: &RunEvaluationContext,
    reference_wins: usize,
    candidate_wins: usize,
) -> Vec<AssessmentSample> {
    let mut counts = [0; 5];
    context
        .plan
        .tasks
        .iter()
        .enumerate()
        .flat_map(|(i, task)| {
            let dim = match task.dimension {
                Dimension::Reasoning => 0,
                Dimension::Code => 1,
                Dimension::Context => 2,
                Dimension::Tool => 3,
                Dimension::Protocol => 4,
            };
            let within = counts[dim];
            counts[dim] += 1;
            [Role::Reference, Role::Candidate]
                .into_iter()
                .enumerate()
                .map(move |(side, role)| {
                    let score = if within < reference_wins {
                        u8::from(role == Role::Reference)
                    } else if within < reference_wins + candidate_wins {
                        u8::from(role == Role::Candidate)
                    } else {
                        1
                    };
                    AssessmentSample {
                        id: Uuid::from_u128((i * 2 + side + 1) as u128),
                        ordinal: (i * 2 + side) as u32,
                        pair_id: task.task.pair_id.clone(),
                        cluster_id: task.task.cluster_id.clone(),
                        dimension: task.dimension,
                        role,
                        request_semantic_hash: task.request_semantic_hash.clone(),
                        task_hash: task.task.task_hash.clone(),
                        input_bytes: task.input_bytes,
                        dispatch_state: DispatchState::Finished,
                        result: if score == 1 {
                            SampleResult::Pass
                        } else {
                            SampleResult::Fail
                        },
                        error_kind: None,
                        owner_generation: None,
                        dispatch_deadline_at: None,
                        sent_at: None,
                        finished_at: None,
                        http_status: Some(200),
                        finish_reason: Some("stop".into()),
                        score: Some(score),
                        usage: Usage::default(),
                        duration_ms: Some(1.0),
                        output_bytes: Some(1),
                        evidence: Evidence {
                            checks: vec![],
                            expected_hash: String::new(),
                            answer_preview: None,
                            answer_scrubbed: false,
                        },
                        effective_request: EffectiveRequest {
                            native_protocol: "openai".into(),
                            adapter_build: "test".into(),
                            stream: false,
                            output_cap: 2048,
                            sampling_policy: "upstream_defaults".into(),
                            semantic_configuration_hash: task.request_semantic_hash.clone(),
                        },
                        error: None,
                    }
                })
        })
        .collect()
}

#[test]
fn exact_sign_test_has_hand_computed_conservative_boundaries() {
    // 错用正态近似、双侧检验、丢掉一侧败题或放松固定阈值都应失败。
    assert_eq!(exact_sign_test(0, 0), None);
    assert_eq!(exact_sign_test(12, 0), Some(1.0 / 4096.0));
    assert_eq!(exact_sign_test(11, 1), Some(13.0 / 4096.0));
    assert_eq!(exact_sign_test(15, 1), Some(17.0 / 65536.0));
    assert_eq!(exact_sign_test(14, 2), Some(137.0 / 65536.0));
    assert_eq!(exact_sign_test(0, 16), Some(1.0));
}

#[test]
fn direction_label_requires_both_frozen_effect_and_five_dimension_threshold() {
    assert_eq!(
        directional_verdict(-0.10, Some(0.002)),
        Verdict::WeakerOnSuite
    );
    assert_eq!(
        directional_verdict(-0.099, Some(0.0001)),
        Verdict::NoDirectionalEvidence
    );
    assert_eq!(
        directional_verdict(-0.5, Some(0.00201)),
        Verdict::NoDirectionalEvidence
    );
    assert_eq!(
        directional_verdict(0.5, Some(0.0001)),
        Verdict::NoDirectionalEvidence
    );
    assert_eq!(
        directional_verdict(-1.0, None),
        Verdict::NoDirectionalEvidence
    );
}

#[test]
fn all_bank_answers_are_regradable_without_executing_model_code_or_tools() {
    let ctx = context();
    assert_eq!(ctx.plan.tasks.len(), 80);
    for task in &ctx.plan.tasks {
        let answer: NormalizedAnswer = match task.task.grader_config.id.as_str() {
            "exact_json" => NormalizedAnswer {
                text: task.task.expected.to_string(),
                tool_calls: vec![],
            },
            "exact_text" => NormalizedAnswer {
                text: task.task.expected.as_str().unwrap_or_default().into(),
                tool_calls: vec![],
            },
            "tool_calls" => serde_json::from_value(task.task.expected.clone()).unwrap(),
            other => panic!("unknown grader {other}"),
        };
        let grade = grade(task, &answer);
        assert_eq!(grade.result, SampleResult::Pass, "{}", task.task.cluster_id);
        assert_eq!(grade.score, Some(1));
        assert_eq!(grade.expected_hash.len(), 64);
        assert!(!grade.checks.is_empty());
    }
}

#[test]
fn json_grader_rejects_missing_extra_wrong_typed_or_embedded_answers() {
    let mut ctx = context();
    let task = ctx
        .plan
        .tasks
        .iter_mut()
        .find(|t| t.task.grader_config.id == "exact_json")
        .unwrap();
    task.task.expected = json!({"answer": 0});
    for text in [
        "{}",
        "{\"answer\":0,\"extra\":true}",
        "{\"answer\":\"0\"}",
        "answer: {\"answer\":0}",
        "```json\n{\"answer\":0}\n```",
        "{\"answer\":0} trailing",
        "{\"answer\":1,\"answer\":0}",
    ] {
        let answer = NormalizedAnswer {
            text: text.into(),
            tool_calls: vec![],
        };
        assert_eq!(
            grade(task, &answer).result,
            SampleResult::Fail,
            "accepted: {text}"
        );
    }
    assert_eq!(
        grade(
            task,
            &NormalizedAnswer {
                text: " { \"answer\": 0 }\n".into(),
                tool_calls: vec![]
            }
        )
        .score,
        Some(1)
    );
}

#[test]
fn tool_grader_checks_name_arguments_call_count_and_unrequested_prose() {
    let ctx = context();
    let task = ctx
        .plan
        .tasks
        .iter()
        .find(|t| t.task.grader_config.id == "tool_calls")
        .unwrap();
    let correct: NormalizedAnswer = serde_json::from_value(task.task.expected.clone()).unwrap();
    assert_eq!(grade(task, &correct).score, Some(1));
    let mut wrong = correct.clone();
    wrong.tool_calls[0].name.push_str("_wrong");
    assert_eq!(grade(task, &wrong).score, Some(0));
    wrong = correct.clone();
    wrong.tool_calls[0].arguments = json!({});
    assert_eq!(grade(task, &wrong).score, Some(0));
    wrong = correct.clone();
    wrong.tool_calls.push(wrong.tool_calls[0].clone());
    assert_eq!(grade(task, &wrong).score, Some(0));
    wrong = correct;
    wrong.text = "I will run it".into();
    assert_eq!(grade(task, &wrong).score, Some(0));
}

#[test]
fn absent_answer_is_an_execution_error_while_a_complete_wrong_answer_fails() {
    let ctx = context();
    let task = &ctx.plan.tasks[0];
    let missing = grade(task, &NormalizedAnswer::default());
    assert_eq!(missing.result, SampleResult::Error);
    assert_eq!(missing.score, None);
    let wrong = grade(
        task,
        &NormalizedAnswer {
            text: "WRONG".into(),
            tool_calls: vec![],
        },
    );
    assert_eq!(wrong.result, SampleResult::Fail);
    assert_eq!(wrong.score, Some(0));
}

#[test]
fn unknown_grader_versions_are_not_scored_using_current_rules() {
    let mut ctx = context();
    let task = ctx
        .plan
        .tasks
        .iter_mut()
        .find(|t| t.task.grader_config.id == "exact_json")
        .unwrap();
    let answer = NormalizedAnswer {
        text: task.task.expected.to_string(),
        tool_calls: vec![],
    };
    task.task.grader_config.version = "unsupported-future-version".into();
    let result = grade(task, &answer);
    assert_eq!(result.result, SampleResult::Unsupported);
    assert_eq!(result.score, None);
}

#[test]
fn fourteen_of_sixteen_is_not_significant_but_fifteen_is() {
    let ctx = context();
    let weaker = summarize(&ctx, &samples(&ctx, 15, 1));
    let not_significant = summarize(&ctx, &samples(&ctx, 14, 2));
    assert_eq!(weaker.len(), 5);
    assert_eq!(not_significant.len(), 5);
    for report in weaker {
        assert_eq!(report.p_threshold, 0.002);
        assert_eq!(report.verdict, Verdict::WeakerOnSuite);
        assert_eq!(report.p_one_sided, Some(17.0 / 65536.0));
    }
    for report in not_significant {
        assert_eq!(report.verdict, Verdict::NoDirectionalEvidence);
        assert_eq!(report.p_one_sided, Some(137.0 / 65536.0));
    }
}

#[test]
fn coverage_is_independent_of_significance_and_transport_is_not_wrong() {
    let ctx = context();
    let mut rows = samples(&ctx, 16, 0);
    let mut invalid = 0;
    for row in rows
        .iter_mut()
        .filter(|r| r.dimension == Dimension::Reasoning && r.role == Role::Candidate)
    {
        if invalid < 4 {
            row.result = SampleResult::Error;
            row.score = None;
            row.error_kind = Some(ErrorKind::Timeout);
            invalid += 1;
        }
    }
    let reports = summarize(&ctx, &rows);
    let report = reports
        .iter()
        .find(|r| r.dimension == Dimension::Reasoning)
        .unwrap();
    assert_eq!(report.valid_clusters, 12);
    assert_eq!(report.paired_coverage, 0.75);
    assert_eq!(report.reference_score, Some(1.0));
    assert_eq!(report.candidate_score, Some(0.0));
    assert_eq!(report.p_one_sided, None);
    assert_eq!(report.verdict, Verdict::Insufficient);
    assert_eq!(report.p_threshold, 0.002);
}

#[test]
fn fourteen_valid_clusters_fail_ninety_percent_but_fifteen_can_infer() {
    let ctx = context();
    for invalid_count in [1, 2] {
        let mut rows = samples(&ctx, 16, 0);
        let indices: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.dimension == Dimension::Tool && r.role == Role::Candidate)
            .take(invalid_count)
            .map(|(i, _)| i)
            .collect();
        for i in indices {
            rows[i].result = SampleResult::Unsupported;
            rows[i].score = None;
        }
        let reports = summarize(&ctx, &rows);
        let report = reports
            .iter()
            .find(|r| r.dimension == Dimension::Tool)
            .unwrap();
        assert_eq!(report.p_threshold, 0.002);
        assert_eq!(report.valid_pairs, 16 - invalid_count as u32);
        assert_eq!(
            report.verdict,
            if invalid_count == 1 {
                Verdict::WeakerOnSuite
            } else {
                Verdict::Insufficient
            }
        );
    }
}

#[test]
fn inference_is_never_available_while_running_cancelled_interrupted_or_stopped() {
    let base = context();
    let rows = samples(&base, 16, 0);
    for state in [
        RunState::Planned,
        RunState::Queued,
        RunState::Running,
        RunState::Cancelled,
        RunState::Interrupted,
    ] {
        let mut ctx = base.clone();
        ctx.state = state;
        let reports = summarize(&ctx, &rows);
        assert_eq!(reports.len(), 5);
        assert!(reports
            .iter()
            .all(|r| r.p_one_sided.is_none() && r.verdict == Verdict::Insufficient));
    }
    for reason in [
        CompletionReason::Deadline,
        CompletionReason::Budget,
        CompletionReason::ConfigurationChanged,
        CompletionReason::OwnerLost,
        CompletionReason::Cancelled,
    ] {
        let mut ctx = base.clone();
        ctx.completion_reason = Some(reason);
        assert!(summarize(&ctx, &rows)
            .iter()
            .all(|r| r.p_one_sided.is_none() && r.verdict == Verdict::Insufficient));
    }
    let mut ctx = base.clone();
    ctx.ever_stopped = true;
    assert!(summarize(&ctx, &rows)
        .iter()
        .all(|r| r.p_one_sided.is_none()));
    let mut unfinished = rows.clone();
    unfinished[0].dispatch_state = DispatchState::Sent;
    assert!(summarize(&base, &unfinished)
        .iter()
        .all(|r| r.p_one_sided.is_none() && r.verdict == Verdict::Insufficient));
}

#[test]
fn duplicate_sample_or_relabelled_task_never_increases_independent_clusters() {
    let ctx = context();
    let rows = samples(&ctx, 16, 0);
    let mut duplicated = rows.clone();
    for _ in 0..100 {
        let mut row = rows[0].clone();
        row.id = Uuid::new_v4();
        row.ordinal = duplicated.len() as u32;
        duplicated.push(row);
    }
    let reports = summarize(&ctx, &duplicated);
    assert_eq!(reports.len(), 5);
    assert!(reports.iter().all(|r| r.valid_clusters <= 16
        && r.p_one_sided.is_none()
        && r.verdict == Verdict::Insufficient));
    let mut broken = ctx.clone();
    broken.plan.tasks[0].request_semantic_hash = broken.plan.tasks[1].request_semantic_hash.clone();
    assert!(summarize(&broken, &rows)
        .iter()
        .all(|r| r.p_one_sided.is_none() && r.verdict == Verdict::Insufficient));
    // 同步伪造样本元数据，确保此反证只能被审定题库校验挡住。
    let mut forged_rows = rows.clone();
    for row in &mut forged_rows {
        if row.pair_id == broken.plan.tasks[0].task.pair_id {
            row.request_semantic_hash = broken.plan.tasks[0].request_semantic_hash.clone();
        }
    }
    assert!(summarize(&broken, &forged_rows)
        .iter()
        .all(|r| r.p_one_sided.is_none() && r.verdict == Verdict::Insufficient));
}

#[test]
fn shared_wrong_answers_are_ties_and_do_not_prove_matching_identity() {
    let ctx = context();
    let mut rows = samples(&ctx, 0, 0);
    for row in &mut rows {
        row.score = Some(0);
        row.result = SampleResult::Fail;
    }
    let reports = summarize(&ctx, &rows);
    assert_eq!(reports.len(), 5);
    for report in &reports {
        assert_eq!(report.reference_score, Some(0.0));
        assert_eq!(report.candidate_score, Some(0.0));
        assert_eq!(report.ties, 16);
        assert_eq!(report.p_one_sided, None);
        assert_eq!(report.verdict, Verdict::NoDirectionalEvidence);
    }
    assert_eq!(overall_verdict(&reports), Verdict::NoDirectionalEvidence);
    assert_eq!(overall_verdict(&reports[..4]), Verdict::Insufficient);
}

#[test]
fn quick_and_unconfirmed_comparability_or_independence_are_descriptive() {
    let base = context();
    let rows = samples(&base, 16, 0);
    for (quick, comparable, independent) in [
        (true, true, true),
        (false, false, true),
        (false, true, false),
    ] {
        let mut ctx = base.clone();
        ctx.mode = if quick { Mode::Quick } else { Mode::Full };
        ctx.configuration_comparable = comparable;
        ctx.independence_confirmed = independent;
        let reports = summarize(&ctx, &rows);
        assert_eq!(reports.len(), 5);
        assert!(reports
            .iter()
            .all(|r| r.p_one_sided.is_none() && r.verdict == Verdict::Insufficient));
    }
    let mut quick = base;
    quick.mode = Mode::Quick;
    quick.plan = super::super::suite::build(Mode::Quick, "statistics-fixture");
    let rows = samples(&quick, 2, 0);
    let reports = summarize(&quick, &rows);
    assert!(reports.iter().all(|r| r
        .reason_codes
        .iter()
        .any(|code| code == "quick_descriptive_only")));
}

#[test]
fn invalid_execution_or_missing_score_never_enters_the_paired_denominator() {
    let ctx = context();
    for variant in 0..6 {
        let mut rows = samples(&ctx, 16, 0);
        let row = rows
            .iter_mut()
            .find(|row| row.dimension == Dimension::Code && row.role == Role::Candidate)
            .unwrap();
        match variant {
            0 => {
                row.error_kind = Some(ErrorKind::Truncated);
            }
            1 => {
                row.score = None;
            }
            2 => {
                row.http_status = Some(429);
            }
            3 => {
                row.finish_reason = Some("length".into());
            }
            4 => {
                row.output_bytes = Some(65_537);
            }
            _ => {
                row.result = SampleResult::Pass;
                row.score = Some(0);
            }
        }
        let reports = summarize(&ctx, &rows);
        let report = reports
            .iter()
            .find(|r| r.dimension == Dimension::Code)
            .unwrap();
        assert_eq!(report.valid_clusters, 15, "variant={variant}");
        assert_eq!(report.paired_coverage, 15.0 / 16.0);
        assert_eq!(report.p_threshold, 0.002);
    }
}
