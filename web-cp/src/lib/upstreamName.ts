/**
 * An upstream model name's shape, as the control plane checks it: 1 to 255
 * characters after trimming, and no control characters.
 *
 * **Membership is never checked.** The picker offers the account's own list,
 * but a typed name outside it is a valid answer — an upstream that refuses
 * the list, or one whose list is incomplete, must never block configuring an
 * account. Empty is not invalid: it clears the mapping, so the public name is
 * what the account's upstream is asked for.
 */

/** The longest upstream name the control plane takes, in characters. */
export const MAX_UPSTREAM_CHARS = 255;

/** The refusal for `name`, or null when its shape is one the control plane takes. */
export function upstreamNameProblem(name: string): string | null {
  const typed = name.trim();
  if (typed === "") return null;
  if (typed.length > MAX_UPSTREAM_CHARS) return `上游模型名称不能超过 ${MAX_UPSTREAM_CHARS} 个字符`;
  // Every C0 control character, and DEL.
  if (/[\u0000-\u001f\u007f]/.test(typed)) return "上游模型名称不能包含控制字符";
  return null;
}
