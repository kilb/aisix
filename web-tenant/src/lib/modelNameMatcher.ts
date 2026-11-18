/** 普通搜索匹配子串；带星号时匹配完整名称，其他字符均按字面匹配。 */
export function modelNameMatcher(input: string): (name: string) => boolean {
  const query = input.trim().toLowerCase();
  if (!query.includes("*")) return name => name.toLowerCase().includes(query);
  const parts = query.split("*");
  const prefix = parts[0]!;
  const suffix = parts[parts.length - 1]!;
  const middle = parts.slice(1, -1).filter(Boolean);
  return inputName => {
    const name = inputName.toLowerCase();
    if (!name.startsWith(prefix) || !name.endsWith(suffix)) return false;
    let cursor = prefix.length;
    for (const segment of middle) {
      const start = name.indexOf(segment, cursor);
      if (start < 0) return false;
      cursor = start + segment.length;
    }
    return cursor <= name.length - suffix.length;
  };
}
