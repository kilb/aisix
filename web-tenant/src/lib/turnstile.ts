type Action = "login" | "signup" | "email_code" | "passkey_login" | "oauth_login";

interface Turnstile {
  render(element: HTMLElement, options: {
    sitekey: string;
    action: Action;
    theme: "light" | "dark";
    size: "normal" | "compact";
    appearance: "interaction-only";
    retry: "never";
    "response-field": false;
    callback: (token: string) => void;
    "error-callback": () => void;
    "expired-callback": () => void;
    "timeout-callback": () => void;
  }): string;
  remove(id: string): void;
}

declare global {
  interface Window {
    turnstile?: Turnstile;
    aisixTurnstileLoaded?: () => void;
  }
}

const WIDGET_URL = "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit&onload=aisixTurnstileLoaded";
let loading: Promise<Turnstile> | undefined;

export function loadTurnstile(): Promise<Turnstile> {
  loading ??= new Promise<Turnstile>((resolve, reject) => {
    const script = document.createElement("script");
    script.src = WIDGET_URL;
    script.async = true;
    const clear = () => {
      window.clearTimeout(timeout);
      delete window.aisixTurnstileLoaded;
    };
    const failed = () => {
      clear();
      script.remove();
      reject(new Error("无法加载人机验证，请检查网络后重试。"));
    };
    const timeout = window.setTimeout(failed, 15_000);
    // 异步脚本使用 Cloudflare 的显式加载回调；ready() 只用于同步脚本。
    window.aisixTurnstileLoaded = () => {
      const widget = window.turnstile;
      if (!widget) { failed(); return; }
      clear();
      resolve(widget);
    };
    script.onerror = failed;
    document.head.append(script);
  }).catch((error: unknown) => { loading = undefined; throw error; });
  return loading;
}

export async function turnstileToken(
  element: HTMLDivElement | null,
  siteKey: string | null,
  action: Action,
  theme: "light" | "dark",
): Promise<string | undefined> {
  if (!siteKey) return undefined;
  if (!element) throw new Error("无法显示人机验证，请刷新后重试。");
  const widget = await loadTurnstile();
  let id: string | undefined;
  let timeout = 0;
  try {
    return await new Promise<string>((resolve, reject) => {
      const failed = () => reject(new Error("人机验证未完成，请重试。"));
      timeout = window.setTimeout(failed, 120_000);
      id = widget.render(element, {
        sitekey: siteKey,
        action,
        theme,
        size: (element.parentElement?.clientWidth ?? 0) >= 300 ? "normal" : "compact",
        appearance: "interaction-only",
        retry: "never",
        "response-field": false,
        callback: resolve,
        "error-callback": failed,
        "expired-callback": failed,
        "timeout-callback": failed,
      });
    });
  } finally {
    window.clearTimeout(timeout);
    if (id) widget.remove(id);
  }
}
