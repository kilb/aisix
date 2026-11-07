/**
 * The portal's one icon set: inline SVG, 16px on a 1.5px round-capped
 * stroke in the current text colour, with one larger mark for empty states.
 * Used sparingly: navigation stops carry none; an action such as copy may.
 * Icons are always decorative here (`aria-hidden`); the control beside or
 * around one carries the words.
 */

const PATHS = {
  copy: (
    <>
      <rect x="5.75" y="5.75" width="7.5" height="7.5" rx="1.5" />
      <path d="M10.25 5.75v-1.5a1.5 1.5 0 0 0-1.5-1.5h-4.5a1.5 1.5 0 0 0-1.5 1.5v4.5a1.5 1.5 0 0 0 1.5 1.5h1.5" />
    </>
  ),
  check: <path d="M3.25 8.5l3 3 6.5-6.75" />,
  refresh: (
    <>
      <path d="M13.25 8a5.25 5.25 0 1 1-1.54-3.71" />
      <path d="M11.75 1.75v2.5h-2.5" />
    </>
  ),
  moon: <path d="M13.25 9.6A5.5 5.5 0 0 1 6.4 2.75a5.5 5.5 0 1 0 6.85 6.85Z" />,
  sun: (
    <>
      <circle cx="8" cy="8" r="2.75" />
      <path d="M8 1.75v1.25M8 13v1.25M1.75 8H3M13 8h1.25M3.58 3.58l.88.88M11.54 11.54l.88.88M3.58 12.42l.88-.88M11.54 4.46l.88-.88" />
    </>
  ),
} as const;

const MARKS = {
  key: (
    <>
      <circle cx="11.25" cy="11.25" r="6.75" />
      <circle cx="11.25" cy="11.25" r="2" />
      <path d="M16 16l11.5 11.5M22.5 22.5l-3 3M25.75 25.75l-2.5 2.5" />
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
    <svg className="i" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
      {PATHS[name]}
    </svg>
  );
}

/** The 32px mark an empty state leads with. */
export function Mark({ name }: { name: MarkName }) {
  return (
    <svg className="i i-32" viewBox="0 0 32 32" aria-hidden="true" focusable="false">
      {MARKS[name]}
    </svg>
  );
}
