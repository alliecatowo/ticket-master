//! Owns hybrid retrieval: weighted reciprocal-rank fusion over the semantic, lexical,
//! symbol-proximity, path-affinity, edit-recency and co-change signals, each independently
//! rankable, fused into one ordered result with a per-hit explanation so retrieval quality is
//! debuggable and benchmarkable.
//!
//! Weights are configurable (harness engineering tunes them in `harness.toml` upstream; this
//! module just accepts whatever [`SignalWeights`] it's given) rather than hard-coded, and
//! every [`RankedHit`] carries `explain: Vec<SignalContribution>` rather than an opaque score.

use crate::exact::Hit;
use crate::semantic::ScoredChunk;

/// A retrieval request: free-text query plus optional seeds that sharpen the symbol-proximity
/// and path-affinity signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// Free-text query, used by the semantic and lexical signals.
    pub text: String,
    /// Symbol ids already known to be relevant (e.g. the symbol a ticket is about), used to
    /// score symbol-graph proximity.
    pub seed_symbols: Vec<u64>,
    /// Paths already known to be relevant (e.g. a ticket's resource claims), used to score
    /// path affinity.
    pub seed_paths: Vec<String>,
}

/// Context a caller supplies so path-affinity, recency and co-change signals have something
/// to score against, beyond the query text itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetrievalContext {
    /// Path prefixes/globs the current ticket claims, for path-affinity scoring.
    pub claimed_paths: Vec<String>,
    /// Paths edited most recently, most recent first, for edit-recency scoring.
    pub recently_edited: Vec<String>,
}

/// One named signal's contribution to a hit's fused rank, kept per-hit so ranking is
/// debuggable rather than a single opaque score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalContribution {
    /// Which signal this is.
    pub signal: Signal,
    /// That signal's own rank for this hit (1 = best), or `None` if the signal did not
    /// surface this hit at all.
    pub rank: Option<u32>,
    /// The reciprocal-rank score this signal contributed to the fused total (already
    /// multiplied by the signal's configured weight).
    pub weighted_score: f32,
}

/// The individual retrieval signals fused by [`hybrid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// Cosine-ranked semantic search.
    Semantic,
    /// Token/lexical match strength.
    Lexical,
    /// Graph distance to `Query::seed_symbols`.
    SymbolProximity,
    /// Path closeness to `Query::seed_paths` / `RetrievalContext::claimed_paths`.
    PathAffinity,
    /// Recency of last edit, from `RetrievalContext::recently_edited`.
    EditRecency,
    /// Co-change frequency with `Query::seed_paths`, from git history.
    CoChange,
}

/// Per-signal weight applied before reciprocal-rank fusion. All default to `1.0`; harness
/// engineering tunes these from `harness.toml`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalWeights {
    /// Weight for [`Signal::Semantic`].
    pub semantic: f32,
    /// Weight for [`Signal::Lexical`].
    pub lexical: f32,
    /// Weight for [`Signal::SymbolProximity`].
    pub symbol_proximity: f32,
    /// Weight for [`Signal::PathAffinity`].
    pub path_affinity: f32,
    /// Weight for [`Signal::EditRecency`].
    pub edit_recency: f32,
    /// Weight for [`Signal::CoChange`].
    pub co_change: f32,
    /// The `k` constant in `1 / (k + rank)` reciprocal-rank fusion; higher flattens the curve
    /// so lower ranks still contribute meaningfully. Conventional default is 60.
    pub rrf_k: f32,
}

impl Default for SignalWeights {
    fn default() -> Self {
        SignalWeights {
            semantic: 1.0,
            lexical: 1.0,
            symbol_proximity: 1.0,
            path_affinity: 1.0,
            edit_recency: 1.0,
            co_change: 1.0,
            rrf_k: 60.0,
        }
    }
}

/// One fused, explainable search result.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedHit {
    /// Path of the hit.
    pub path: String,
    /// 1-based start line, inclusive, if the hit is line-addressable (chunk or exact match).
    pub line_start: Option<u32>,
    /// 1-based end line, inclusive.
    pub line_end: Option<u32>,
    /// Text snippet to display.
    pub snippet: String,
    /// Final fused score (sum of weighted per-signal reciprocal ranks); higher is better.
    pub fused_score: f32,
    /// Per-signal breakdown explaining `fused_score`.
    pub explain: Vec<SignalContribution>,
}

