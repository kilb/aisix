import { schemas, type JsonSchema } from "./api";

/**
 * Form options, defaults and descriptions read from the gateway's strict
 * schemas (`GET /admin/schemas/{resource}`), by JSON pointer into the schema
 * document, such as `/properties/adapter` or
 * `/definitions/Routing/properties/strategy`.
 *
 * Forms never hard-code an enum: the schema is generated from the code that
 * enforces the field. A pointer that resolves to nothing throws
 * `SchemaShapeError`, which a form shows as an error, never as an empty
 * dropdown: a schema reshape breaks the form loudly.
 */

export class SchemaShapeError extends Error {
  constructor(readonly pointer: string, reason: string) {
    super(`表单所依赖的配置结构已变化（${pointer}：${reason}）`);
  }
}

const cache = new Map<string, Promise<JsonSchema>>();

/** The strict schema of `resource`, fetched once per page load. */
export function loadSchema(resource: string): Promise<JsonSchema> {
  let hit = cache.get(resource);
  if (!hit) {
    hit = schemas.get(resource);
    // A failed fetch is not cached, so the next form that asks retries.
    hit.catch(() => cache.delete(resource));
    cache.set(resource, hit);
  }
  return hit;
}

type Node = Record<string, unknown>;

/** Bounds `$ref` chains, so a cyclic schema cannot loop forever. */
const MAX_DEPTH = 32;

function isNode(v: unknown): v is Node {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** RFC 6901 segments of `pointer` (`""` and `"/"` are the root). */
function segments(pointer: string): string[] {
  const body = pointer.replace(/^#/, "");
  if (body === "" || body === "/") return [];
  return body
    .replace(/^\//, "")
    .split("/")
    .map((s) => s.replace(/~1/g, "/").replace(/~0/g, "~"));
}

/** The node a local `$ref` (`#/...`) names. */
function deref(root: JsonSchema, ref: string, pointer: string): Node {
  if (!ref.startsWith("#")) throw new SchemaShapeError(pointer, `不支持的引用 ${ref}`);
  let node: unknown = root;
  for (const s of segments(ref)) node = isNode(node) ? node[s] : undefined;
  if (!isNode(node)) throw new SchemaShapeError(pointer, `引用 ${ref} 不存在`);
  return node;
}

/**
 * `node` and every schema it stands for through `$ref`, `allOf`, `anyOf` and
 * `oneOf`, depth first, `node` itself first.
 */
function expand(root: JsonSchema, node: Node, pointer: string, depth = 0): Node[] {
  if (depth > MAX_DEPTH) throw new SchemaShapeError(pointer, "引用层级过深");
  const out: Node[] = [node];
  if (typeof node.$ref === "string") {
    out.push(...expand(root, deref(root, node.$ref, pointer), pointer, depth + 1));
  }
  for (const key of ["allOf", "anyOf", "oneOf"]) {
    const list = node[key];
    if (!Array.isArray(list)) continue;
    for (const sub of list) if (isNode(sub)) out.push(...expand(root, sub, pointer, depth + 1));
  }
  return out;
}

/**
 * The node at `pointer`. Each step takes the key from the current node or,
 * when it lacks it, from the first schema the node stands for that has it,
 * so `/properties/routing/properties/strategy` reads through a `$ref` or an
 * `anyOf: [{$ref}, {type: null}]` on `routing`.
 */
function nodeAt(schema: JsonSchema, pointer: string): Node {
  let node: Node = schema;
  for (const s of segments(pointer)) {
    const next = expand(schema, node, pointer)
      .map((n) => n[s])
      .find((v) => v !== undefined);
    if (!isNode(next)) throw new SchemaShapeError(pointer, `找不到 ${s}`);
    node = next;
  }
  return node;
}

/**
 * The string values allowed at `pointer`: the node's `enum` or `const`, or
 * those of the schemas it stands for (a `$ref`, an `allOf`, the non-null
 * branches of an `anyOf`, a `oneOf` of single-value enums), in schema order
 * without repeats.
 */
export function enumAt(schema: JsonSchema, pointer: string): string[] {
  const values: string[] = [];
  for (const n of expand(schema, nodeAt(schema, pointer), pointer)) {
    const own = Array.isArray(n.enum) ? n.enum : "const" in n ? [n.const] : [];
    for (const v of own) if (typeof v === "string" && !values.includes(v)) values.push(v);
  }
  if (values.length === 0) throw new SchemaShapeError(pointer, "没有可选值");
  return values;
}

/** The description at `pointer`, or of the first schema it stands for that has one. */
export function describeAt(schema: JsonSchema, pointer: string): string | null {
  for (const n of expand(schema, nodeAt(schema, pointer), pointer)) {
    if (typeof n.description === "string") return n.description;
  }
  return null;
}

/** The default at `pointer`, or of the first schema it stands for that has one. */
export function defaultAt(schema: JsonSchema, pointer: string): unknown {
  for (const n of expand(schema, nodeAt(schema, pointer), pointer)) {
    if ("default" in n) return n.default;
  }
  return undefined;
}
