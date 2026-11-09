/**
 * A direct document a public model generated for one serving account: owned
 * by that public model (`public:<its id>`) and at an id other than its own,
 * which is the routing document's. It is granted and priced only through its
 * public model — its name carries the account's, and a grant, a baseline or a
 * group rate on it is refused — so no picker offers it.
 */
export function generatedDirect(m: { id: string; managed_by: string | null }): boolean {
  return m.managed_by !== null && m.managed_by.startsWith("public:") && m.managed_by !== `public:${m.id}`;
}