/// A snippet lookup callback: given a hit's `(path, line_start)`, returns its display snippet
/// and resolved `line_end`. Kept as a named alias since the raw `dyn Fn` signature reads as
/// unbounded complexity to clippy.
pub type SnippetLookup<'a> = dyn Fn(&str, Option<u32>) -> (String, Option<u32>) + 'a;

/// A per-signal ranked list feeding fusion: an ordered sequence of (identity, raw score)
/// pairs, where identity is whatever key [`hybrid`] uses to align hits across signals (here,
/// `path` plus an optional line-start, since different signals address different
/// granularities).
#[derive(Debug, Clone, PartialEq)]
pub struct SignalRanking {
    /// Which signal produced this ranking.
    pub signal: Signal,
    /// Hits in descending relevance order as this signal sees it.
    pub ranked: Vec<(String, Option<u32>)>,
}

/// Fuse per-signal rankings into one explainable, weighted result list.
///
/// This is the pure fusion core: it does not run any of the individual retrieval modes
/// itself (callers gather [`SignalRanking`]s from [`crate::semantic::SemanticSearch`],
/// [`crate::exact::ExactSearch`], [`crate::symbols::SymbolIndex`] and
/// [`crate::history::HistoryIndex`] beforehand, typically via [`crate::api::CodeIntel`]),
/// which keeps it unit-testable without a database.
pub fn hybrid(
    _query: &Query,
    _ctx: &RetrievalContext,
    signals: &[SignalRanking],
    weights: SignalWeights,
    snippet_lookup: &SnippetLookup<'_>,
) -> Vec<RankedHit> {
    use std::collections::{BTreeMap, HashMap};

    // Map from (path, line_start) to signal contributions.
    let mut accumulated: BTreeMap<(String, Option<u32>), HashMap<Signal, SignalContribution>> =
        BTreeMap::new();

    // Process each signal's ranking.
    for signal_ranking in signals {
        // Get the weight for this signal.
        let weight = match signal_ranking.signal {
            Signal::Semantic => weights.semantic,
            Signal::Lexical => weights.lexical,
            Signal::SymbolProximity => weights.symbol_proximity,
            Signal::PathAffinity => weights.path_affinity,
            Signal::EditRecency => weights.edit_recency,
            Signal::CoChange => weights.co_change,
        };

        // Process each hit in this signal's ranking.
        for (rank_idx, (path, line_start)) in signal_ranking.ranked.iter().enumerate() {
            let rank = (rank_idx + 1) as u32; // 1-based rank
            let weighted_score = weight / (weights.rrf_k + rank as f32);

            let key = (path.clone(), *line_start);
            accumulated.entry(key).or_insert_with(HashMap::new).insert(
                signal_ranking.signal,
                SignalContribution {
                    signal: signal_ranking.signal,
                    rank: Some(rank),
                    weighted_score,
                },
            );
        }
    }

    // Define the canonical signal order for consistent output.
    let signal_order = [
        Signal::Semantic,
        Signal::Lexical,
        Signal::SymbolProximity,
        Signal::PathAffinity,
        Signal::EditRecency,
        Signal::CoChange,
    ];

    // Build results.
    let mut results = Vec::new();
    for ((path, line_start), signal_contributions) in accumulated {
        // Build explain vector in canonical order, including missing signals.
        let mut explain = Vec::new();
        for signal in &signal_order {
            let contribution =
                signal_contributions
                    .get(signal)
                    .copied()
                    .unwrap_or(SignalContribution {
                        signal: *signal,
                        rank: None,
                        weighted_score: 0.0,
                    });
            explain.push(contribution);
        }

        // Calculate fused score.
        let fused_score: f32 = explain.iter().map(|c| c.weighted_score).sum();

        // Resolve snippet and line_end.
        let (snippet, line_end) = snippet_lookup(&path, line_start);

        results.push(RankedHit {
            path,
            line_start,
            line_end,
            snippet,
            fused_score,
            explain,
        });
    }

    // Sort by fused_score descending, then by identity for determinism.
    results.sort_by(|a, b| {
        b.fused_score
            .partial_cmp(&a.fused_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                // Sort by path, then by line_start.
                a.path
                    .cmp(&b.path)
                    .then_with(|| a.line_start.cmp(&b.line_start))
            })
    });

    results
}

/// Build a [`SignalRanking`] for [`Signal::Semantic`] from raw [`ScoredChunk`]s (already
/// cosine-ranked by [`crate::semantic::SemanticSearch::search`]).
pub fn semantic_ranking(chunks: &[ScoredChunk]) -> SignalRanking {
    SignalRanking {
        signal: Signal::Semantic,
        ranked: chunks
            .iter()
            .map(|c| (c.path.clone(), Some(c.line_start)))
            .collect(),
    }
}

