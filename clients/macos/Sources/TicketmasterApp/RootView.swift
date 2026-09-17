import SwiftUI
import TicketmasterKit

struct RootView: View {
    @State private var controller = ConnectionController()
    @Namespace private var glassNamespace
    @State private var columnVisibility: NavigationSplitViewVisibility = .all

    var body: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            SidebarView(controller: controller)
                .navigationSplitViewColumnWidth(min: 220, ideal: 260)
        } content: {
            TicketListView(controller: controller)
                .navigationSplitViewColumnWidth(min: 280, ideal: 360)
        } detail: {
            TicketInspectorView(controller: controller)
                .backgroundExtensionEffect()
        }
        .toolbar {
            ToolbarItem(placement: .principal) {
                ConnectionBar(controller: controller, namespace: glassNamespace)
            }
        }
        .task {
            // Best-effort auto-connect against the default local loopback address; harmless (and
            // visibly reported as .failed) if nothing is listening yet.
        }
    }
}

/// The floating connect/status control. Uses one [`GlassEffectContainer`] so the text field,
/// connect button, and status pill blend and morph as a single glass group rather than stacking
/// independent glass layers — and a shared `glassEffectID` so the connect button and the
/// resulting status pill morph into each other across connection-state changes.
private struct ConnectionBar: View {
    @Bindable var controller: ConnectionController
    let namespace: Namespace.ID

    var body: some View {
        GlassEffectContainer(spacing: 12) {
            HStack(spacing: 12) {
                TextField("Server URL", text: $controller.serverURLText)
                    .textFieldStyle(.plain)
                    .frame(width: 220)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .glassEffect(.regular, in: .capsule)

                SecureField("Token (optional)", text: $controller.tokenText)
                    .textFieldStyle(.plain)
                    .frame(width: 140)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .glassEffect(.regular, in: .capsule)

                statusOrConnect
            }
        }
        .padding(.vertical, 4)
    }

    @ViewBuilder
    private var statusOrConnect: some View {
        switch controller.model?.connection {
        case .none, .disconnected:
            Button("Connect") {
                Task { await controller.connect() }
            }
            .buttonStyle(.glassProminent)
            .glassEffectID("connection-control", in: namespace)

        case .connecting:
            StatusPill(text: "Connecting…", tint: .yellow)
                .glassEffectID("connection-control", in: namespace)

        case .connected:
            StatusPill(text: "Connected", tint: .green)
                .glassEffectID("connection-control", in: namespace)
                .onTapGesture { controller.disconnect() }

        case .failed(let message):
            StatusPill(text: "Failed: \(message)", tint: .red)
                .glassEffectID("connection-control", in: namespace)
                .onTapGesture { Task { await controller.connect() } }
        }
    }
}

private struct StatusPill: View {
    let text: String
    let tint: Color

    var body: some View {
        Label(text, systemImage: "circle.fill")
            .labelStyle(.titleAndIcon)
            .font(.caption)
            .foregroundStyle(tint)
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .glassEffect(.regular.tint(tint.opacity(0.18)).interactive(), in: .capsule)
            .lineLimit(1)
            .truncationMode(.tail)
    }
}

struct SidebarView: View {
    @Bindable var controller: ConnectionController

    var body: some View {
        List {
            Section("Milestones") {
                if let model = controller.model, !model.milestones.isEmpty {
                    ForEach(model.milestones) { milestone in
                        Label(milestone.title, systemImage: milestone.state == .open ? "flag" : "flag.checkered")
                    }
                } else {
                    Text("No milestones yet").foregroundStyle(.secondary)
                }
            }
            Section("Participants") {
                if let model = controller.model, !model.participants.isEmpty {
                    ForEach(model.participants) { entry in
                        Label(entry.participant, systemImage: entry.action == "active" ? "person.fill" : "person")
                            .badge(entry.ticket ?? "")
                    }
                } else {
                    Text("No one present").foregroundStyle(.secondary)
                }
            }
        }
        .navigationTitle("Ticketmaster")
    }
}

struct TicketListView: View {
    @Bindable var controller: ConnectionController

    var body: some View {
        Group {
            if let model = controller.model {
                List(model.tickets, selection: Binding(
                    get: { model.selectedTicketID },
                    set: { model.selectedTicketID = $0 }
                )) { ticket in
                    TicketRow(ticket: ticket).tag(ticket.id)
                }
            } else {
                ContentUnavailableView(
                    "Not Connected",
                    systemImage: "antenna.radiowaves.left.and.right.slash",
                    description: Text("Enter a server URL above and press Connect.")
                )
            }
        }
        .navigationTitle("Tickets")
    }
}

private struct TicketRow: View {
    let ticket: Ticket

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(ticket.objective)
                .font(.body)
                .lineLimit(1)
            HStack(spacing: 6) {
                Text(ticket.id).font(.caption).foregroundStyle(.secondary)
                Text(ticket.state.rawValue.capitalized)
                    .font(.caption)
                    .foregroundStyle(ticket.state.isTerminal ? .secondary : .primary)
            }
        }
    }
}

struct TicketInspectorView: View {
    @Bindable var controller: ConnectionController

    var body: some View {
        Group {
            if let model = controller.model, let ticket = model.selectedTicket {
                ScrollView {
                    VStack(alignment: .leading, spacing: 16) {
                        Text(ticket.objective).font(.title2)
                        LabeledContent("ID", value: ticket.id)
                        LabeledContent("Kind", value: ticket.kind.rawValue)
                        LabeledContent("State", value: ticket.state.rawValue)
                        LabeledContent("Priority", value: String(ticket.priority))
                        LabeledContent("Attempts", value: String(ticket.attempts))
                        if let milestone = ticket.milestone {
                            LabeledContent("Milestone", value: milestone)
                        }

                        if ticket.state == .draft {
                            Button("Activate") {
                                Task { await model.activate(ticketID: ticket.id, actor: controller.actorText) }
                            }
                            .buttonStyle(.glass)
                        }
                    }
                    .padding(24)
                }
            } else {
                ContentUnavailableView(
                    "No Ticket Selected",
                    systemImage: "checklist",
                    description: Text("Select a ticket from the list.")
                )
            }
        }
        .navigationTitle("Inspector")
    }
}
