/**
 * Routed placeholder for a view listed in SPEC.md §18.2 that this scaffolding pass did not
 * build. States plainly what's missing rather than implying the view works — see
 * ../../README.md for the exact list.
 */
export function NotBuilt({ view, contents }: { view: string; contents: string }) {
  return (
    <section className="not-built" data-testid="not-built">
      <h1>{view}</h1>
      <p className="not-built__notice">
        This view is <strong>not built yet</strong>. It is routed as a placeholder only.
      </p>
      <p className="not-built__spec">Per SPEC.md §18.2, this view should show: {contents}</p>
    </section>
  );
}
