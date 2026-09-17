import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import { App } from "./App";
import { TicketmasterProvider } from "./hooks/useTicketmaster";
import "./index.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <BrowserRouter>
      <TicketmasterProvider>
        <App />
      </TicketmasterProvider>
    </BrowserRouter>
  </StrictMode>,
);
