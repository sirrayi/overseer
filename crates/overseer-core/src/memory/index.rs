//! Lexical retrieval over the memory stores (decision record §3): a
//! Unicode tokenizer, BM25F (Robertson & Zaragoza) over four fields,
//! one-hop `[[link]]` expansion, and reciprocal-rank fusion with
//! activation and confidence.
//!
//! The index lives in memory only, built lazily and rebuilt when a store's
//! file fingerprint (paths, sizes, mtimes) changes or an indexed note's
//! validity lapses. Only current notes are indexed; `proposals/` never is.
// DEFERRED(owner): persisted index cache — gate: build >50 ms on a real store

use super::stores::Scope;
use super::{activation, EntryMeta, Layer, CORE_NAME, INDEX_NAME};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

pub const K1: f64 = 1.2;
pub const B: f64 = 0.75;
/// Field boosts, in field order: name/path, cues, index line, body.
pub const BOOST: [f64; 4] = [4.0, 3.0, 2.0, 1.0];
pub const RRF_K: f64 = 60.0;
/// Association strength S of a one-hop link (spreading activation
/// `S − ln(fan)`): a source with more than e² ≈ 7 links spreads nothing.
const ASSOC_S: f64 = 2.0;
/// Top BM25F hits whose links are expanded.
const LINK_SOURCES: usize = 5;

/// Compact English stopword list (sorted: binary-searched).
const STOPWORDS: &[&str] = &[
    "a", "about", "above", "after", "again", "against", "all", "am", "an", "and", "any", "are",
    "as", "at", "be", "because", "been", "before", "being", "below", "between", "both", "but",
    "by", "can", "could", "did", "do", "does", "doing", "down", "during", "each", "few", "for",
    "from", "further", "had", "has", "have", "having", "he", "her", "here", "hers", "him", "his",
    "how", "i", "if", "in", "into", "is", "it", "its", "just", "me", "more", "most", "my", "no",
    "nor", "not", "now", "of", "off", "on", "once", "only", "or", "other", "our", "out", "over",
    "own", "same", "she", "should", "so", "some", "such", "than", "that", "the", "their", "them",
    "then", "there", "these", "they", "this", "those", "through", "to", "too", "under", "until",
    "up", "very", "was", "we", "were", "what", "when", "where", "which", "while", "who", "whom",
    "why", "will", "with", "would", "you", "your",
];

pub fn is_stopword(tok: &str) -> bool {
    STOPWORDS.binary_search(&tok).is_ok()
}

/// Lowercased Unicode alphanumeric runs with their char offsets.
fn spans(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0;
    for (i, c) in text.chars().enumerate() {
        if c.is_alphanumeric() {
            if cur.is_empty() {
                start = i;
            }
            cur.extend(c.to_lowercase());
        } else if !cur.is_empty() {
            out.push((start, std::mem::take(&mut cur)));
        }
    }
    if !cur.is_empty() {
        out.push((start, cur));
    }
    out
}

/// Minimal plural strip: longer than 3 chars, ends in `s` but not `ss`.
fn strip_plural(mut tok: String) -> String {
    if tok.chars().count() > 3 && tok.ends_with('s') && !tok.ends_with("ss") {
        tok.pop();
    }
    tok
}

/// Index terms of `text`, in order, repeats kept: stopwords dropped, then
/// the plural strip.
pub fn terms(text: &str) -> Vec<String> {
    spans(text)
        .into_iter()
        .filter(|(_, t)| !is_stopword(t))
        .map(|(_, t)| strip_plural(t))
        .collect()
}

/// Every token of `text` normalized like [`terms`] but with stopwords
/// kept — for keyword triggers, which match whatever the user named.
pub fn raw_terms(text: &str) -> Vec<String> {
    spans(text)
        .into_iter()
        .map(|(_, t)| strip_plural(t))
        .collect()
}

