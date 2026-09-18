//! Retrieval patterns ported from the RAG batch (arsenal B2).
//!
//! Four ports, all zero-dependency and all *patterns* — no model weights, no
//! server, no vector store:
//!
//! - **docling `to_chunks`** — document → chunk spans with byte offsets, so
//!   a hit can be quoted and re-read (`read path:lo-hi`) instead of trusted.
//! - **pgvector recency decay** — similarity × time decay, the ranking half
//!   of a vector store's `ORDER BY`.
//! - **flagembedding two-stage** — cheap coarse recall, then a finer local
//!   score over the survivors: the shape of every embed→rerank pipeline,
//!   expressed lexically because the weights are out of scope.
//! - **sentence-transformers traits + colbert maxsim** — the interfaces
//!   (`Embedder`, `Reranker`) and the late-interaction scoring function, so
//!   a real backend can be dropped in later without moving call sites.
//!
//! `HashingEmbedder` is the deterministic stand-in used by tests and by
//! any offline path: a hashed bag of tokens, L2-normalized. It is a
//! *lexical* embedding, stated as such — it makes ranking testable, not
//! semantic retrieval possible.
//! `// DEFERRED(owner): real embedding/reranking backends, a persistent
//! vector index, and hierarchical (parent/child) chunk linking — this batch
//! lands the interfaces, the chunker, and the scoring math only.`

/// One chunk of a document: the text plus its byte span in the source, so
/// every chunk is quotable and re-readable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// Split `text` into chunks of at most `max_chars` characters (char
/// counts, never bytes — a multi-byte boundary is never cut), preferring
/// paragraph boundaries, with `overlap` characters of trailing context
/// repeated at the head of each following chunk.
///
/// Invariants (asserted by the tests):
/// - every `chunk.text` equals `&text[chunk.start..chunk.end]`;
/// - every chunk is at most `max_chars` characters;
/// - the spans cover the whole text in order (overlap repeats, it never
///   skips);
/// - `overlap` is capped at half the budget so chunks always make progress.
pub fn to_chunks(text: &str, max_chars: usize, overlap: usize) -> Vec<Chunk> {
    if text.is_empty() {
        return Vec::new();
    }
    let max_chars = max_chars.max(1);
    let overlap = overlap.min(max_chars / 2);
    let pieces = hard_split(paragraph_spans(text), text, max_chars);
    if pieces.is_empty() {
        return Vec::new();
    }
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut i = 0usize;
    while i < pieces.len() {
        // Budget for everything after the first chunk is reduced by the
        // overlap it will re-read from its predecessor.
        let budget = if chunks.is_empty() {
            max_chars
        } else {
            max_chars - overlap
        };
        let first = pieces[i].0;
        let mut end = pieces[i].1;
        let mut j = i + 1;
        while j < pieces.len() && pieces[j].1 - first <= budget {
            end = pieces[j].1;
            j += 1;
        }
        let mut start = first;
        if let Some(prev) = chunks.last() {
            // Only re-read as much context as the cap still allows: a chunk
            // that is already at the limit takes no overlap (the cap wins
            // over context — a chunk longer than `max_chars` breaks both
            // the promise and the downstream embedding budget).
            let room = max_chars.saturating_sub(end - first);
            let back = overlap.min(room);
            if back > 0 {
                let widened = floor_boundary(text, first.saturating_sub(back));
                // Never step backwards past the previous chunk's own start:
                // progress first, context second.
                if widened > prev.start {
                    start = widened;
                }
            }
        }
        chunks.push(Chunk {
            text: text[start..end].to_string(),
            start,
            end,
        });
        i = j;
    }
    chunks
}

/// Byte spans of the paragraphs (maximal runs of non-blank lines), each
/// extended to the next paragraph's start so the blank separator lines
/// belong to the paragraph they follow. That keeps the spans gapless: a
/// blank line is context, never a hole between two chunks.
fn paragraph_spans(text: &str) -> Vec<(usize, usize)> {
    let mut starts: Vec<(usize, usize)> = Vec::new();
    let mut open: Option<(usize, usize)> = None;
    let mut off = 0usize;
    for line in text.split_inclusive('\n') {
        if line.trim().is_empty() {
            if let Some(span) = open.take() {
                starts.push(span);
            }
        } else {
            let end = off + line.len();
            open = Some(match open {
                Some((s, _)) => (s, end),
                None => (off, end),
            });
        }
        off += line.len();
    }
    if let Some(span) = open {
        starts.push(span);
    }
    let mut out = Vec::with_capacity(starts.len());
    for i in 0..starts.len() {
        let start = starts[i].0;
        let end = match starts.get(i + 1) {
            Some(next) => next.0,
            None => text.len(),
        };
        if end > start {
            out.push((start, end));
        }
    }
    out
}

