/**
 * WCAG 2.x contrast, and the colour tokens of `theme/tokens.css` as each
 * theme resolves them, so the token pairs can be checked in a unit test.
 */

/** `#RGB` or `#RRGGBB` as 0–255 channels. */
function channels(hex: string): [number, number, number] {
  const m = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(hex.trim());
  if (!m) throw new Error(`not a hex colour: ${hex}`);
  const h = m[1]!.length === 3 ? [...m[1]!].map((c) => c + c).join("") : m[1]!;
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16)) as [number, number, number];
}

/** The relative luminance of a hex colour (WCAG 2.x, sRGB). */
export function relativeLuminance(hex: string): number {
  const [r, g, b] = channels(hex).map((c) => {
    const s = c / 255;
    return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  }) as [number, number, number];
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** The contrast ratio of two hex colours, from 1 to 21, in either order. */
export function contrastRatio(a: string, b: string): number {
  const [hi, lo] = [relativeLuminance(a), relativeLuminance(b)].sort((x, y) => y - x) as [number, number];
  return (hi + 0.05) / (lo + 0.05);
}

/** The declarations of every rule in `css` whose selector is exactly `selector`. */
function declarations(css: string, selector: string): Record<string, string> {
  const out: Record<string, string> = {};
  const body = css.replace(/\/\*[\s\S]*?\*\//g, "");
  const rule = /([^{}]+)\{([^}]*)\}/g;
  for (let m = rule.exec(body); m; m = rule.exec(body)) {
    if (m[1]!.trim() !== selector) continue;
    for (const decl of m[2]!.split(";")) {
      const colon = decl.indexOf(":");
      if (colon < 0) continue;
      const name = decl.slice(0, colon).trim();
      if (name.startsWith("--")) out[name] = decl.slice(colon + 1).trim();
    }
  }
  return out;
}

/**
 * Every custom property of `tokens.css` under `theme`, with each `var()`
 * replaced by what it resolves to: `:root` for light, and `:root` with
 * `:root[data-theme="dark"]` over it for dark.
 */
export function readTokens(css: string, theme: "light" | "dark"): Record<string, string> {
  const raw = {
    ...declarations(css, ":root"),
    ...(theme === "dark" ? declarations(css, ':root[data-theme="dark"]') : {}),
  };
  const resolved: Record<string, string> = {};
  const resolve = (name: string, seen: string[]): string => {
    if (resolved[name] !== undefined) return resolved[name]!;
    if (seen.includes(name)) throw new Error(`a var() cycle: ${[...seen, name].join(" → ")}`);
    const value = raw[name];
    if (value === undefined) throw new Error(`${name} is not defined`);
    const out = value.replace(/var\((--[\w-]+)\)/g, (_, ref: string) => resolve(ref, [...seen, name]));
    resolved[name] = out;
    return out;
  };
  for (const name of Object.keys(raw)) resolve(name, []);
  return resolved;
}