/// Distinct [`terms`] in first-seen order.
pub fn query_terms(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    terms(text)
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

/// One indexed (current) note.
#[derive(Debug, Clone)]
pub struct Doc {
    pub scope: Scope,
    /// Store-relative path, `/`-separated: `layer/name.md` or `name.md`.
    pub rel: String,
    pub path: PathBuf,
    pub title: String,
    /// The note text after its frontmatter.
    pub body: String,
    pub meta: EntryMeta,
    /// Modification time, epoch seconds.
    pub mtime: u64,
    len: [u32; 4],
    links: Vec<u32>,
}

impl Doc {
    /// `scope:rel` — how every surface names a note.
    pub fn id(&self) -> String {
        format!("{}:{}", self.scope.name(), self.rel)
    }

    pub fn layer(&self) -> Option<Layer> {
        self.rel.split_once('/').and_then(|(l, _)| Layer::parse(l))
    }
}

/// A ranked search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub doc: usize,
    pub bm25: f64,
    /// Distinct query terms the note matched.
    pub matched: usize,
    /// Σ idf of matched terms / Σ idf of all query terms.
    pub coverage: f64,
    /// Fused RRF score.
    pub score: f64,
}

pub struct Index {
    stores: Vec<(Scope, PathBuf)>,
    stamp: u64,
    expires: Option<u64>,
    pub docs: Vec<Doc>,
    postings: HashMap<String, Vec<(u32, [u16; 4])>>,
    avg: [f64; 4],
}

/// Topic files of one store: the root and each layer dir, never
/// `proposals/`. Sorted for a deterministic doc order.
fn topic_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let subdirs = std::iter::once(None).chain(Layer::ALL.iter().map(|l| Some(l.name())));
    for sub in subdirs {
        let d = sub.map_or_else(|| dir.to_path_buf(), |s| dir.join(s));
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_topic = name.ends_with(".md")
                && name.len() > 3
                && e.file_type().is_ok_and(|t| t.is_file())
                && (sub.is_some() || (name != INDEX_NAME && name != CORE_NAME));
            if is_topic {
                let rel = sub.map_or_else(|| name.clone(), |s| format!("{s}/{name}"));
                out.push((rel, e.path()));
            }
        }
    }
    out.sort();
    out
}

fn mtime_of(md: &std::fs::Metadata) -> std::time::Duration {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default()
}

/// Hash over every topic file's path, size and mtime plus each INDEX.md.
fn fingerprint(stores: &[(Scope, PathBuf)]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (_, dir) in stores {
        let files = topic_files(dir).into_iter().map(|(_, p)| p);
        for p in files.chain([dir.join(INDEX_NAME)]) {
            p.hash(&mut h);
            if let Ok(md) = std::fs::metadata(&p) {
                md.len().hash(&mut h);
                mtime_of(&md).hash(&mut h);
            }
        }
    }
    h.finish()
}

impl Index {
    /// Build over `stores` at clock `now` (liveness is evaluated at `now`).
    pub fn build(stores: &[(Scope, PathBuf)], now: u64) -> Index {
        let stamp = fingerprint(stores);
        let mut docs = Vec::new();
        let mut fields: Vec<[String; 4]> = Vec::new();
        let mut expires: Option<u64> = None;
        for (scope, dir) in stores {
            let lines: HashMap<String, String> = super::pointer_lines(dir).into_iter().collect();
            for (rel, path) in topic_files(dir) {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                // Malformed headers fail closed: never served.
                let Ok((meta, body)) = super::parse_meta(&text) else {
                    continue;
                };
                let mtime = std::fs::metadata(&path)
                    .map(|m| mtime_of(&m).as_secs())
                    .unwrap_or(0);
                if !super::current_at(&meta, &text, mtime, now) {
                    continue;
                }
                if let Some(t) = super::expires_at(&meta, mtime) {
                    expires = Some(expires.map_or(t, |e| e.min(t)));
                }
                fields.push([
                    rel.trim_end_matches(".md").to_string(),
                    meta.cues.join(", "),
                    lines.get(&rel).cloned().unwrap_or_default(),
                    body.clone(),
                ]);
                docs.push(Doc {
                    scope: *scope,
                    title: super::title_of(&body).to_string(),
                    rel,
                    path,
                    body,
                    meta,
                    mtime,
                    len: [0; 4],
                    links: Vec::new(),
                });
            }
        }
        let mut postings: HashMap<String, Vec<(u32, [u16; 4])>> = HashMap::new();
        let mut total = [0f64; 4];
        for (i, f) in fields.iter().enumerate() {
            let mut tf: BTreeMap<String, [u16; 4]> = BTreeMap::new();
            for (k, text) in f.iter().enumerate() {
                let ts = terms(text);
                docs[i].len[k] = ts.len() as u32;
                total[k] += ts.len() as f64;
                for t in ts {
                    let e = tf.entry(t).or_default();
                    e[k] = e[k].saturating_add(1);
                }
            }
            for (t, counts) in tf {
                postings.entry(t).or_default().push((i as u32, counts));
            }
        }
        let n = docs.len().max(1) as f64;
        let avg = total.map(|t| t / n);
        let mut index = Index {
            stores: stores.to_vec(),
            stamp,
            expires,
            docs,
            postings,
            avg,
        };
        index.link();
        index
    }

