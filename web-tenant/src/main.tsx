import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import { App } from "./App";
import { ToastProvider } from "./ui/Toast";
// 中文沿用自托管字体，拉丁字符与数字使用模板的 Manrope。
import "@fontsource/noto-sans-sc/400.css";
import "./assets/fonts/fonts.css";

/**
 * The invitation token, if the page was opened from an invitation link
 * (`<portal>/invite#token=<token>`), taken out of the address bar before
 * anything renders or any request is made.
 *
 * The link carries the token in its fragment, which a browser never sends to
 * a server; replacing the URL also keeps it out of the history, out of an
 * address someone copies, and out of anything that reads `location` later.
 * From here it lives in memory only: it is never stored, never logged, and
 * leaves the page once, in the body of the acceptance request. An empty
 * `token` is cleared from the address bar too, but opens no form.
 */
function takeInviteToken(): string | null {
  const fragment = new URLSearchParams(window.location.hash.slice(1));
  if (!fragment.has("token")) return null;
  window.history.replaceState(null, "", import.meta.env.BASE_URL);
  const token = fragment.get("token");
  if (!token) return null;
  return token;
}

const inviteToken = takeInviteToken();

function takeOAuthResult(): "success" | "linked" | "error" | "restart" | null {
  const url = new URL(window.location.href);
  const result = url.searchParams.get("auth");
  if (result !== "success" && result !== "linked" && result !== "error" && result !== "restart") return null;
  url.searchParams.delete("auth");
  if (result === "linked" || result === "error") url.hash = "account";
  window.history.replaceState(null, "", url);
  return result;
}

const oauthResult = takeOAuthResult();
const root = document.getElementById("root");
if (!root) throw new Error("#root missing from the HTML shell");
createRoot(root).render(
  <StrictMode>
    <ToastProvider>
      <App inviteToken={inviteToken} oauthResult={oauthResult} />
    </ToastProvider>
  </StrictMode>,
);
