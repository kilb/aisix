import assert from "node:assert/strict";
import { test } from "node:test";
import { cardShapeProblem } from "./rateCardShape.ts";

// `npm run test:unit`.

test("a card the field view reads is accepted", () => {
  assert.equal(cardShapeProblem({}), null);
  assert.equal(cardShapeProblem({ default: { base: { input: "3", output: "6" } } }), null);
  assert.equal(
    cardShapeProblem({
      default: { base: { input: "3" }, long_context: [{ above_input_tokens: 200_000, rates: { input: "6" } }] },
      batch: { base: {} },
    }),
    null,
  );
  // The JSON view allows what the fields have no control for: it must still open.
  assert.equal(cardShapeProblem({ default: { base: {}, priority: { base: { input: "1" } } } }), null);
});

test("a band that is not an object is named where it sits", () => {
  // The review's document: top-level legal, inner band null. Rendering the
  // field view reads `band.rates` and throws, taking the page with it.
  assert.equal(
    cardShapeProblem({ default: { base: { input: "1" }, long_context: [null] } }),
    "/default/long_context/0：每个长上下文档应是一个对象，含 above_input_tokens 与 rates",
  );
  assert.equal(
    cardShapeProblem({ default: { long_context: [{ rates: "x" }] } }),
    "/default/long_context/0/rates：长上下文档的价格应是一个对象",
  );
});

test("a tier or base that is not an object is named too", () => {
  assert.match(cardShapeProblem({ default: null }) ?? "", /^\/default：档位应是一个对象/);
  assert.match(cardShapeProblem({ default: { base: "3" } }) ?? "", /^\/default\/base：/);
  assert.match(cardShapeProblem({ default: { long_context: {} } }) ?? "", /^\/default\/long_context：/);
  assert.match(cardShapeProblem([]) ?? "", /价格卡应是一个对象/);
});

test("a tier name with a slash or a tilde is escaped the way the control plane writes a path", () => {
  assert.match(cardShapeProblem({ "a/b": null }) ?? "", /^\/a~1b：/);
  assert.match(cardShapeProblem({ "c~d": null }) ?? "", /^\/c~0d：/);
});
