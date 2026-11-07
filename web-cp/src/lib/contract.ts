/**
 * The control-plane API contract this Dashboard is built against.
 *
 * It mirrors `API_CONTRACT` in `crates/aisix-control-plane/src/http.rs`,
 * which `/admin/session` serves and which is the authority; the test
 * `ui_contracts_match` there pins the two equal, so every commit agrees with
 * itself and a mismatch at run time can only mean the two were deployed from
 * different commits.
 *
 * Change it when the API really changes, together with the Rust constant;
 * never with a release number or a build time.
 */
export const EXPECTED_API_CONTRACT = 1;