    /// Rebuild when a store changed on disk or an indexed note lapsed.
    pub fn refresh(&mut self, now: u64) {
        let lapsed = self.expires.is_some_and(|e| now >= e);
        if lapsed || fingerprint(&self.stores) != self.stamp {
            *self = Index::build(&self.stores, now);
        }
    }

    /// Resolve every doc's `[[links]]` to doc indices (distinct, not self).
    fn link(&mut self) {
        let mut by_rel: HashMap<(Scope, &str), u32> = HashMap::new();
        let mut by_file: HashMap<(Scope, &str), u32> = HashMap::new();
        for (i, d) in self.docs.iter().enumerate() {
            by_rel.insert((d.scope, d.rel.as_str()), i as u32);
            let file = d.rel.rsplit('/').next().unwrap_or(&d.rel);
            by_file.entry((d.scope, file)).or_insert(i as u32);
        }
        let resolve = |from: Scope, target: &str| -> Option<u32> {
            let (scopes, name) = match target
                .split_once(':')
                .and_then(|(s, n)| Some((Scope::parse(s)?, n)))
            {
                Some((s, n)) => (vec![s], n),
                None => {
                    let other = if from == Scope::User {
                        Scope::Project
                    } else {
                        Scope::User
                    };
                    (vec![from, other], target)
                }
            };
            let name = if name.ends_with(".md") {
                name.to_string()
            } else {
                format!("{name}.md")
            };
            scopes.into_iter().find_map(|s| {
                if name.contains('/') {
                    by_rel.get(&(s, name.as_str())).copied()
                } else {
                    by_file.get(&(s, name.as_str())).copied()
                }
            })
        };
        let links: Vec<Vec<u32>> = self
            .docs
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let mut out: Vec<u32> = Vec::new();
                for t in super::links_of(&d.body) {
                    if let Some(j) = resolve(d.scope, t) {
                        if j != i as u32 && !out.contains(&j) {
                            out.push(j);
                        }
                    }
                }
                out
            })
            .collect();
        for (d, l) in self.docs.iter_mut().zip(links) {
            d.links = l;
        }
    }

    /// `ln(1 + (N − df + 0.5)/(df + 0.5))`.
    pub fn idf(&self, term: &str) -> f64 {
        let n = self.docs.len() as f64;
        let df = self.postings.get(term).map_or(0, Vec::len) as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    /// Find a current note by `scope:rel`, `rel`, `name.md` or `name`.
    /// Unscoped names try the user store first; bare file names match in
    /// any layer.
    pub fn find(&self, name: &str) -> Option<&Doc> {
        let (scope, name) = match name
            .split_once(':')
            .and_then(|(s, n)| Some((Scope::parse(s)?, n)))
        {
            Some((s, n)) => (Some(s), n),
            None => (None, name),
        };
        let name = if name.ends_with(".md") {
            name.to_string()
        } else {
            format!("{name}.md")
        };
        let wanted = |d: &&Doc| scope.is_none_or(|s| s == d.scope);
        self.docs
            .iter()
            .filter(wanted)
            .find(|d| d.rel == name)
            .or_else(|| {
                self.docs.iter().filter(wanted).find(|d| {
                    !name.contains('/') && d.rel.rsplit('/').next() == Some(name.as_str())
                })
            })
    }

    /// BM25F over `query`: `(doc, score, matched terms, Σ idf matched)`.
    fn bm25f(&self, query: &[String]) -> HashMap<u32, (f64, usize, f64)> {
        let mut acc: HashMap<u32, (f64, usize, f64)> = HashMap::new();
        for t in query {
            let Some(list) = self.postings.get(t) else {
                continue;
            };
            let idf = self.idf(t);
            for (d, tf) in list {
                let len = self.docs[*d as usize].len;
                let w: f64 = (0..4)
                    .filter(|&f| tf[f] > 0)
                    .map(|f| {
                        let norm = 1.0 - B + B * f64::from(len[f]) / self.avg[f];
                        BOOST[f] * f64::from(tf[f]) / norm
                    })
                    .sum();
                let e = acc.entry(*d).or_default();
                e.0 += idf * w / (K1 + w);
                e.1 += 1;
                e.2 += idf;
            }
        }
        acc
    }

    /// Ranked hits for `query` at clock `now`, best first: the BM25F
    /// matches plus the one-hop links of the top hits, fused by RRF over
    /// lexical (or association) rank, activation and confidence.
    pub fn search(&self, query: &str, now: u64) -> Vec<Hit> {
        let q = query_terms(query);
        let idf_total: f64 = q.iter().map(|t| self.idf(t)).sum();
        let lex = self.bm25f(&q);
        let id = |d: u32| self.docs[d as usize].id();
        let mut lexical: Vec<(u32, f64)> = lex.iter().map(|(d, v)| (*d, v.0)).collect();
        lexical.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| id(a.0).cmp(&id(b.0))));
        let mut assoc: HashMap<u32, f64> = HashMap::new();
        for (src, _) in lexical.iter().take(LINK_SOURCES) {
            let links = &self.docs[*src as usize].links;
            let a = ASSOC_S - (links.len() as f64).ln();
            if a <= 0.0 {
                continue;
            }
            for t in links.iter().filter(|t| !lex.contains_key(t)) {
                let e = assoc.entry(*t).or_insert(a);
                *e = e.max(a);
            }
        }
        let mut linked: Vec<(u32, f64)> = assoc.into_iter().collect();
        linked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| id(a.0).cmp(&id(b.0))));

        let uses: HashMap<Scope, BTreeMap<String, activation::Uses>> = self
            .stores
            .iter()
            .map(|(s, dir)| (*s, activation::load(dir)))
            .collect();
        let act = |d: u32| {
            let doc = &self.docs[d as usize];
            let u = uses
                .get(&doc.scope)
                .and_then(|m| m.get(&doc.rel))
                .cloned()
                .unwrap_or_else(|| activation::Uses::once(doc.mtime));
            activation::base_level(&u, now)
        };
        let cands: Vec<u32> = lexical.iter().chain(&linked).map(|(d, _)| *d).collect();
        let mut by_act: Vec<(u32, f64)> = cands.iter().map(|&d| (d, act(d))).collect();
        by_act.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| id(a.0).cmp(&id(b.0))));
        let mut by_conf: Vec<(u32, f64)> = cands
            .iter()
            .map(|&d| (d, self.docs[d as usize].meta.confidence))
            .collect();
        by_conf.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| id(a.0).cmp(&id(b.0))));

        // Link-only candidates rank on the lexical axis by association,
        // after every direct match.
        let mut fused: HashMap<u32, f64> = HashMap::new();
        for (list, offset) in [
            (&lexical, 0),
            (&linked, lexical.len()),
            (&by_act, 0),
            (&by_conf, 0),
        ] {
            for (d, rank) in competition_ranks(list, offset) {
                *fused.entry(d).or_default() += 1.0 / (RRF_K + rank as f64);
            }
        }
        let mut hits: Vec<Hit> = cands
            .iter()
            .map(|&d| {
                let (bm25, matched, idf_hit) = lex.get(&d).copied().unwrap_or_default();
                Hit {
                    doc: d as usize,
                    bm25,
                    matched,
                    coverage: if idf_total > 0.0 {
                        idf_hit / idf_total
                    } else {
                        0.0
                    },
                    score: fused[&d],
                }
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| self.docs[a.doc].id().cmp(&self.docs[b.doc].id()))
        });
        hits
    }
}

