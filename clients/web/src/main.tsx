import { StrictMode } from "react";
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

function DemoBanner() {
  return (
    <div className="demo-banner" role="note">
      <strong>Live demo</strong> with mock data. It runs entirely in your browser; accept, reject
      or dispatch a ticket and watch the worker respond.{" "}
      <a href="../">Back to the project site</a>
    </div>
  );
}

void boot();
