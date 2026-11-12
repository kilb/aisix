import assert from "node:assert/strict";
import test from "node:test";
import { rankCurrentMemberCosts } from "./memberBento.ts";

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
