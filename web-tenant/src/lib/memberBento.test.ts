import assert from "node:assert/strict";
import test from "node:test";
import { rankCurrentMemberCosts } from "./memberBento.ts";
import * as memberBento from "./memberBento.ts";

const people = [
  { id: "a", email: "a@example.test", display_name: "成员甲" },
  { id: "b", email: "b@example.test", display_name: null },
];

test("成员费用按大整数精确排序，并排除已离开成员", () => {
  const rows = rankCurrentMemberCosts(people, [
    { member_id: "a", charged_micro_cny: "9007199254740992" },
    { member_id: "b", charged_micro_cny: "9007199254740993" },
    { member_id: "removed", charged_micro_cny: "999999999999999999999" },
  ]);
  assert.deepEqual(rows, [
    { id: "b", label: "b@example.test", amount: 9007199254740993n },
    { id: "a", label: "成员甲", amount: 9007199254740992n },
  ]);
  assert.equal(people[0]?.id, "a");
});

test("相邻超安全整数费用不能按姓名而非金额决定先后", () => {
  const rows = rankCurrentMemberCosts([
    { id: "lower", email: "a@example.test", display_name: null },
    { id: "higher", email: "z@example.test", display_name: null },
  ], [
    { member_id: "lower", charged_micro_cny: "9007199254740992" },
    { member_id: "higher", charged_micro_cny: "9007199254740993" },
  ]);
  assert.deepEqual(rows?.map((row) => row.id), ["higher", "lower"]);
});

test("缺失或坏费用不能冒充零；明确零和负调整保持原值", () => {
  assert.equal(rankCurrentMemberCosts(people, [{ member_id: "a", charged_micro_cny: "0" }]), null);
  assert.equal(rankCurrentMemberCosts(people, [
    { member_id: "a", charged_micro_cny: "0" },
    { member_id: "b", charged_micro_cny: "invalid" },
  ]), null);
  assert.deepEqual(rankCurrentMemberCosts(people, [
    { member_id: "a", charged_micro_cny: "-1000001" },
    { member_id: "b", charged_micro_cny: "0" },
  ]), [
    { id: "b", label: "b@example.test", amount: 0n },
    { id: "a", label: "成员甲", amount: -1000001n },
  ]);
  assert.deepEqual(rankCurrentMemberCosts([], []), []);
});

test("活跃只由窗口内已记录调用决定，不由账号正常或停用决定", () => {
  const members = [
    { id: "enabled", email: "enabled@example.test", display_name: null, disabled: false },
    { id: "disabled", email: "disabled@example.test", display_name: "曾调用的成员", disabled: true },
    { id: "another", email: "another@example.test", display_name: null, disabled: false },
  ];
  assert.deepEqual(memberBento.rankCurrentMemberActivity?.(members, [
    { member_id: "enabled", requests: 0 },
    { member_id: "disabled", requests: 8 },
    { member_id: "another", requests: 3 },
    { member_id: "removed", requests: 9000 },
  ]), [
    { id: "disabled", label: "曾调用的成员", requests: 8n },
    { id: "another", label: "another@example.test", requests: 3n },
  ]);
});

test("缺失、负数或不安全调用数不能生成完整活跃统计", () => {
  for (const requests of [-1, 1.5, Number.MAX_SAFE_INTEGER + 1]) {
    assert.equal(memberBento.rankCurrentMemberActivity?.(people, [
      { member_id: "a", requests: 2 },
      { member_id: "b", requests },
    ]), null);
  }
  assert.equal(memberBento.rankCurrentMemberActivity?.(people, [{ member_id: "a", requests: 2 }]), null);
});

test("零调用与没有成员均生成明确空排行", () => {
  assert.deepEqual(memberBento.rankCurrentMemberActivity?.(people, [
    { member_id: "a", requests: 0 },
    { member_id: "b", requests: 0 },
  ]), []);
  assert.deepEqual(memberBento.rankCurrentMemberActivity?.([], []), []);
});
