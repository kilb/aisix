/**
 * Where a row of the model list comes from, and whether a caller can reach it
 * by name.
 *
 * A public model generates two kinds of document, both owned by
 * `public:<public model id>`: the routing document, whose id IS the public
 * model's, and one direct document per serving account, at its own id. An
 * operator's own models carry no owner.
 *
 * Only the routing document is a model a caller asks for. The direct
 * documents are the routing document's targets — internal plumbing, one per
 * serving account — and they exist so a grant, a baseline or a group rate on
 * them is refused rather than quietly doing nothing. With 27 public models
 * served by 66 accounts the flat list is 93 rows, 66 of them plumbing, so the
 * page separates them.
 */
export type ModelSource =
  /** An operator wrote it: it has no owner, and `DELETE` on it works. */
  | { kind: "manual" }
  /** A public model's routing document: the public model's own id. */
  | { kind: "public"; id: string; generated: false }
  /** A public model's direct document for one serving account. */
  | { kind: "public"; id: string; generated: true };

/**
 * What the list groups by: a model a caller asks for, the plumbing behind
 * one, or an operator's own.
 */
export type ModelClass = "facing" | "internal" | "manual";

/** The owner prefix a public model's generated documents carry. */
const OWNER = "public:";

/**
 * `m`'s source. `generated` follows [`generatedDirect`](./generatedDirect.ts):
 * a direct document's id differs from the public model's, the routing
 * document's does not.
 */
export function modelSource(m: { id: string; managed_by: string | null }): ModelSource {
  if (m.managed_by === null || !m.managed_by.startsWith(OWNER)) return { kind: "manual" };
  return { kind: "public", id: m.managed_by.slice(OWNER.length), generated: m.managed_by !== `${OWNER}${m.id}` };
}

/** `m`'s class: the only grouping the list's 分类 filter offers. */
export function modelClass(m: { id: string; managed_by: string | null }): ModelClass {
  const source = modelSource(m);
  if (source.kind === "manual") return "manual";
  return source.generated ? "internal" : "facing";
}