/// 1-based ranks of a list sorted by value (desc), from `offset + 1`;
/// equal values share the better rank, so a tie on one axis (two unused
/// notes, equal confidence) never decides the fused order by name.
fn competition_ranks(list: &[(u32, f64)], offset: usize) -> Vec<(u32, usize)> {
    let mut out = Vec::with_capacity(list.len());
    let mut rank = offset;
    for (i, (d, v)) in list.iter().enumerate() {
        if i == 0 || *v != list[i - 1].1 {
            rank = offset + i + 1;
        }
        out.push((*d, rank));
    }
    out
}

/// The body line best matching `query` (most distinct query terms, first
/// wins), windowed to at most `cap` chars around its first match. With no
/// match: the first non-empty line after the title, else the title.
pub fn snippet(doc: &Doc, query: &str, cap: usize) -> String {
    let q: HashSet<String> = query_terms(query).into_iter().collect();
    let mut best: Option<(usize, usize, &str)> = None;
    for line in doc.body.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let found: Vec<(usize, String)> = spans(line)
            .into_iter()
            .filter(|(_, t)| !is_stopword(t))
            .map(|(i, t)| (i, strip_plural(t)))
            .filter(|(_, t)| q.contains(t))
            .collect();
        let distinct = found.iter().map(|(_, t)| t).collect::<HashSet<_>>().len();
        if distinct > best.map_or(0, |b| b.0) {
            best = Some((distinct, found[0].0, line));
        }
    }
    let (start, line) = match best {
        Some((_, at, line)) => (at, line),
        None => {
            let mut lines = doc.body.lines().map(str::trim).filter(|l| !l.is_empty());
            let first = lines.next().unwrap_or("");
            let pick = if first.starts_with("# ") {
                lines.next().unwrap_or(first)
            } else {
                first
            };
            (0, pick)
        }
    };
    let chars = line.chars().count();
    if chars <= cap {
        return line.to_string();
    }
    let from = start.saturating_sub(cap / 4).min(chars - cap);
    line.chars().skip(from).take(cap).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ov-index-{tag}-{}", uuid::Uuid::now_v7()));
        crate::memory::ensure(&d).unwrap();
        d
    }

    fn note(dir: &Path, rel: &str, text: &str) {
        std::fs::write(dir.join(rel), text).unwrap();
    }

    const NOW: u64 = 1_900_000_000;

    #[test]
    fn tokenizer_lowercases_drops_stopwords_and_strips_plurals() {
        assert!(
            STOPWORDS.windows(2).all(|w| w[0] < w[1]),
            "sorted and distinct"
        );
        assert!((90..=130).contains(&STOPWORDS.len()));
        assert_eq!(
            terms("The Deploys of ÉCOLE: glass buses, is it tests? cats's"),
            ["deploy", "école", "glass", "buse", "test", "cat", "s"]
        );
        // ≤3 chars keeps its s; `ss` keeps it too.
        assert_eq!(terms("gas bus boss"), ["gas", "bus", "boss"]);
        assert_eq!(raw_terms("The notes"), ["the", "note"]);
        assert_eq!(query_terms("deploy deploys Deploy"), ["deploy"]);
    }

    /// Three-doc corpus, body field only (names/cues/index lines carry no
    /// query term), scored by hand:
    /// N = 3, df(apple) = 2 → idf = ln(1 + 1.5/2.5) = ln 1.6.
    /// Body lengths 2, 4, 1 (names contribute separately) → avg 7/3.
    #[test]
    fn bm25f_matches_a_hand_computed_corpus() {
        let dir = store("bm25");
        note(&dir, "semantic/d1.md", "apple apple\n");
        note(&dir, "semantic/d2.md", "apple pear pear pear\n");
        note(&dir, "semantic/d3.md", "pear\n");
        let idx = Index::build(&[(Scope::Project, dir)], NOW);
        let avg = 7.0 / 3.0;
        let w = |tf: f64, len: f64| tf / (1.0 - B + B * len / avg);
        let idf = 1.6f64.ln();
        let s1 = idf * w(2.0, 2.0) / (K1 + w(2.0, 2.0));
        let s2 = idf * w(1.0, 4.0) / (K1 + w(1.0, 4.0));
        assert!((idx.idf("apple") - idf).abs() < 1e-12);
        let lex = idx.bm25f(&["apple".to_string()]);
        let score = |rel: &str| {
            let d = idx.docs.iter().position(|d| d.rel == rel).unwrap() as u32;
            lex.get(&d).map(|v| v.0)
        };
        assert!((score("semantic/d1.md").unwrap() - s1).abs() < 1e-12);
        assert!((score("semantic/d2.md").unwrap() - s2).abs() < 1e-12);
        assert_eq!(score("semantic/d3.md"), None);
    }

    #[test]
    fn name_and_cue_boosts_outrank_body_mentions() {
        let dir = store("boost");
        note(&dir, "semantic/deploy.md", "# Shipping\nsteps\n");
        note(
            &dir,
            "semantic/other.md",
            "---\ncues: rollout\n---\n# Other\nnothing\n",
        );
        note(
            &dir,
            "semantic/body.md",
            "# Body\nwe deploy and rollout here\n",
        );
        let idx = Index::build(&[(Scope::Project, dir)], NOW);
        let hits = idx.search("deploy", NOW);
        assert_eq!(idx.docs[hits[0].doc].rel, "semantic/deploy.md");
        let hits = idx.search("rollout", NOW);
        assert_eq!(idx.docs[hits[0].doc].rel, "semantic/other.md");
    }

    /// RRF: a doc second on BM25F but first on activation and confidence
    /// beats the BM25F leader; absent lists add nothing; ties by name.
    #[test]
    fn rrf_fuses_lexical_activation_and_confidence() {
        let dir = store("rrf");
        note(
            &dir,
            "semantic/a.md",
            "---\nconfidence: 0.1\n---\nkiwi kiwi kiwi\n",
        );
        note(
            &dir,
            "semantic/b.md",
            "---\nconfidence: 0.9\n---\nkiwi and more words here\n",
        );
        activation::record(&dir, "semantic/b.md", NOW).unwrap();
        activation::record(&dir, "semantic/b.md", NOW).unwrap();
        let idx = Index::build(&[(Scope::Project, dir.clone())], NOW);
        let hits = idx.search("kiwi", NOW);
        let rels: Vec<&str> = hits.iter().map(|h| idx.docs[h.doc].rel.as_str()).collect();
        assert_eq!(rels, ["semantic/b.md", "semantic/a.md"]);
        assert!(hits[1].bm25 > hits[0].bm25, "a leads on BM25F alone");
        let r = |k: usize| 1.0 / (RRF_K + k as f64);
        assert!((hits[0].score - (r(2) + r(1) + r(1))).abs() < 1e-12);
        assert!((hits[1].score - (r(1) + r(2) + r(2))).abs() < 1e-12);

        // Exact ties (same text, same mtime, no uses) break by name.
        let dir = store("tie");
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(NOW - 86_400);
        for rel in ["semantic/y.md", "semantic/x.md"] {
            note(&dir, rel, "plum\n");
            let f = std::fs::File::options()
                .write(true)
                .open(dir.join(rel))
                .unwrap();
            f.set_modified(when).unwrap();
        }
        let t = Index::build(&[(Scope::Project, dir)], NOW);
        let hits = t.search("plum", NOW);
        assert_eq!(hits[0].score, hits[1].score);
        assert_eq!(t.docs[hits[0].doc].rel, "semantic/x.md");
    }

    #[test]
    fn links_expand_one_hop_with_a_fan_penalty() {
        let dir = store("links");
        note(
            &dir,
            "semantic/hub.md",
            "# Hub\nmango notes [[target]] [[project:semantic/far.md]]\n",
        );
        note(&dir, "semantic/target.md", "# Target\nunrelated\n");
        note(&dir, "semantic/far.md", "# Far\n[[deep]]\n");
        note(&dir, "semantic/deep.md", "# Deep\ntwo hops away\n");
        let links: String = (0..8).map(|i| format!("[[f{i}]] ")).collect();
        note(&dir, "semantic/fan.md", &format!("# Fan\nguava {links}\n"));
        for i in 0..8 {
            note(&dir, &format!("semantic/f{i}.md"), "# F\nleaf\n");
        }
        let idx = Index::build(&[(Scope::Project, dir)], NOW);
        let rels = |q: &str| -> Vec<String> {
            idx.search(q, NOW)
                .iter()
                .map(|h| idx.docs[h.doc].rel.clone())
                .collect()
        };
        let got = rels("mango");
        assert_eq!(got[0], "semantic/hub.md");
        assert!(got.contains(&"semantic/target.md".to_string()));
        assert!(got.contains(&"semantic/far.md".to_string()));
        assert!(
            !got.contains(&"semantic/deep.md".to_string()),
            "one hop only"
        );
        // fan 2: association 2 − ln 2 > 0; the link-only hit has no bm25.
        let hit = idx
            .search("mango", NOW)
            .into_iter()
            .find(|h| idx.docs[h.doc].rel == "semantic/target.md")
            .unwrap();
        assert_eq!((hit.bm25, hit.matched), (0.0, 0));
        // fan 8: 2 − ln 8 < 0 → nothing spreads.
        assert_eq!(rels("guava"), ["semantic/fan.md"]);
    }

    #[test]
    fn index_holds_current_notes_only_and_refreshes() {
        let dir = store("live");
        note(
            &dir,
            "semantic/old.md",
            "---\nvalid_to: 2000-01-01T00:00:00Z\n---\nfig\n",
        );
        note(&dir, "semantic/bad.md", "---\nconfidence: 9\n---\nfig\n");
        note(&dir, "semantic/gone.md", "fig\nsuperseded_by new.md\n");
        std::fs::create_dir_all(dir.join("proposals")).unwrap();
        note(&dir, "proposals/p.md", "fig\n");
        note(
            &dir,
            "semantic/soon.md",
            "---\nvalid_to: 2031-01-01T00:00:00Z\n---\nfig\n",
        );
        let stores = [(Scope::Project, dir.clone())];
        let mut idx = Index::build(&stores, NOW);
        let rels: Vec<&str> = idx.docs.iter().map(|d| d.rel.as_str()).collect();
        assert_eq!(rels, ["semantic/soon.md"]);
        note(&dir, "semantic/new.md", "# New\nfig\n");
        idx.refresh(NOW);
        assert_eq!(idx.docs.len(), 2);
        // Lapsed validity drops a note without any file change.
        idx.refresh(super::super::rfc3339_epoch("2031-01-01T00:00:01Z").unwrap());
        let rels: Vec<&str> = idx.docs.iter().map(|d| d.rel.as_str()).collect();
        assert_eq!(rels, ["semantic/new.md"]);
    }

    #[test]
    fn find_and_snippet() {
        let dir = store("find");
        note(
            &dir,
            "semantic/deploy.md",
            "# Deploy\nfirst line\nuse the blue green deploy switch\n",
        );
        let idx = Index::build(&[(Scope::Project, dir)], NOW);
        for n in [
            "project:semantic/deploy.md",
            "semantic/deploy.md",
            "deploy.md",
            "deploy",
        ] {
            assert_eq!(
                idx.find(n).map(|d| d.rel.as_str()),
                Some("semantic/deploy.md"),
                "{n}"
            );
        }
        assert!(idx.find("user:deploy").is_none());
        let d = idx.find("deploy").unwrap();
        assert_eq!(
            snippet(d, "blue switch", 200),
            "use the blue green deploy switch"
        );
        assert_eq!(snippet(d, "nothing", 200), "first line");
        assert_eq!(snippet(d, "switch", 10).chars().count(), 10);
        assert!(snippet(d, "switch", 10).contains("switch"));
    }

    /// Release-mode timings: `cargo test --release -p overseer-core
    /// memory_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn memory_bench() {
        let empty = store("resident0");
        let resident =
            |dir: &Path| crate::memory::resident(&[(Scope::Project, dir.to_path_buf())], NOW).len();
        println!("resident empty store: {} bytes", resident(&empty));
        let two = store("resident200");
        let mut index = String::new();
        for i in 0..200 {
            note(
                &two,
                &format!("semantic/note-{i}.md"),
                &format!("# Note {i}\nbody {i}\n"),
            );
            index.push_str(&format!(
                "semantic/note-{i}.md — Deploy runbook step {i} for the staging cluster\n"
            ));
        }
        std::fs::write(two.join(INDEX_NAME), index).unwrap();
        println!("resident 200-note store: {} bytes", resident(&two));
        let words: Vec<String> = (0..4_000).map(|i| format!("w{i:x}q")).collect();
        for n in [500usize, 5_000, 20_000] {
            let dir = store(&format!("bench{n}"));
            let mut index = String::new();
            let mut seed = 0x9e37_79b9_7f4a_7c15u64;
            for i in 0..n {
                let mut body = format!("# Note {i}\n");
                while body.len() < 1_000 {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    body.push_str(&words[(seed % words.len() as u64) as usize]);
                    body.push(' ');
                }
                note(
                    &dir,
                    &format!("semantic/n{i}.md"),
                    &format!("---\ncues: c{}\n---\n{body}\n", i % 97),
                );
                index.push_str(&format!("semantic/n{i}.md — note {i}\n"));
            }
            std::fs::write(dir.join(INDEX_NAME), index).unwrap();
            let stores = [(Scope::Project, dir)];
            let t = std::time::Instant::now();
            let idx = Index::build(&stores, NOW);
            let build = t.elapsed();
            let t = std::time::Instant::now();
            let hits = idx.search("w1aq w2bq c5 note", NOW);
            let search = t.elapsed();
            println!(
                "bench n={n:>6}: build {:>8.1} ms, search {:>7.2} ms ({} hits)",
                build.as_secs_f64() * 1e3,
                search.as_secs_f64() * 1e3,
                hits.len()
            );
        }
    }
}
