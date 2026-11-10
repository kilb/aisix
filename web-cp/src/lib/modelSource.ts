/**
 * Where a row of the model list comes from.
 *
 * A public model generates two kinds of document, both owned by
 * `public:<public model id>`: the routing document, whose id IS the public
 * model's, and one direct document per serving account, at its own id. An
 * operator's own models carry no owner. The list says which is which, and
 * only an operator's model is deleted on its own page; a generated one is
 * removed by retiring the public model it came from.
 */
export type ModelSource =
  /** An operator wrote it: it has no owner, and `DELETE` on it works. */
  | { kind: "manual" }
  /** A public model's routing document: the public model's own id. */
  | { kind: "public"; id: string; generated: false }
  /** A public model's direct document for one serving account. */
  | { kind: "public"; id: string; generated: true };

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
