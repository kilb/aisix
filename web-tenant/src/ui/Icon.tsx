// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.

/**
 * The one icon set: 16px, a 1.5px stroke with round caps, drawn in
 * currentColor so it follows the text it sits beside. Used sparingly, on
 * actions (refresh, copy, close, external, the theme switch) and as the
 * larger mark of an empty state. Navigation stops carry none. An icon is
 * always decorative (`aria-hidden`): the control around it carries the
 * words.
 */

const PATHS = {
  refresh: (
    <>
      <path d="M13.25 8a5.25 5.25 0 1 1-1.54-3.71" />
      <path d="M11.75 1.75v2.5h-2.5" />
    </>
  ),
  copy: (
    <>
      <rect x="5.75" y="5.75" width="7.5" height="7.5" rx="1.5" />
      <path d="M10.25 5.75v-1.5a1.5 1.5 0 0 0-1.5-1.5h-4.5a1.5 1.5 0 0 0-1.5 1.5v4.5a1.5 1.5 0 0 0 1.5 1.5h1.5" />
    </>
  ),
  close: <path d="M4.25 4.25l7.5 7.5M11.75 4.25l-7.5 7.5" />,
  external: (
    <>
      <path d="M9.75 2.75h3.5v3.5M13.25 2.75 7.75 8.25" />
      <path d="M12.25 9.25v2.5a1.5 1.5 0 0 1-1.5 1.5h-6.5a1.5 1.5 0 0 1-1.5-1.5v-6.5a1.5 1.5 0 0 1 1.5-1.5h2.5" />
    </>
  ),
  check: <path d="M3.25 8.5l3 3 6.5-6.75" />,
  plus: <path d="M8 3.25v9.5M3.25 8h9.5" />,
  moon: <path d="M13.25 9.6A5.5 5.5 0 0 1 6.4 2.75a5.5 5.5 0 1 0 6.85 6.85Z" />,
  sun: (
    <>
      <circle cx="8" cy="8" r="2.75" />
      <path d="M8 1.75v1.25M8 13v1.25M1.75 8H3M13 8h1.25M3.58 3.58l.88.88M11.54 11.54l.88.88M3.58 12.42l.88-.88M11.54 4.46l.88-.88" />
    </>
  ),
} as const;

/** The 32px marks of empty states, on a 32-unit grid. */
const MARKS = {
  key: (
    <>
      <circle cx="11.25" cy="11.25" r="6.75" />
      <circle cx="11.25" cy="11.25" r="2" />
      <path d="M16 16l11.5 11.5M22.5 22.5l-3 3M25.75 25.75l-2.5 2.5" />
    </>
  ),
  /** A route with no stop on it yet: the generic empty list. */
  route: (
    <>
      <path d="M4 16h24" />
      <circle cx="8" cy="16" r="3.25" />
      <circle cx="24" cy="16" r="3.25" />
      <path d="M14 12.5v7M18 12.5v7" strokeDasharray="1.5 2.5" />
    </>
  ),
  gateway: (
    <>
      <rect x="4.75" y="6.75" width="22.5" height="7.5" rx="1.5" />
      <rect x="4.75" y="17.75" width="22.5" height="7.5" rx="1.5" />
      <path d="M9 10.5h.01M9 21.5h.01M14 10.5h9M14 21.5h9" />
    </>
  ),
  log: (
    <>
      <rect x="6.75" y="4.75" width="18.5" height="22.5" rx="1.5" />
      <path d="M11 11h10M11 16h10M11 21h6" />
    </>
  ),
  mail: (
    <>
      <rect x="4.75" y="7.75" width="22.5" height="16.5" rx="2" />
      <path d="M5.5 9l10.5 8.25L26.5 9" />
    </>
  ),
} as const;

export type IconName = keyof typeof PATHS;
export type MarkName = keyof typeof MARKS;

export function Icon({ name }: { name: IconName }) {
  return (
    <svg className="icon" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      {PATHS[name]}
    </svg>
  );
}

/** The 32px mark an empty state leads with. */
export function Mark({ name }: { name: MarkName }) {
  return (
    <svg className="icon icon-32" viewBox="0 0 32 32" aria-hidden="true" focusable="false">
      {MARKS[name]}
    </svg>
  );
}

export const ICON_NAMES = Object.keys(PATHS) as IconName[];
export const MARK_NAMES = Object.keys(MARKS) as MarkName[];