/// Cut spans longer than `max` into `max`-char windows at char boundaries.
fn hard_split(spans: Vec<(usize, usize)>, text: &str, max: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (s, e) in spans {
        let mut cur = s;
        while e.saturating_sub(cur) > max {
            let cut = floor_boundary(text, cur + max);
            if cut <= cur {
                break; // no progress possible (shouldn't happen: max ≥ 1)
            }
            out.push((cur, cut));
            cur = cut;
        }
        if e > cur {
            out.push((cur, e));
        }
    }
    out
}

/// Largest char boundary at or below `idx`.
fn floor_boundary(text: &str, idx: usize) -> usize {
    let mut i = idx.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// pgvector-style recency decay: `0.5^(age / half_life)`. A non-positive
/// half-life or age means "no decay" (1.0) — a zero half-life must not
/// divide the ranking by zero.
pub fn recency_decay(age_secs: f64, half_life_secs: f64) -> f64 {
    if half_life_secs <= 0.0 {
        return 1.0;
    }
    if age_secs <= 0.0 {
        return 1.0;
    }
    0.5f64.powf(age_secs / half_life_secs)
}

/// Rank `(id, similarity, age_secs)` hits by `similarity × recency_decay`,
/// best first. Ties break on id so the order is deterministic (a stable
/// order is what makes a retrieval result reproducible).
pub fn ranked_hits(hits: &[(String, f32, f64)], half_life_secs: f64) -> Vec<(String, f64)> {
    let mut scored: Vec<(String, f64)> = hits
        .iter()
        .map(|(id, sim, age)| {
            (
                id.clone(),
                f64::from(*sim) * recency_decay(*age, half_life_secs),
            )
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored
}

/// Cosine similarity, 0.0 when either side is zero-length or degenerate
/// (an all-zero vector has no direction — scoring it 0 keeps ranking sane).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// flagembedding's two-stage shape: a cheap coarse pass picks `coarse_k`
/// candidates, then a finer score re-ranks just those. Both stages are
/// lexical here (unigram overlap, then unigram+bigram) — the *structure* is
/// the port; swapping in real embeddings changes the two scoring closures,
/// not the pipeline.
pub fn two_stage(query: &str, docs: &[&str], coarse_k: usize) -> Vec<(usize, f32)> {
    if docs.is_empty() || coarse_k == 0 {
        return Vec::new();
    }
    let q = tokens(query);
    let mut coarse: Vec<(usize, f32)> = docs
        .iter()
        .enumerate()
        .map(|(i, d)| (i, unigram_overlap(&q, &tokens(d))))
        .collect();
    // Deterministic: score desc, then document order.
    coarse.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    coarse.truncate(coarse_k.min(coarse.len()));
    let q_bigrams = bigrams(&q);
    let mut fine: Vec<(usize, f32)> = coarse
        .into_iter()
        .map(|(i, uni)| {
            let d = tokens(docs[i]);
            let bi = unigram_overlap(&q_bigrams, &bigrams(&d));
            (i, 0.5 * uni + 0.5 * bi)
        })
        .collect();
    fine.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    fine
}

/// Lowercased alphanumeric tokens (zero-dep stand-in for a tokenizer).
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

fn bigrams(tokens: &[String]) -> Vec<String> {
    tokens
        .windows(2)
        .map(|w| format!("{} {}", w[0], w[1]))
        .collect()
}

/// |A ∩ B| / |A| — a recall-flavored overlap (missing query terms hurt).
fn unigram_overlap(query: &[String], doc: &[String]) -> f32 {
    if query.is_empty() {
        return 0.0;
    }
    let hits = query.iter().filter(|q| doc.contains(q)).count();
    hits as f32 / query.len() as f32
}

/// The sentence-transformers `Embedder` shape: batch in, vectors out, fixed
/// dimension. Implementations must be deterministic — retrieval ranking is
/// only reproducible if embedding is.
pub trait Embedder {
    fn dim(&self) -> usize;
    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>>;
}

/// The `Reranker` shape: one score per document, higher = more relevant.
pub trait Reranker {
    fn rerank(&self, query: &str, docs: &[&str]) -> Vec<f32>;
}

/// Deterministic hashed bag-of-words embedding (FNV-1a, L2-normalized).
/// Lexical, not semantic — stated so nobody mistakes it for a model.
#[derive(Debug, Clone, Copy)]
pub struct HashingEmbedder {
    pub dim: usize,
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        HashingEmbedder { dim: 256 }
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.dim.max(1)
    }

    fn embed(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; self.dim()];
                for tok in tokens(t) {
                    let h = fnv1a(&tok) % self.dim() as u64;
                    v[h as usize] += 1.0;
                }
                let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for x in v.iter_mut() {
                        *x /= norm;
                    }
                }
                v
            })
            .collect()
    }
}

