import assert from "node:assert/strict";
import { test } from "node:test";
import {
  deleteRefusalText,
  deleteWarning,
  inUseText,
  retireWarning,
  withoutServing,
  type Refusal,
  type Retirable,
} from "./retireWarning.ts";

// `npm run test:unit`.

const model = (patch: Partial<Retirable> = {}): Retirable => ({
  name: "pub-a",
  tenants: 0,
  keys: 0,
  served_by: [],
  ...patch,
});

test("nothing holds it: the warning says what goes and that the name stays taken", () => {
  const text = retireWarning(model({ served_by: [{ enabled: false, provider_key_id: "k1" }] }));
  assert.match(text, /停用后不能恢复/);
  assert.match(text, /路由文档/);
  assert.match(text, /直连文档/);
  assert.match(text, /1 个账号上的映射设置/);
  assert.match(text, /「pub-a」以后不能再用于公共模型/);
  assert.doesNotMatch(text, /停用会被拒绝/);
});

test("a grant holds it: the warning counts the tenants and keys and names what to clear", () => {
  const text = retireWarning(model({ tenants: 2, keys: 3 }));
  assert.match(text, /仍有 2 个租户、3 个密钥的授权指向它，停用会被拒绝，什么也不会删除/);
  assert.match(text, /请先从租户与密钥中移除它/);
  assert.doesNotMatch(text, /将删除它的路由文档/);
});

test("an enabled account holds it: the warning counts accounts and sends to the account's 模型 section", () => {
  const text = retireWarning(
    model({
      served_by: [
        { enabled: true, provider_key_id: "k1" },
        { enabled: false, provider_key_id: "k2" },
      ],
    }),
  );
  assert.match(text, /仍有 1 个启用的账号指向它/);
  assert.match(text, /在上游账号的「模型」里取消勾选/);
  // The disabled row still counts for the mappings sentence, never for the holders.
  assert.doesNotMatch(text, /2 个启用的账号/);
});

test("the two kinds of holder are named together, in one sentence", () => {
  const text = retireWarning(model({ tenants: 1, keys: 0, served_by: [{ enabled: true, provider_key_id: "k1" }] }));
  assert.match(text, /1 个租户、0 个密钥的授权，以及 1 个启用的账号/);
});

test("a view without one account's row is the state the retire will meet", () => {
  const two = model({
    served_by: [
      { enabled: true, provider_key_id: "k1" },
      { enabled: true, provider_key_id: "k2" },
    ],
  });
  assert.deepEqual(withoutServing(two, "k1").served_by, [{ enabled: true, provider_key_id: "k2" }]);
  // Removing the only serving account leaves nothing holding it: the retire goes through.
  assert.match(retireWarning(withoutServing(two, "k2")), /仍有 1 个启用的账号/);
  const alone = model({ served_by: [{ enabled: true, provider_key_id: "k1" }] });
  assert.match(retireWarning(withoutServing(alone, "k1")), /停用后不能恢复/);
});

/** A refusal's answer, by the fields `inUseText` reads. */
function refusal(counts: Record<string, number> | null): Refusal {
  return { message: "refused", code: "public_model_in_use", counts };
}

test("a refusal prints its own counts, zeros included", () => {
  assert.equal(inUseText(refusal({ tenants: 1, keys: 2, channels: 3 })), "该公共模型仍在使用中：租户 1、密钥 2、启用的账号 3");
  assert.equal(inUseText(refusal(null)), "该公共模型仍在使用中：租户 0、密钥 0、启用的账号 0");
});

test("a delete says the mappings go first, and offers no account to untick", () => {
  const text = deleteWarning(
    model({
      served_by: [
        { enabled: true, provider_key_id: "k1" },
        { enabled: false, provider_key_id: "k2" },
      ],
    }),
  );
  // An enabled account does not hold a delete: it is what the delete removes.
  assert.match(text, /先删除 2 个账号对它的映射，再删除它的路由文档和各账号的直连文档/);
  assert.match(text, /删除后不能恢复/);
  assert.match(text, /名称「pub-a」以后不能再用于公共模型/);
  assert.doesNotMatch(text, /删除会被拒绝/);
  assert.doesNotMatch(text, /取消勾选/);
});

test("a delete with nothing mapped still names what goes", () => {
  const text = deleteWarning(model());
  assert.match(text, /删除它的路由文档和为各账号生成的直连文档/);
  assert.doesNotMatch(text, /个账号对它的映射/);
});

test("a grant refuses a delete, and says to clear it first", () => {
  const text = deleteWarning(model({ tenants: 2, keys: 3 }));
  assert.match(text, /仍有 2 个租户、3 个密钥的授权指向它，删除会被拒绝，什么也不会删除/);
  assert.match(text, /请先从租户与密钥中移除它/);
  // A serving account would never appear here: it refuses a retire, not a delete.
  assert.doesNotMatch(text, /个启用的账号/);
});

test("a refused delete names the two places to clear, not the accounts", () => {
  assert.equal(
    deleteRefusalText(refusal({ tenants: 1, keys: 2, channels: 5 })),
    "该公共模型仍在使用中：1 个租户、2 个密钥仍指向它。请在租户的模型授权、以及密钥的模型选择里移除它，再删除。",
  );
});
