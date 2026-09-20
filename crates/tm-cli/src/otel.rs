//! OpenTelemetry span export (D-008), compiled in only behind the `otel` Cargo feature and,
//! even then, only active when [`TM_OTEL_ENDPOINT_VAR`] is set at process start. This module
//! never changes what gets traced -- it adds an OTLP export layer alongside `main.rs`'s existing
//! stderr `fmt` layer, on the same `tracing` call sites the rest of the workspace already has, so
//! no call site anywhere else needs to change *for this decision to be correct*. Whether it
//! currently exports anything *useful* is a separate question, and the honest answer today is
//! "no": `tracing_opentelemetry::OpenTelemetryLayer` is span-shaped, not a general event/log
//! exporter -- it exports `#[instrument]`/`*_span!`-created spans as OTel spans, and turns
//! `tracing::info!`/`debug!` events *inside* a span into that span's OTel events, but drops a
//! bare event with no enclosing span outright. As of this writing this workspace has zero
//! `#[instrument]` or `*_span!` call sites (`rg '#\[instrument|_span!\('  crates/` -- every
//! `tracing::info!`/`debug!`/`warn!`/`error!` call in the tree is a bare event), so with this
//! feature on against a live collector, a `tm` invocation exports nothing yet. See D-008's "What
//! this costs" for why that's a deliberate, separate follow-up rather than folded into this
//! change: this module's job is the export plumbing being correct and inert-until-opted-in: what
//! it carries is downstream instrumentation work. The stderr `fmt` layer is unaffected either way
//! -- every one of those bare events still reaches it exactly as before.
//!
//! Split into a pure construction half ([`build_provider`], safe to call repeatedly and exactly
//! what the unit tests below exercise) and a process-global-mutating half ([`install`], which
//! calls [`tracing_subscriber::util::SubscriberInitExt::try_init`] and so can only meaningfully
//! succeed once per process) because the latter cannot be unit-tested more than once per test
//! binary without the second call spuriously failing on "already initialized".

use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig as _};
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// Env var gating OpenTelemetry export: the full OTLP/HTTP traces endpoint, e.g.
/// `http://localhost:4318/v1/traces`. Passed to the exporter as a programmatic endpoint
/// (`WithExportConfig::with_endpoint`), which -- unlike the bare-host form the OTel SDK accepts
/// via its own `OTEL_EXPORTER_OTLP_ENDPOINT` env var -- is used verbatim, with no `/v1/traces`
/// suffix appended; include the full path.
pub const TM_OTEL_ENDPOINT_VAR: &str = "TM_OTEL_ENDPOINT";

/// Bound on a single export request (`SpanExporterBuilder::with_timeout`) and on
/// `SdkTracerProvider::shutdown_with_timeout`'s final flush in [`super::TracingGuard`]'s drop.
/// Deliberately short and shared between both: a `tm` invocation that finished its real work
/// should not sit around for the OTLP SDK's own 10s/5s defaults just because `TM_OTEL_ENDPOINT`
/// points at a collector that's down or unreachable -- a stale env var should cost a few seconds
/// at exit, not feel like a hang.
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(3);

