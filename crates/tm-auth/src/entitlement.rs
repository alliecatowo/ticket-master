//! [`Entitlement`]: what an [`crate::AuthAdapter`]'s credential is entitled to, per `SPEC.md`
//! §28.2 — "It reports what it is entitled to, not just whether it works" — and §31.1, which is
//! what the fabric routes on to tell metered spend from subscription capacity.

/// Which bucket of capacity a credential draws from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum QuotaClass {
    /// Pay-per-token API spend. The conservative default: an adapter that cannot introspect its
    /// real quota class should assume metered rather than silently claiming free/subscription
    /// capacity (§31.1: "metered spend should be conserved").
    #[default]
    Metered,
    /// Capacity within a consumer subscription's ceiling (Claude Pro/Max, ChatGPT Plus/Pro, a
    /// Gemini plan, ...), used instead of metered credits.
    Subscription,
    /// A free tier with no metered cost, distinct from a paid subscription's ceiling.
    Free,
}

/// What a resolved credential is entitled to: quota class, rate limits, and any daily ceiling.
/// `SPEC.md` §28.2: "Subscription and free-tier capacity are ordinary capacity with a ceiling,
/// not special cases" — the fabric routes on this struct exactly the same way for every
/// [`QuotaClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entitlement {
    /// Which capacity bucket this credential draws from.
    pub quota_class: QuotaClass,
    /// Requests per minute this credential is permitted, if known.
    pub rpm: Option<u32>,
    /// Tokens per minute this credential is permitted, if known.
    pub tpm: Option<u32>,
    /// Whether use of this credential is metered spend (`true`) or subscription/free capacity
    /// (`false`). Kept alongside `quota_class` rather than derived from it, because a
    /// `Subscription`-class credential can still have a metered overage tier once its ceiling is
    /// hit — this field is "does *this call* cost money", `quota_class` is "which bucket is it
    /// drawn from". Defaults to `true`: an adapter that cannot introspect real billing state
    /// should assume it costs money rather than silently claiming free usage.
    pub metered: bool,
    /// A hard daily spend/usage ceiling, in whatever unit the adapter tracks (tokens, requests,
    /// or micro-dollars — see the adapter's own docs), if this credential has one.
    pub daily_ceiling: Option<u64>,
}

/// Conservative by construction: metered, no known limits, no known ceiling. An adapter that
/// cannot introspect real quota returns this rather than guessing at generous numbers.
impl Default for Entitlement {
    fn default() -> Self {
        Entitlement {
            quota_class: QuotaClass::default(),
            rpm: None,
            tpm: None,
            metered: true,
            daily_ceiling: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_entitlement_is_conservative() {
        let e = Entitlement::default();
        assert_eq!(e.quota_class, QuotaClass::Metered);
        assert!(
            e.metered,
            "an unknown entitlement must assume it costs money"
        );
        assert_eq!(e.rpm, None);
        assert_eq!(e.tpm, None);
        assert_eq!(e.daily_ceiling, None);
    }
}
