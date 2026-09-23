import { Navigate, Route, Routes } from "react-router-dom";
import { Nav } from "./components/Nav";
import { ConnectionBanner, Notices } from "./components/Notices";
import { StatusView } from "./views/StatusView";
import { BacklogView } from "./views/BacklogView";
import { TicketsView } from "./views/TicketsView";
import { TicketView } from "./views/TicketView";
import { ReviewView } from "./views/ReviewView";
import { NotBuilt } from "./views/NotBuilt";

export function App() {
  return (
    <div className="app-shell">
      <Nav />
      <ConnectionBanner />
      <main className="app-main">
        <Routes>
          <Route path="/" element={<TicketsView />} />
          <Route path="/tickets" element={<Navigate to="/" replace />} />
          <Route path="/ticket/:id" element={<TicketView />} />
          <Route path="/review" element={<ReviewView />} />
          <Route path="/status" element={<StatusView />} />
          <Route path="/backlog" element={<BacklogView />} />
          <Route
            path="/graph"
            element={
              <NotBuilt
                view="Graph"
                contents="dependency and parent/child graph, milestones as cuts, loop edges marked"
              />
            }
          />
          <Route
            path="/rooms"
            element={
              <NotBuilt
                view="Rooms"
                contents="per-milestone activity: participants, comments, live presence"
              />
            }
          />
          <Route
            path="/decisions"
            element={<NotBuilt view="Decisions" contents="decision log with supersession chains" />}
          />
          <Route
            path="/providers"
            element={
              <NotBuilt
                view="Providers"
                contents="role→model routing, quota headroom, breaker state, spend"
              />
            }
          />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </main>
      <Notices />
    </div>
  );
}