/// Read [`TM_OTEL_ENDPOINT_VAR`], treating unset or all-whitespace as "export disabled".
pub fn endpoint_from_env() -> Option<String> {
    std::env::var(TM_OTEL_ENDPOINT_VAR)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Build (but do not install) an OTLP HTTP span exporter and the [`SdkTracerProvider`] wrapping
/// it in a batch processor. Building the exporter does no network I/O -- the underlying HTTP
/// client connects lazily on first export -- so this is safe to call in tests without a live
/// collector; it only fails on a malformed `endpoint`.
pub fn build_provider(endpoint: &str) -> anyhow::Result<SdkTracerProvider> {
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|err| anyhow::anyhow!("building OTLP span exporter for {endpoint:?}: {err}"))?;

    let resource = Resource::builder().with_service_name("tm").build();

    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

/// Build the provider for `endpoint` and install a combined stderr-`fmt` + OTLP subscriber as
/// the process-wide default, returning the provider so the caller can flush it
/// (`SdkTracerProvider::shutdown_with_timeout`) before exit. Returns that provider rather than a
/// `Drop` guard because `main.rs` exits via `std::process::exit`, which skips `Drop` entirely --
/// the caller must call `shutdown_with_timeout` explicitly on the way out.
pub fn install(endpoint: &str) -> anyhow::Result<SdkTracerProvider> {
    let provider = build_provider(endpoint)?;
    let tracer = provider.tracer("tm");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(otel_layer)
        .try_init()
        .map_err(|err| anyhow::anyhow!("installing tracing subscriber with OTel layer: {err}"))?;

    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // `TM_OTEL_ENDPOINT_VAR` is process-global state; `#[serial]` keeps these from racing other
    // tests in this same binary that touch the same env var.

    #[test]
    #[serial]
    fn endpoint_from_env_is_none_when_unset() {
        std::env::remove_var(TM_OTEL_ENDPOINT_VAR);
        assert_eq!(endpoint_from_env(), None);
    }

    #[test]
    #[serial]
    fn endpoint_from_env_is_none_when_blank() {
        std::env::set_var(TM_OTEL_ENDPOINT_VAR, "   ");
        assert_eq!(endpoint_from_env(), None);
        std::env::remove_var(TM_OTEL_ENDPOINT_VAR);
    }

    #[test]
    #[serial]
    fn endpoint_from_env_is_some_when_set() {
        std::env::set_var(TM_OTEL_ENDPOINT_VAR, "http://127.0.0.1:0/v1/traces");
        assert_eq!(
            endpoint_from_env(),
            Some("http://127.0.0.1:0/v1/traces".to_string())
        );
        std::env::remove_var(TM_OTEL_ENDPOINT_VAR);
    }

    /// Building the exporter/provider is pure (no network I/O -- the HTTP client connects
    /// lazily), so this is safe without a live collector: port 0 never accepts a real
    /// connection, and shutting down a provider that never recorded a span exports an empty
    /// batch, which the SDK skips rather than making a real request. `#[serial]` because
    /// `build_client` (inside `build_provider`) reads `OTEL_EXPORTER_OTLP_*` env vars
    /// unconditionally even when every value we care about is supplied explicitly -- same
    /// process-global env hazard the other tests in this module serialize against.
    #[test]
    #[serial]
    fn build_provider_succeeds_for_well_formed_endpoint_without_a_live_collector() {
        let provider =
            build_provider("http://127.0.0.1:0/v1/traces").expect("exporter construction");
        provider
            .shutdown()
            .expect("shutdown with no recorded spans");
    }

    #[test]
    #[serial]
    fn build_provider_rejects_a_malformed_endpoint() {
        let err = build_provider("not a valid uri").expect_err("malformed endpoint must fail");
        assert!(err.to_string().contains("building OTLP span exporter"));
    }

    /// Backs [`otel_layer_alongside_does_not_change_fmt_output`]: a `Write` target that several
    /// subscriber builds can share so their formatted output is comparable in-process, without
    /// installing either as the real global default (`tracing::subscriber::with_default` scopes
    /// a subscriber to one closure via a thread-local, so this never touches the process-global
    /// state `install`'s `try_init` would, and is safe to run any number of times).
    #[derive(Clone, Default)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock poisoned").write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn emit_sample_events() {
        tracing::info!(answer = 42, "sample info event");
        tracing::debug!("sample debug event");
        tracing::warn!(error = "boom", "sample warn event");
    }

    /// Directly checks the "otherwise ... byte-for-byte identical" claim `install_tracing`'s doc
    /// comment and D-008 both make: that adding the OTel layer alongside the stderr `fmt` layer
    /// (`install`'s composition) does not change what that `fmt` layer prints, compared to
    /// `main.rs`'s original stand-alone `tracing_subscriber::fmt()` builder for the *same*
    /// events. Both sides disable ANSI and timestamps (`.with_ansi(false)`/`.without_time()`) --
    /// a deliberate simplification for this comparison only (the real, non-test code sets
    /// neither explicitly): two separate emissions of these events happen at different wall-clock
    /// instants, so raw timestamps would never compare equal regardless of whether the
    /// compositions are actually equivalent, and that's noise this test isn't about.
    #[test]
    fn otel_layer_alongside_does_not_change_fmt_output() {
        let baseline_buf = SharedBuf::default();
        let baseline_subscriber = tracing_subscriber::fmt()
            .with_writer(baseline_buf.clone())
            .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(baseline_subscriber, emit_sample_events);

        let combined_buf = SharedBuf::default();
        let provider =
            build_provider("http://127.0.0.1:0/v1/traces").expect("provider construction");
        let tracer = provider.tracer("tm");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let combined_subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new("trace"))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(combined_buf.clone())
                    .with_ansi(false)
                    .without_time(),
            )
            .with(otel_layer);
        tracing::subscriber::with_default(combined_subscriber, emit_sample_events);
        provider.shutdown().expect("shutdown");

        let baseline_output = String::from_utf8(baseline_buf.0.lock().expect("lock").clone())
            .expect("utf8 fmt output");
        let combined_output = String::from_utf8(combined_buf.0.lock().expect("lock").clone())
            .expect("utf8 fmt output");
        assert!(
            !baseline_output.is_empty(),
            "sanity: the sample events must actually have produced fmt output"
        );
        assert_eq!(
            baseline_output, combined_output,
            "adding the OTel layer must not change the stderr fmt layer's own output"
        );
    }
}