/// Lexical reranker: unigram + bigram overlap against the query.
#[derive(Debug, Clone, Copy, Default)]
pub struct LexicalReranker;

impl Reranker for LexicalReranker {
    fn rerank(&self, query: &str, docs: &[&str]) -> Vec<f32> {
        let q = tokens(query);
        let qb = bigrams(&q);
        docs.iter()
            .map(|d| {
                let t = tokens(d);
                0.5 * unigram_overlap(&q, &t) + 0.5 * unigram_overlap(&qb, &bigrams(&t))
            })
            .collect()
    }
}

/// FNV-1a over bytes — a stable hash with no dependency (the hasher must
/// not change between runs, or stored vectors stop matching).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Per-token vectors (ColBERT's late-interaction input), using any
/// `Embedder` — one vector per token instead of one per document.
pub fn token_vectors(embedder: &impl Embedder, text: &str) -> Vec<Vec<f32>> {
    let toks: Vec<String> = tokens(text);
    let refs: Vec<&str> = toks.iter().map(String::as_str).collect();
    embedder.embed(&refs)
}

/// ColBERT maxsim: for every query token take its best cosine against any
/// document token, then sum. Empty on either side scores 0.
pub fn maxsim(query: &[Vec<f32>], doc: &[Vec<f32>]) -> f32 {
    if query.is_empty() || doc.is_empty() {
        return 0.0;
    }
    let mut total = 0.0f32;
    for q in query {
        let best = doc
            .iter()
            .map(|d| cosine(q, d))
            .fold(f32::NEG_INFINITY, f32::max);
        if best.is_finite() {
            total += best;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> String {
        let mut s = String::new();
        for p in 0..5 {
            s.push_str(&format!(
                "Paragraph {p}: {} \n\n",
                "word ".repeat(20).trim_end()
            ));
        }
        // A long final paragraph so the hard-split path runs.
        s.push_str(&"tail ".repeat(300));
        s
    }

    #[test]
    fn chunks_respect_bounds_offsets_and_coverage() {
        let text = doc();
        for (max, overlap) in [(40usize, 0usize), (120, 20), (1, 5), (10_000, 7)] {
            let chunks = to_chunks(&text, max, overlap);
            assert!(!chunks.is_empty(), "max={max} produced nothing");
            let mut covered = 0usize;
            for c in &chunks {
                assert_eq!(c.text, text[c.start..c.end], "span/text mismatch");
                assert!(
                    c.text.chars().count() <= max,
                    "chunk of {} chars exceeds {max}",
                    c.text.chars().count()
                );
                assert!(c.end > c.start, "empty chunk span");
                assert!(c.start >= covered || overlap > 0, "gap before {c:?}");
                covered = covered.max(c.end);
            }
            // Spans are ordered and cover the document end to end.
            for w in chunks.windows(2) {
                assert!(w[0].start < w[1].start, "must progress: {w:?}");
                assert!(w[0].end <= w[1].end);
            }
            assert_eq!(chunks[0].start, 0, "the first chunk starts at the head");
            assert_eq!(
                covered,
                text.len(),
                "the last chunk reaches the end (max={max})"
            );
        }
        assert!(to_chunks("", 10, 2).is_empty(), "empty text → no chunks");
    }

    #[test]
    fn chunks_prefer_paragraph_boundaries() {
        let text = "one\n\ntwo\n\nthree\n";
        let chunks = to_chunks(text, 9, 0);
        // Each paragraph carries its own blank separator, so the spans are
        // gapless; packing two of them would exceed the 9-char budget.
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert_eq!(chunks[0].text, "one\n\n");
        assert_eq!(chunks[1].text, "two\n\n");
        assert_eq!(chunks[2].text, "three\n");
        assert_eq!(chunks[0].end, chunks[1].start, "gapless spans");
        // The same text at a wider budget packs the first two together.
        let wide = to_chunks(text, 10, 0);
        assert_eq!(wide.len(), 2, "{wide:?}");
        assert_eq!(wide[0].text, "one\n\ntwo\n\n");
        // Single paragraphs longer than the budget are hard-split.
        let long = "x".repeat(25);
        let chunks = to_chunks(&long, 10, 0);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.iter().map(|c| c.text.len()).sum::<usize>(), 25);
    }

    #[test]
    fn recency_decay_halves_at_the_half_life_and_ranks() {
        assert!((recency_decay(0.0, 100.0) - 1.0).abs() < 1e-9);
        assert!((recency_decay(100.0, 100.0) - 0.5).abs() < 1e-9);
        assert!((recency_decay(200.0, 100.0) - 0.25).abs() < 1e-9);
        // Degenerate half-life: no decay, never a division by zero.
        assert!((recency_decay(50.0, 0.0) - 1.0).abs() < 1e-9);

        let hits = vec![
            ("fresh".to_string(), 0.80f32, 0.0f64),
            ("stale".to_string(), 0.95f32, 1_000.0),
            ("mid".to_string(), 0.90f32, 100.0),
        ];
        let ranked = ranked_hits(&hits, 100.0);
        assert_eq!(
            ranked.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["fresh", "mid", "stale"],
            "{ranked:?}"
        );
        // A perfect-similarity stale hit loses to a good fresh one.
        assert!(ranked[0].1 > ranked[2].1);
    }

    #[test]
    fn two_stage_recalls_then_reranks_within_the_coarse_set() {
        let docs = [
            "unrelated content about gardening",
            "the parser handles nested braces carefully",
            "the parser nested braces are nested braces",
        ];
        let out = two_stage("parser nested braces", &docs, 3);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0].0, 2,
            "the fine pass prefers the bigram-rich document: {out:?}"
        );
        assert!(out.iter().all(|(i, _)| *i < docs.len()));
        // coarse_k bounds the second stage, and 0 is an empty result.
        assert_eq!(two_stage("parser nested braces", &docs, 1).len(), 1);
        assert!(two_stage("q", &docs, 0).is_empty());
        assert!(two_stage("q", &[], 3).is_empty());
    }

    #[test]
    fn hashing_embedder_is_deterministic_and_normalized() {
        let e = HashingEmbedder::default();
        let a = e.embed(&["the parser handles braces"]);
        let b = e.embed(&["the parser handles braces"]);
        assert_eq!(a, b, "embedding must be reproducible");
        assert_eq!(a[0].len(), e.dim());
        let norm: f32 = a[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "unit norm, got {norm}");
        // Identical text → 1.0; disjoint vocabulary → 0.0.
        assert!((cosine(&a[0], &b[0]) - 1.0).abs() < 1e-5);
        let c = e.embed(&["zzz qqq"]);
        assert!(cosine(&a[0], &c[0]).abs() < 1e-5, "{:?}", c);
        // Degenerate inputs score 0 rather than NaN.
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn lexical_reranker_orders_by_overlap() {
        let r = LexicalReranker;
        let docs = ["gardening tips", "parser handles nested braces", "braces"];
        let scores = r.rerank("nested braces parser", &docs);
        assert_eq!(scores.len(), 3);
        assert!(scores[1] > scores[2], "full overlap beats partial");
        assert!(scores[1] > scores[0]);
        assert!(r.rerank("anything", &[]).is_empty());
    }

    #[test]
    fn maxsim_sums_best_matches_per_query_token() {
        let e = HashingEmbedder::default();
        let q = token_vectors(&e, "parser braces");
        let same = token_vectors(&e, "parser braces");
        assert_eq!(q.len(), 2);
        assert!(
            (maxsim(&q, &same) - 2.0).abs() < 1e-4,
            "identical token sets score one per query token: {}",
            maxsim(&q, &same)
        );
        let disjoint = token_vectors(&e, "zzz qqq");
        assert!(maxsim(&q, &disjoint).abs() < 1e-4);
        assert_eq!(maxsim(&[], &same), 0.0);
        assert_eq!(maxsim(&q, &[]), 0.0);
        // Late interaction is not bag-of-words cosine: a document that
        // covers each query token somewhere outscores a denser partial.
        let spread = token_vectors(&e, "braces and the parser");
        assert!(maxsim(&q, &spread) > maxsim(&q, &disjoint));
    }
}