/// Build a [`SignalRanking`] for [`Signal::Lexical`] from raw exact-search [`Hit`]s, in the
/// order [`crate::exact::ExactSearch`] already returned them (file-then-line order is used as
/// a relevance proxy: earlier files/lines rank higher).
pub fn lexical_ranking(hits: &[Hit]) -> SignalRanking {
    SignalRanking {
        signal: Signal::Lexical,
        ranked: hits
            .iter()
            .map(|h| (h.path.clone(), Some(h.line)))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hybrid_empty_signals() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("".to_string(), None)
        });

        assert!(results.is_empty());
    }

    #[test]
    fn test_hybrid_single_signal_single_hit() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![SignalRanking {
            signal: Signal::Semantic,
            ranked: vec![("file.rs".to_string(), Some(10))],
        }];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), Some(12))
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "file.rs");
        assert_eq!(results[0].line_start, Some(10));
        assert_eq!(results[0].line_end, Some(12));
        assert_eq!(results[0].snippet, "snippet");

        // Check fused score: 1.0 / (60.0 + 1.0) = 0.01639...
        let expected_score = 1.0 / (60.0 + 1.0);
        assert!((results[0].fused_score - expected_score).abs() < 0.0001);

        // Check explain has all 6 signals
        assert_eq!(results[0].explain.len(), 6);
        // Semantic should have rank 1
        assert_eq!(results[0].explain[0].signal, Signal::Semantic);
        assert_eq!(results[0].explain[0].rank, Some(1));

        // Others should have no rank
        for i in 1..6 {
            assert_eq!(results[0].explain[i].rank, None);
            assert_eq!(results[0].explain[i].weighted_score, 0.0);
        }
    }

    #[test]
    fn test_hybrid_multiple_signals_fused() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![
            SignalRanking {
                signal: Signal::Semantic,
                ranked: vec![("file.rs".to_string(), Some(10))],
            },
            SignalRanking {
                signal: Signal::Lexical,
                ranked: vec![("file.rs".to_string(), Some(10))],
            },
        ];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), Some(12))
        });

        assert_eq!(results.len(), 1);

        // Both signals should contribute to the score
        let expected_score = 2.0 * (1.0 / (60.0 + 1.0)); // 2 signals, both at rank 1
        assert!((results[0].fused_score - expected_score).abs() < 0.0001);

        // Check that both signals have rank
        assert_eq!(results[0].explain[0].signal, Signal::Semantic);
        assert_eq!(results[0].explain[0].rank, Some(1));
        assert_eq!(results[0].explain[1].signal, Signal::Lexical);
        assert_eq!(results[0].explain[1].rank, Some(1));
    }

    #[test]
    fn test_hybrid_different_ranks_across_signals() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![
            SignalRanking {
                signal: Signal::Semantic,
                ranked: vec![("file.rs".to_string(), Some(10))],
            },
            SignalRanking {
                signal: Signal::Lexical,
                ranked: vec![
                    ("other.rs".to_string(), Some(5)),
                    ("file.rs".to_string(), Some(10)),
                ],
            },
        ];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|path, line| {
            let snippet = if path == "file.rs" { "file" } else { "other" };
            (snippet.to_string(), line.map(|l| l + 2))
        });

        // Should have 2 results: file.rs and other.rs
        assert_eq!(results.len(), 2);

        // file.rs should rank higher (has both signals)
        assert_eq!(results[0].path, "file.rs");
        assert_eq!(results[1].path, "other.rs");

        // Verify scores
        let file_score = 1.0 / 61.0 + 1.0 / 62.0; // semantic rank 1, lexical rank 2
        let other_score = 1.0 / 61.0; // lexical rank 1
        assert!((results[0].fused_score - file_score).abs() < 0.0001);
        assert!((results[1].fused_score - other_score).abs() < 0.0001);
    }

    #[test]
    fn test_hybrid_sorting_by_score_descending() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![SignalRanking {
            signal: Signal::Semantic,
            ranked: vec![
                ("high.rs".to_string(), None), // rank 1
                ("low.rs".to_string(), None),  // rank 2
            ],
        }];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|path, _| {
            (format!("snippet for {}", path), None)
        });

        assert_eq!(results.len(), 2);
        assert!(results[0].fused_score > results[1].fused_score);
        assert_eq!(results[0].path, "high.rs");
        assert_eq!(results[1].path, "low.rs");
    }

    #[test]
    fn test_hybrid_deterministic_tiebreaking() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        // Two different signals, each ranking a different path first, with equal default
        // weights: both hits get the same fused score (1.0 / 61.0), so the tie must be broken
        // by path.
        let signals = vec![
            SignalRanking {
                signal: Signal::Semantic,
                ranked: vec![("b.rs".to_string(), Some(10))],
            },
            SignalRanking {
                signal: Signal::Lexical,
                ranked: vec![("a.rs".to_string(), Some(10))],
            },
        ];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), Some(12))
        });

        // Both have the same score, should be sorted by path
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].path, "a.rs");
        assert_eq!(results[1].path, "b.rs");
    }

    #[test]
    fn test_hybrid_weighted_signals() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![
            SignalRanking {
                signal: Signal::Semantic,
                ranked: vec![("file.rs".to_string(), Some(10))],
            },
            SignalRanking {
                signal: Signal::Lexical,
                ranked: vec![("file.rs".to_string(), Some(10))],
            },
        ];
        let mut weights = SignalWeights::default();
        weights.semantic = 2.0;
        weights.lexical = 0.5;

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), Some(12))
        });

        assert_eq!(results.len(), 1);

        // Score should be 2.0 / 61.0 + 0.5 / 61.0 = 2.5 / 61.0
        let expected_score = 2.5 / 61.0;
        assert!((results[0].fused_score - expected_score).abs() < 0.0001);
    }

    #[test]
    fn test_hybrid_rrf_k_parameter() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![SignalRanking {
            signal: Signal::Semantic,
            ranked: vec![("file.rs".to_string(), Some(10))],
        }];
        let mut weights = SignalWeights::default();
        weights.rrf_k = 100.0;

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), Some(12))
        });

        assert_eq!(results.len(), 1);

        // Score should be 1.0 / (100.0 + 1.0) = 1.0 / 101.0
        let expected_score = 1.0 / 101.0;
        assert!((results[0].fused_score - expected_score).abs() < 0.0001);
    }

    #[test]
    fn test_hybrid_explain_completeness() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![SignalRanking {
            signal: Signal::Semantic,
            ranked: vec![("file.rs".to_string(), None)],
        }];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), None)
        });

        assert_eq!(results.len(), 1);

        // All 6 signals should be in explain
        assert_eq!(results[0].explain.len(), 6);

        let signals_in_explain: Vec<Signal> = results[0].explain.iter().map(|c| c.signal).collect();
        assert_eq!(
            signals_in_explain,
            vec![
                Signal::Semantic,
                Signal::Lexical,
                Signal::SymbolProximity,
                Signal::PathAffinity,
                Signal::EditRecency,
                Signal::CoChange,
            ]
        );
    }

    #[test]
    fn test_hybrid_snippet_lookup_called() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![SignalRanking {
            signal: Signal::Semantic,
            ranked: vec![("file.rs".to_string(), Some(42))],
        }];
        let weights = SignalWeights::default();

        let lookup_calls = std::cell::RefCell::new(Vec::new());
        let results = hybrid(&query, &ctx, &signals, weights, &|path, line| {
            lookup_calls.borrow_mut().push((path.to_string(), line));
            ("resolved snippet".to_string(), Some(45))
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].snippet, "resolved snippet");
        assert_eq!(results[0].line_end, Some(45));
        let lookup_calls = lookup_calls.into_inner();
        assert_eq!(lookup_calls.len(), 1);
        assert_eq!(lookup_calls[0], ("file.rs".to_string(), Some(42)));
    }

    #[test]
    fn test_hybrid_multiple_signals_disjoint_hits() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![
            SignalRanking {
                signal: Signal::Semantic,
                ranked: vec![("semantic.rs".to_string(), None)],
            },
            SignalRanking {
                signal: Signal::Lexical,
                ranked: vec![("lexical.rs".to_string(), None)],
            },
        ];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|path, _| {
            (format!("snippet {}", path), None)
        });

        assert_eq!(results.len(), 2);

        // Both should have score 1.0 / 61.0, sorted by path
        assert_eq!(results[0].path, "lexical.rs");
        assert_eq!(results[1].path, "semantic.rs");
    }

    #[test]
    fn test_hybrid_no_line_start() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![SignalRanking {
            signal: Signal::Semantic,
            ranked: vec![("file.rs".to_string(), None)],
        }];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), Some(42))
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "file.rs");
        assert_eq!(results[0].line_start, None);
        assert_eq!(results[0].line_end, Some(42));
    }

    #[test]
    fn test_hybrid_three_signals_partial_coverage() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![
            SignalRanking {
                signal: Signal::Semantic,
                ranked: vec![
                    ("file.rs".to_string(), Some(10)),
                    ("other.rs".to_string(), Some(20)),
                ],
            },
            SignalRanking {
                signal: Signal::Lexical,
                ranked: vec![
                    ("file.rs".to_string(), Some(10)),
                    ("third.rs".to_string(), Some(30)),
                ],
            },
            SignalRanking {
                signal: Signal::SymbolProximity,
                ranked: vec![("other.rs".to_string(), Some(20))],
            },
        ];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), None)
        });

        assert_eq!(results.len(), 3);

        // file.rs has both semantic and lexical
        // other.rs has both semantic and symbol_proximity
        // third.rs has only lexical
        // All three should have full explain vectors
        for result in &results {
            assert_eq!(result.explain.len(), 6);
            // Check that every signal appears
            let has_all_signals = result.explain.iter().all(|c| {
                [
                    Signal::Semantic,
                    Signal::Lexical,
                    Signal::SymbolProximity,
                    Signal::PathAffinity,
                    Signal::EditRecency,
                    Signal::CoChange,
                ]
                .contains(&c.signal)
            });
            assert!(has_all_signals);
        }

        // Verify scores
        let file_score = 1.0 / 61.0 + 1.0 / 61.0; // semantic rank 1, lexical rank 1
        let other_score = 1.0 / 62.0 + 1.0 / 61.0; // semantic rank 2, symbol_proximity rank 1
        let third_score = 1.0 / 62.0; // lexical rank 2

        assert!((results[0].fused_score - file_score).abs() < 0.0001);
        assert!((results[1].fused_score - other_score).abs() < 0.0001);
        assert!((results[2].fused_score - third_score).abs() < 0.0001);
    }

    #[test]
    fn test_hybrid_deterministic_line_start_tiebreaker() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        // Three different signals, each ranking a different line of the same file first, with
        // equal default weights: all three hits get the same fused score (1.0 / 61.0), so the
        // tie must be broken by line_start.
        let signals = vec![
            SignalRanking {
                signal: Signal::Semantic,
                ranked: vec![("file.rs".to_string(), Some(20))],
            },
            SignalRanking {
                signal: Signal::Lexical,
                ranked: vec![("file.rs".to_string(), Some(10))],
            },
            SignalRanking {
                signal: Signal::SymbolProximity,
                ranked: vec![("file.rs".to_string(), Some(30))],
            },
        ];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), None)
        });

        // All same score (1.0 / 61.0), same path, should be sorted by line_start
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].line_start, Some(10));
        assert_eq!(results[1].line_start, Some(20));
        assert_eq!(results[2].line_start, Some(30));
    }

    #[test]
    fn test_hybrid_high_rank_gets_low_score() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![SignalRanking {
            signal: Signal::Semantic,
            ranked: vec![
                ("file.rs".to_string(), Some(10)), // rank 1
                ("file.rs".to_string(), Some(20)), // rank 2
                ("file.rs".to_string(), Some(30)), // rank 3
            ],
        }];
        let weights = SignalWeights::default();

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), None)
        });

        assert_eq!(results.len(), 3);
        // Earlier ranks should have higher scores
        assert!(results[0].fused_score > results[1].fused_score);
        assert!(results[1].fused_score > results[2].fused_score);
    }

    #[test]
    fn test_hybrid_zero_weight_signal() {
        let query = Query {
            text: "test".to_string(),
            seed_symbols: vec![],
            seed_paths: vec![],
        };
        let ctx = RetrievalContext::default();
        let signals = vec![
            SignalRanking {
                signal: Signal::Semantic,
                ranked: vec![("file.rs".to_string(), Some(10))],
            },
            SignalRanking {
                signal: Signal::Lexical,
                ranked: vec![("file.rs".to_string(), Some(10))],
            },
        ];
        let mut weights = SignalWeights::default();
        weights.lexical = 0.0;

        let results = hybrid(&query, &ctx, &signals, weights, &|_, _| {
            ("snippet".to_string(), Some(12))
        });

        assert_eq!(results.len(), 1);

        // Only semantic should contribute
        let expected_score = 1.0 / 61.0;
        assert!((results[0].fused_score - expected_score).abs() < 0.0001);

        // Lexical should have a rank but zero weighted_score
        assert_eq!(results[0].explain[1].signal, Signal::Lexical);
        assert_eq!(results[0].explain[1].rank, Some(1));
        assert_eq!(results[0].explain[1].weighted_score, 0.0);
    }
}
