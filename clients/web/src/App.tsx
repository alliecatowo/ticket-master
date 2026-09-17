import { Navigate, Route, Routes } from "react-router-dom";
import { Nav } from "./components/Nav";
import { StatusView } from "./views/StatusView";
import { BacklogView } from "./views/BacklogView";
import { TicketView } from "./views/TicketView";
import { NotBuilt } from "./views/NotBuilt";

export function App() {
  return (
    <div className="app-shell">
      <Nav />
      <main className="app-main">
        <Routes>
          <Route path="/" element={<Navigate to="/status" replace />} />
          <Route path="/status" element={<StatusView />} />
          <Route path="/backlog" element={<BacklogView />} />
          <Route path="/ticket/:id" element={<TicketView />} />
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
            path="/review"
            element={
              <NotBuilt
                view="Review"
                contents="diffs and evidence awaiting audit, with approve/reject"
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
          <Route path="*" element={<Navigate to="/status" replace />} />
        </Routes>
      </main>
    </div>
  );
}
