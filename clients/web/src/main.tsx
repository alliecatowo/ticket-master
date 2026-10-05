import { StrictMode, useState } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, HashRouter } from "react-router-dom";
import { App } from "./App";
import { TicketmasterClient } from "./api/client";
import { loadHandle } from "./api/identity";
import { TicketmasterProvider } from "./hooks/useTicketmaster";
import "./index.css";

// Demo build (`VITE_TM_DEMO=1`, the GitHub Pages demo): no `tm serve` exists, so the client talks
// to an in-browser mock (`demo/mockServer.ts`), and routes use the URL hash because a static
// host cannot rewrite deep links. Every other build is the real app, unchanged.
const DEMO = import.meta.env.VITE_TM_DEMO === "1";

async function boot() {
  let client: TicketmasterClient | undefined;
  if (DEMO) {
    const { createDemoFetch } = await import("./demo/mockServer");
    client = new TicketmasterClient({ fetchImpl: createDemoFetch(), handle: loadHandle() });
  }
  const Router = DEMO ? HashRouter : BrowserRouter;
  const routerProps = DEMO ? {} : { basename: "/app" };
  createRoot(document.getElementById("root")!).render(
    <StrictMode>
      <Router {...routerProps}>
        <TicketmasterProvider client={client}>
          {DEMO && <DemoBanner />}
          <App />
        </TicketmasterProvider>
      </Router>
    </StrictMode>,
  );
}

function ThemeToggle() {
  const effective = () =>
    document.documentElement.getAttribute("data-theme") ??
    (window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark");
  const [theme, setTheme] = useState(effective);
  return (
    <button
      type="button"
      className="theme-toggle"
      aria-label="Switch between light (paper) and dark (ink) theme"
      onClick={() => {
        const next = theme === "light" ? "dark" : "light";
        document.documentElement.setAttribute("data-theme", next);
        try {
          localStorage.setItem("tm-theme", next);
        } catch {
          /* storage can be blocked; the toggle still works for this page view */
        }
        setTheme(next);
      }}
    >
      {theme === "light" ? "Paper" : "Ink"}
    </button>
  );
}

function DemoBanner() {
  return (
    <div className="demo-banner" role="note">
      <strong>Live demo</strong> with mock data. It runs entirely in your browser; accept, reject
      or dispatch a ticket and watch the worker respond.{" "}
      <a href="../">Back to the project site</a>
      <ThemeToggle />
    </div>
  );
}

void boot();
