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
//! - **ragflow `chunk_text`** — the delimiter hierarchy (headings →
//!   paragraphs → sentences → char window) with section-local overlap and
//!   exact byte spans, so a chunk can be quoted and re-read.
//!
//! `HashingEmbedder` is the deterministic stand-in used by tests and by
//! any offline path: a hashed bag of tokens, L2-normalized. It is a
//! *lexical* embedding, stated as such — it makes ranking testable, not
//! semantic retrieval possible.
//! `// DEFERRED(owner): real embedding/reranking backends, a persistent
//! vector index, and hierarchical (parent/child) chunk linking — this batch
//! lands the interfaces, the chunker, and the scoring math only.`
// DEFERRED(owner): ragflow's PDF/DOCX layout parsers, table extraction, and
// tokenizer-based budgets (this port takes a plain text and a char budget) —
// the delivery gate forbids new dependencies, and every layout parser needs
// one.

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
    pack(
        &hard_split(paragraph_spans(text), text, max_chars),
        text,
        max_chars,
        overlap,
    )
}

/// Pack pre-cut pieces into chunks: greedy up to `max_chars`, with the tail
/// of the previous chunk repeated as `overlap` chars of context. The pieces
/// arrive in order and gapless, and every piece is already at most
/// `max_chars`, so the cap holds for every produced chunk and progress is
/// guaranteed (`widened > prev.start`). One call packs one section — a
/// caller that wants overlap to stop at a boundary calls `pack` per section.
fn pack(pieces: &[(usize, usize)], text: &str, max_chars: usize, overlap: usize) -> Vec<Chunk> {
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
        while j < pieces.len() && char_span_len(text, first, pieces[j].1) <= budget {
            end = pieces[j].1;
            j += 1;
        }
        let mut start = first;
        if let Some(prev) = chunks.last() {
            // Only re-read as much context as the cap still allows: a chunk
            // that is already at the limit takes no overlap (the cap wins
            // over context — a chunk longer than `max_chars` breaks both
            // the promise and the downstream embedding budget).
            let room = max_chars.saturating_sub(char_span_len(text, first, end));
            let back = overlap.min(room);
            if back > 0 {
                let widened = back_char_boundary(text, first, back);
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

/// Cut spans into at most `max`-char windows at char boundaries. Char
/// counts, never bytes: a multibyte char is never split and a CJK run still
/// spends its whole budget instead of being cut at a byte count.
fn hard_split(spans: Vec<(usize, usize)>, text: &str, max: usize) -> Vec<(usize, usize)> {
    // A zero window would never advance; both callers guarantee `max >= 1`.
    debug_assert!(max >= 1, "hard_split: zero window");
    let mut out = Vec::new();
    for (s, e) in spans {
        let mut cur = s;
        loop {
            let cut = char_window_end(text, cur, max);
            if cut >= e {
                if e > cur {
                    out.push((cur, e));
                }
                break;
            }
            out.push((cur, cut));
            cur = cut;
        }
    }
    out
}

/// Byte offset of the first char *after* the `max`-char window starting at
/// `start` (`text.len()` when fewer than `max` chars remain).
fn char_window_end(text: &str, start: usize, max: usize) -> usize {
    text[start..]
        .char_indices()
        .nth(max)
        .map_or(text.len(), |(off, _)| start + off)
}

/// Byte offset of the char `back` chars before `idx` (0 when `idx` is fewer
/// than `back` chars in) — the overlap window is counted in chars, like
/// every other budget here.
fn back_char_boundary(text: &str, idx: usize, back: usize) -> usize {
    if back == 0 {
        return idx;
    }
    text[..idx]
        .char_indices()
        .rev()
        .nth(back - 1)
        .map_or(0, |(off, _)| off)
}

/// Char length of `text[a..b]`: the packer measures in the unit it promises
/// (`max_chars` is chars, so a byte length would under-fill every CJK chunk).
fn char_span_len(text: &str, a: usize, b: usize) -> usize {
    text[a..b].chars().count()
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

/// Budget knobs for `chunk_text` (ragflow's per-chunk character budget and
/// repeated context), as one value so a caller cannot pass them swapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkOpts {
    /// Hard ceiling on one chunk, in chars (never bytes).
    pub max_chars: usize,
    /// Chars of the previous chunk repeated at the head of the next one,
    /// inside the same section only.
    pub overlap: usize,
}

/// Split `text` down ragflow's delimiter hierarchy: markdown headings open
/// sections, blank lines open paragraphs inside a section, sentence marks
/// split an over-long paragraph, and only a run with no delimiter at all is
/// cut at a char window. Chunk spans are absolute byte offsets into `text`,
/// so every chunk is quotable with `read path:lo-hi`.
///
/// Invariants (asserted by the tests):
/// - a chunk never spans two headings — a heading line starts a new chunk
///   even when the previous one is short;
/// - every `chunk.text == &text[chunk.start..chunk.end]`;
/// - every chunk is at most `ChunkOpts::max_chars` chars (char counts, never
///   byte offsets — a multibyte char is never split);
/// - spans increase monotonically and cover the section's text in order
///   (overlap repeats text, it never skips);
/// - `overlap` is applied only inside a section, so context never leaks
///   across a heading boundary.
///
/// A zero budget and an overlap that cannot make progress are errors rather
/// than silent repair: either one hands back an empty or non-advancing
/// result, and only the caller knows the downstream embedding budget.
pub fn chunk_text(text: &str, opts: &ChunkOpts) -> Result<Vec<Chunk>, String> {
    if opts.max_chars == 0 {
        return Err(
            "rag: chunk_text: max_chars 0 would cut every chunk to nothing — pass \
             max_chars >= 1"
                .into(),
        );
    }
    if opts.overlap >= opts.max_chars {
        return Err(format!(
            "rag: chunk_text: overlap {} must be smaller than max_chars {} — an overlap \
             that large cannot make progress",
            opts.overlap, opts.max_chars
        ));
    }
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let mut chunks: Vec<Chunk> = Vec::new();
    for (start, end) in section_spans(text) {
        let section = &text[start..end];
        let pieces: Vec<(usize, usize)> = sentence_pieces(section, opts.max_chars)
            .into_iter()
            .map(|(s, e)| (s + start, e + start))
            .collect();
        // One `pack` per section: its first chunk gets no overlap, which is
        // exactly "overlap never crosses a heading boundary".
        chunks.extend(pack(&pieces, text, opts.max_chars, opts.overlap));
    }
    Ok(chunks)
}

/// Section spans, cut at ATX heading lines. Gapless and covering the whole
/// text: the head before the first heading is a section of its own, so no
/// byte is left outside a chunk.
fn section_spans(text: &str) -> Vec<(usize, usize)> {
    let mut starts: Vec<usize> = vec![0];
    let mut off = 0usize;
    for line in text.split_inclusive('\n') {
        // `off > 0`: a heading on the first line opens the section that
        // already starts at 0.
        if off > 0 && is_heading(line) {
            starts.push(off);
        }
        off += line.len();
    }
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(starts.len());
    for (i, s) in starts.iter().enumerate() {
        let e = starts.get(i + 1).copied().unwrap_or(text.len());
        if e > *s {
            out.push((*s, e));
        }
    }
    out
}

/// True when a line is an ATX heading: one to six `#` at the line start,
/// followed by a space/tab or the line end. An indented `#` is body text —
/// the port reads "at line start" literally, so a quoted `  # title` never
/// becomes a chunk boundary.
fn is_heading(line: &str) -> bool {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return false;
    }
    match line[hashes..].chars().next() {
        None => true, // a bare `#` line is still a heading
        Some(c) => c == ' ' || c == '\t',
    }
}

/// The hierarchy below a section: paragraphs, then sentences inside an
/// over-long paragraph, then a hard char window for a run with no delimiter.
/// Spans are section-relative, gapless, and each is at most `max` chars.
fn sentence_pieces(section: &str, max: usize) -> Vec<(usize, usize)> {
    let paragraphs = paragraph_spans(section);
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    // `paragraph_spans` starts at the first non-blank line, so a leading
    // blank run (only the pre-heading section can have one) is covered
    // explicitly: the pieces must stay gapless.
    let head = paragraphs.first().map_or(section.len(), |(s, _)| *s);
    if head > 0 {
        // The head is whitespace and can still be longer than the budget.
        pieces.extend(hard_split(vec![(0, head)], section, max));
    }
    for (ps, pe) in paragraphs {
        let paragraph = &section[ps..pe];
        for (s, e) in hard_split(sentence_spans(paragraph), paragraph, max) {
            pieces.push((s + ps, e + ps));
        }
    }
    pieces
}

/// Sentence spans inside one paragraph: cut after `。`, after `. `/`! `/`? `
/// (the space run after the mark belongs to the sentence that ends, so the
/// next piece starts on a word), and after every line end. Gapless and
/// covering the paragraph — a boundary moves, it never drops text.
fn sentence_spans(paragraph: &str) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < paragraph.len() {
        // One char at a time: every offset stays on a char boundary.
        let Some(c) = paragraph[i..].chars().next() else {
            break;
        };
        let after = i + c.len_utf8();
        let (cut, swallow) = match c {
            '。' => (true, false),
            '.' | '!' | '?' => (paragraph[after..].starts_with(' '), true),
            '\n' => (true, false),
            _ => (false, false),
        };
        if cut {
            let mut end = after;
            if swallow {
                while let Some(c) = paragraph[end..].chars().next() {
                    if c == ' ' || c == '\t' {
                        end += c.len_utf8();
                    } else {
                        break;
                    }
                }
            }
            out.push((start, end));
            start = end;
        }
        i = after;
    }
    if start < paragraph.len() {
        out.push((start, paragraph.len()));
    }
    out
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

    #[test]
    fn chunk_text_starts_a_new_chunk_at_every_heading() {
        let text = "# One\n\nshort\n## Two\n\nbody two\n### Three\n";
        let chunks = chunk_text(
            text,
            &ChunkOpts {
                max_chars: 10,
                overlap: 4,
            },
        )
        .unwrap();
        // A chunk carries at most one heading line, and only as its first
        // line: no chunk straddles two sections.
        for c in &chunks {
            let heads = c.text.lines().filter(|l| is_heading(l)).count();
            assert!(heads <= 1, "chunk straddles a heading: {c:?}");
            if heads == 1 {
                assert!(
                    is_heading(c.text.lines().next().unwrap_or("")),
                    "a heading opens its chunk: {c:?}"
                );
            }
        }
        // Every heading line — even one following a short section — starts a
        // chunk of its own.
        for heading in ["# One", "## Two", "### Three"] {
            let at = text.find(heading).unwrap();
            assert!(
                chunks.iter().any(|c| c.start == at),
                "`{heading}` must start its own chunk: {chunks:?}"
            );
        }
        // Coverage across section boundaries: no hole at a heading, and the
        // document is covered end to end.
        assert_eq!(chunks[0].start, 0);
        assert_eq!(chunks.last().unwrap().end, text.len());
        for w in chunks.windows(2) {
            assert!(w[1].start <= w[0].end, "gap between chunks: {w:?}");
        }
    }

    #[test]
    fn chunk_text_offsets_are_exact_bounded_and_monotonic() {
        let text = doc();
        for opts in [
            ChunkOpts {
                max_chars: 40,
                overlap: 0,
            },
            ChunkOpts {
                max_chars: 120,
                overlap: 20,
            },
            ChunkOpts {
                max_chars: 1,
                overlap: 0,
            },
            ChunkOpts {
                max_chars: 10_000,
                overlap: 7,
            },
        ] {
            let chunks = chunk_text(&text, &opts).unwrap();
            assert!(!chunks.is_empty(), "{opts:?} produced nothing");
            for c in &chunks {
                assert_eq!(c.text, text[c.start..c.end], "span/text mismatch");
                assert!(
                    c.text.chars().count() <= opts.max_chars,
                    "chunk of {} chars exceeds {}",
                    c.text.chars().count(),
                    opts.max_chars
                );
                assert!(c.end > c.start, "empty chunk span");
            }
            for w in chunks.windows(2) {
                assert!(w[0].start < w[1].start, "must progress: {w:?}");
                assert!(w[0].end <= w[1].end);
            }
            assert_eq!(chunks[0].start, 0, "the first chunk starts at the head");
            assert_eq!(
                chunks.last().unwrap().end,
                text.len(),
                "the last chunk reaches the end"
            );
        }
        assert!(
            chunk_text(
                "",
                &ChunkOpts {
                    max_chars: 10,
                    overlap: 2
                }
            )
            .unwrap()
            .is_empty(),
            "empty text → no chunks"
        );
    }

    #[test]
    fn chunk_text_hard_splits_a_punctuationless_paragraph_at_char_boundaries() {
        // Two-byte chars and no delimiter anywhere: only the char window can
        // cut this, and it must cut between chars, never through one.
        let text = "å".repeat(25);
        let chunks = chunk_text(
            &text,
            &ChunkOpts {
                max_chars: 10,
                overlap: 0,
            },
        )
        .unwrap();
        assert_eq!(chunks.len(), 3);
        for c in &chunks {
            assert!(text.is_char_boundary(c.start) && text.is_char_boundary(c.end));
            assert_eq!(c.text.chars().count(), c.text.len() / 2);
        }
        // No overlap → the windows tile the paragraph exactly.
        for w in chunks.windows(2) {
            assert_eq!(w[0].end, w[1].start, "gapless");
        }
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, text);
    }

    #[test]
    fn chunk_text_keeps_cjk_content_valid() {
        let text = "最初の文。次の文です。三つ目の文。";
        let chunks = chunk_text(
            text,
            &ChunkOpts {
                max_chars: 6,
                overlap: 0,
            },
        )
        .unwrap();
        assert_eq!(chunks.len(), 3, "one chunk per sentence: {chunks:?}");
        for c in &chunks {
            // Slicing a &str at a non-boundary would panic; the slices below
            // are the proof, the char count is the budget check.
            assert_eq!(c.text, text[c.start..c.end]);
            assert!(c.text.chars().count() <= 6);
        }
        // The CJK full stop survives: it is a boundary, not a separator that
        // gets dropped.
        assert_eq!(
            chunks
                .iter()
                .map(|c| c.text.matches('。').count())
                .sum::<usize>(),
            3
        );
        let joined: String = chunks.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined, text, "no multibyte char was lost or doubled");
        assert_eq!(chunks[0].text, "最初の文。");
    }

    #[test]
    fn chunk_text_rejects_a_budget_or_overlap_that_cannot_progress() {
        let e = chunk_text(
            "x",
            &ChunkOpts {
                max_chars: 0,
                overlap: 0,
            },
        )
        .unwrap_err();
        assert!(e.contains("max_chars 0"), "{e}");
        assert!(e.contains("pass max_chars >= 1"), "{e}");
        for overlap in [8usize, 9] {
            let e = chunk_text(
                "x",
                &ChunkOpts {
                    max_chars: 8,
                    overlap,
                },
            )
            .unwrap_err();
            assert!(e.contains("smaller than max_chars"), "{e}");
            assert!(e.contains("cannot make progress"), "{e}");
        }
        // One below the cap is still legal.
        assert!(chunk_text(
            "x",
            &ChunkOpts {
                max_chars: 8,
                overlap: 7
            }
        )
        .is_ok());
    }

    #[test]
    fn chunk_text_applies_overlap_only_within_a_section() {
        let text = "# One\n\nAAAA BBBB CCCC DDDD\n\n## Two\n\nEEEE FFFF GGGG HHHH\n";
        let chunks = chunk_text(
            text,
            &ChunkOpts {
                max_chars: 12,
                overlap: 6,
            },
        )
        .unwrap();
        let two = text.find("## Two").unwrap();
        let second = chunks
            .iter()
            .find(|c| c.start == two)
            .unwrap_or_else(|| panic!("the second heading must open a chunk: {chunks:?}"));
        assert!(
            !second.text.contains("AAAA"),
            "the second section re-read the first: {second:?}"
        );
        for c in &chunks {
            assert!(
                c.start <= two || !c.text.contains("# One"),
                "a chunk re-read past the heading boundary: {c:?}"
            );
        }
        // Overlap is still applied where it belongs — inside a section.
        assert!(
            chunks.windows(2).any(|w| w[1].start < w[0].end),
            "no chunk repeated context inside its section: {chunks:?}"
        );
    }

    #[test]
    fn chunk_text_prefers_sentence_boundaries_over_a_char_window() {
        let text = "Alpha one. Beta two. Gamma three.";
        let chunks = chunk_text(
            text,
            &ChunkOpts {
                max_chars: 12,
                overlap: 0,
            },
        )
        .unwrap();
        assert_eq!(
            chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
            ["Alpha one. ", "Beta two. ", "Gamma three."],
            "a char window would have cut mid-sentence"
        );
        // The same paragraph with the budget wide enough stays whole.
        let whole = chunk_text(
            text,
            &ChunkOpts {
                max_chars: 100,
                overlap: 0,
            },
        )
        .unwrap();
        assert_eq!(whole.len(), 1);
        assert_eq!(whole[0].text, text);
    }
}
