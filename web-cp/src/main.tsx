import { StrictMode, lazy, Suspense } from "react";
import { createRoot } from "react-dom/client";
import "./index.css";
import "./assets/fonts/fonts.css";
import { App } from "./App";
// 中文只导入一个字重并允许合成粗体，减少重复字体分片。
import "@fontsource/noto-sans-sc/400.css";
import { ToastProvider } from "./ui/Toast";

const root = document.getElementById("root");
if (!root) throw new Error("#root missing from the HTML shell");

// The component style guide, in development builds only: `import.meta.env.DEV`
// is false in a production build, so the page and its import are dropped.
const Styleguide = import.meta.env.DEV ? lazy(() => import("./Styleguide")) : null;

createRoot(root).render(
  <StrictMode>
    <ToastProvider>
      {Styleguide && location.pathname === "/__styleguide" ? (
        <Suspense fallback={null}>
          <Styleguide />
        </Suspense>
      ) : (
        <App />
      )}
    </ToastProvider>
  </StrictMode>,
);
