import { StrictMode, lazy, Suspense } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import "./index.css";
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
