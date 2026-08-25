import { useCallback, useEffect, useState } from "react";

export type Theme = "dark" | "light";
const KEY = "aisix-console-theme";

/**
 * 主题选择。默认暗色，而不是跟随系统。
 *
 * 这是一个产品判断，不是疏忽：这个界面是仪表间，而仪表间是暗的、被自己的
 * 仪表照亮 —— 那是它的身份所在。跟随系统会让多数用户永远看不到它本来的
 * 样子。
 *
 * 但「默认」不等于「强制」：用户一旦显式选过，选择就一直生效。
 */
export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(() => {
    const saved = localStorage.getItem(KEY);
    return saved === "light" || saved === "dark" ? saved : "dark";
  });

  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
  }, [theme]);

  const toggle = useCallback(() => {
    setTheme((t) => {
      const next = t === "dark" ? "light" : "dark";
      localStorage.setItem(KEY, next);
      return next;
    });
  }, []);

  return [theme, toggle];
}
