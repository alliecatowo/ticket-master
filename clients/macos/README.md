# `clients/macos` — Ticketmaster.app

A SwiftUI macOS 26 client, per `SPEC.md` §18.3: a testable Swift package
(`TicketmasterKit`) with models, an HTTP/SSE client, and a view model, plus a thin
SwiftUI app target (`TicketmasterApp`) that uses genuine Liquid Glass.

Built and verified on this machine: macOS 26.4, Xcode 26.4, Swift 6.3.

## What is built

**`TicketmasterKit`** (`Sources/TicketmasterKit/`), a Swift 6 library, no third-party
dependencies — only Foundation and Observation:

- `Models.swift` — `Ticket`, `Milestone`, `PresenceEntry`, `TMEvent`, `TicketKind`,
  `TicketState`, `MilestoneState`, decoded straight off `tm-server`'s real wire shapes
  (`crates/tm-server/src/routes.rs`'s `*_json` helpers and `Ticket`'s own `Serialize`
  derive). Rich substructures this slice doesn't yet interpret (`authority`,
  `executor`, `verification`, `budget`, `retry`, `cycle`, `resources`,
  `context_refs`, `success`, `failures`) decode into an opaque `JSONValue` rather than
  a hand-modeled Swift type — see `JSONValue.swift`'s doc comment for why: it keeps
  decoding honest (never crashes or silently drops data on a field this client
  doesn't model) without pretending those fields are understood.
- `APIClient.swift` — a `URLSession`-based REST client (`TicketmasterAPI` protocol +
  `APIClient` implementation) covering `GET /health`, `GET /tickets`,
  `GET /milestones`, `GET /presence`, and the one wired mutation,
  `POST /tickets/:id/transition` with an `{"activate": {"actor": ...}}` body (moves a
  `Draft` ticket to `Blocked`/`Ready`). Sends `Authorization: Bearer <token>` when a
  token is configured, matching `crates/tm-server/src/auth.rs`'s loopback-vs-bearer
  policy.
- `EventStream.swift` — `GET /events?from=<seq>` as an `AsyncThrowingStream<TMEvent,
  Error>`, plus the pure, network-free pieces that do the real work and are what's
  actually unit tested: `SSELineParser` (incremental SSE frame parsing, including
  frames split mid-line across chunks) and `decodeEvent` (frame → `TMEvent`,
  matching `crates/tm-server/src/sse.rs`'s `WireEvent`).
- `TicketmasterViewModel.swift` — the vertical slice's core: `connect()` loads
  tickets/milestones/presence over REST, then (if given a stream) tails `/events`
  and refreshes those three lists on every event. `activate(ticketID:actor:)` is the
  one mutation. `@MainActor @Observable`, driven entirely through the
  `TicketmasterAPI`/`TicketmasterEventStream` protocols so it's testable against
  in-memory fakes with no real server or network.

**`TicketmasterApp`** (`Sources/TicketmasterApp/`), the SwiftUI app:

- `RootView.swift` — `NavigationSplitView` with sidebar (milestones, participants),
  content (ticket list), detail (ticket inspector, with the Activate button for
  `Draft` tickets).
- `ConnectionController.swift` — holds the connect form (server URL, optional bearer
  token, actor identity) and the live `TicketmasterViewModel` once connected.
- Liquid Glass, the real APIs, not a blur imitation:
  - `GlassEffectContainer` groups the server-URL field, token field, and
    connect/status control into one blended glass group in the toolbar
    (`ConnectionBar`), rather than stacking independent glass layers.
  - `.glassEffect(.regular, in: .capsule)` on the text fields; `.glassEffect(.regular
    .tint(_).interactive())` on the connection status pill.
  - `@Namespace` + `.glassEffectID("connection-control", in:)` morphs the Connect
    button into the status pill (and between connecting/connected/failed) as one
    continuous glass element rather than three separate controls appearing and
    disappearing.
  - `.buttonStyle(.glassProminent)` on Connect, `.buttonStyle(.glass)` on Activate.
  - `.backgroundExtensionEffect()` on the detail column, so ticket inspector content
    bleeds correctly under the split view's edge.
  - The sidebar, content list, detail column, and toolbar all adopt Liquid Glass
    automatically from `NavigationSplitView`/`.toolbar` on macOS 26 — nothing here
    hand-rolls chrome the system already supplies, and nothing substitutes
    `.ultraThinMaterial`.
  - Reduce Transparency / Increase Contrast: no custom view in this app disables or
    special-cases either setting — every glass surface is a system API call with
    system content underneath (`Label`, `Text`, `TextField`), so the system's own
    Reduce Transparency / Increase Contrast substitution (opaque backgrounds,
    stronger borders) applies without this code fighting it. This has **not** been
    visually verified against a live server with those settings toggled on this
    machine — only reasoned about from the API surface used. Treat that as an open
    verification item, not a claim of having eyeballed it.

## What is not built

- **Menu bar extra** (`SPEC.md` §18.3 calls for one showing active work / needs-you
  count) — not built.
- **Native notifications** for `approval.requested` / `ticket.escalated` — not built.
- **Graph view** of the ticket graph (`GET /graph`) — not built; the content column is
  a flat ticket list only.
- **Saved views** in the sidebar — not built; sidebar is milestones + participants
  only, as a fixed pair of sections.
- **Everything beyond the one wired mutation**: create ticket, submit, verify, audit,
  close/cancel/reopen, lease acquire/heartbeat/release, decisions, artifacts,
  approvals — the routes exist server-side and `TicketmasterAPI` could grow to cover
  them, but only `activate` is wired into this client.
- **Reconnection/backoff**: `EventStreamClient` opens one `GET /events` connection and
  finishes (or fails) when it drops; there is no automatic retry loop.
- **Persisted connection settings**: the server URL/token/actor fields reset to
  defaults on every launch; nothing is written to `UserDefaults` or Keychain.
- **App icon, bundle identifier, entitlements, signing**: this is an `xcodebuild`
  scheme generated by Swift Package Manager from `Package.swift`, not a hand-built
  Xcode project — there is no `Info.plist`, no app icon, and no code signing
  configuration beyond Xcode's automatic defaults for a package-based scheme.
- **Reduce Transparency / Increase Contrast**, verified live: see above — reasoned
  about, not visually checked against a running server on this machine.

## How to run it

Test the package (no UI, no server required):

```sh
cd clients/macos
swift test
```

Build the app via the SwiftPM-generated Xcode scheme:

```sh
cd clients/macos
xcodebuild -scheme TicketmasterApp -destination 'platform=macOS' build
```

Or open `clients/macos` (the folder containing `Package.swift`) directly in Xcode —
Xcode 26 treats a `Package.swift` as an openable project and exposes
`TicketmasterApp`/`TicketmasterKit` as schemes — and run `TicketmasterApp` from
there (⌘R).

To connect it to a real server: start `tm-server` (e.g. via `tm serve` from
`crates/tm-cli`, once that binary exists and is wired to a project — see that
crate's own docs) bound to a loopback address, note the printed URL (default: an
ephemeral port on `127.0.0.1`, no bearer token required per
`crates/tm-server/src/auth.rs`), then in the app enter that URL in the toolbar's
server field and press **Connect**. The ticket list, milestones, and participants
populate from `GET /state`'s constituent endpoints; selecting a `Draft` ticket and
pressing **Activate** in the inspector exercises the one wired mutation.

## Verification actually run on this machine

```sh
cd clients/macos && swift test
# 17 tests, 3 suites, all passed, 0 failures.

cd clients/macos && xcodebuild -scheme TicketmasterApp -destination 'platform=macOS' build
# ** BUILD SUCCEEDED **
```

Not run: the app has not been launched and driven interactively against a live
`tm-server` on this machine, so the Liquid Glass rendering, the SSE reconnect path
against a real socket, and the Reduce Transparency/Increase Contrast behavior are
unverified beyond code review and the unit-tested parsing/view-model logic above.
