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
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    // Hot in the build (every token): a stopword is at most 8 bytes, so it
    // packs into one nonzero word, looked up in a small open-addressed
    // table — no string compares, no binary-search branches.
    static TABLE: std::sync::OnceLock<[u64; STOP_SLOTS]> = std::sync::OnceLock::new();
    if tok.is_empty() || tok.len() > STOPWORD_MAX {
        return false;
    }
    let table = TABLE.get_or_init(|| {
        let mut t = [0u64; STOP_SLOTS];
        for w in STOPWORDS {
            let k = word_key(w);
            let mut i = stop_slot(k);
            while t[i] != 0 {
                i = (i + 1) % STOP_SLOTS;
            }
            t[i] = k;
        }
        t
    });
    let k = word_key(tok);
    let mut i = stop_slot(k);
    loop {
        match table[i] {
            0 => return false,
            x if x == k => return true,
            _ => i = (i + 1) % STOP_SLOTS,
        }
    }
}

/// Longest entry of [`STOPWORDS`].
const STOPWORD_MAX: usize = 7;
const STOP_SLOTS: usize = 512;

fn word_key(w: &str) -> u64 {
    let mut b = [0u8; 8];
    for (d, s) in b.iter_mut().zip(w.bytes()) {
        *d = s;
    }
    u64::from_le_bytes(b)
}

fn stop_slot(k: u64) -> usize {
    (k.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 55) as usize
}

/// FxHash-style hasher for the build's hot maps (term → id, id → tf):
/// a note corpus is local, so SipHash's flood resistance buys nothing
/// while costing the build most of its hashing time.
#[derive(Default, Clone, Copy)]
struct Fx(u64);

impl Fx {
    /// Folded multiply: the high half of the product folds back into the
    /// low bits, which a plain multiply leaves depending only on the
    /// input's low (first) bytes.
    fn add(&mut self, w: u64) {
        let x = u128::from(self.0 ^ w) * 0x517c_c1b7_2722_0a95;
        self.0 = (x as u64) ^ ((x >> 64) as u64);
    }
}

impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            self.add(u64::from_le_bytes(*c));
        }
        if !rest.is_empty() {
            let mut b = [0u8; 8];
            b[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(b));
        }
    }
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
}

type FxMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<Fx>>;

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

/// The lowercased alphanumeric runs of `text` (as [`spans`], without
/// offsets), each handed to `f` as a slice: of `text` itself for an
/// already-lowercase ASCII run, else of `scratch`, which is reused — only
/// non-ASCII lowercasing allocates, and only when `scratch` must grow.
fn each_token(text: &str, scratch: &mut String, mut f: impl FnMut(&str)) {
    let bytes = text.as_bytes();
    let class = |i: usize| CLASS[usize::from(bytes[i])];
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && class(i) == SEP {
            i += 1;
        }
        let (start, mut ascii, mut upper) = (i, true, false);
        while i < bytes.len() {
            while i < bytes.len() && class(i) == LOWER {
                i += 1;
            }
            if i == bytes.len() {
                break;
            }
            match class(i) {
                UPPER => (upper, i) = (true, i + 1),
                WIDE => {
                    let c = text[i..].chars().next().unwrap_or_default();
                    if !c.is_alphanumeric() {
                        break;
                    }
                    (ascii, i) = (false, i + c.len_utf8());
                }
                _ => break,
            }
        }
        if i == start {
            // A non-ASCII separator (or the end of the text).
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        }
        let run = &text[start..i];
        if ascii && !upper {
            f(run);
            continue;
        }
        scratch.clear();
        if ascii {
            scratch.push_str(run);
            scratch.make_ascii_lowercase();
        } else {
            scratch.extend(run.chars().flat_map(char::to_lowercase));
        }
        f(scratch);
    }
}

const SEP: u8 = 0;
const LOWER: u8 = 1;
const UPPER: u8 = 2;
const WIDE: u8 = 3;

/// Byte classes for [`each_token`]: ASCII separator, lowercase-or-digit,
/// uppercase, and non-ASCII (decoded as a char).
static CLASS: [u8; 256] = {
    let mut t = [SEP; 256];
    let mut b = 0;
    while b < 256 {
        let c = b as u8;
        t[b] = if !c.is_ascii() {
            WIDE
        } else if c.is_ascii_lowercase() || c.is_ascii_digit() {
            LOWER
        } else if c.is_ascii_uppercase() {
            UPPER
        } else {
            SEP
        };
        b += 1;
    }
    t
};

/// [`terms`] as slices through [`each_token`]: stopwords dropped, then the
/// plural strip.
fn each_term(text: &str, scratch: &mut String, mut f: impl FnMut(&str)) {
    each_token(text, scratch, |t| {
        if !is_stopword(t) {
            f(plural_stem(t));
        }
    });
}

/// Minimal plural strip: longer than 3 chars, ends in `s` but not `ss`.
fn plural_stem(tok: &str) -> &str {
    if tok.ends_with('s') && !tok.ends_with("ss") && tok.chars().nth(3).is_some() {
        &tok[..tok.len() - 1]
    } else {
        tok
    }
}

fn strip_plural(tok: String) -> String {
    match plural_stem(&tok).len() {
        n if n == tok.len() => tok,
        n => tok[..n].to_string(),
    }
}

/// Index terms of `text`, in order, repeats kept: stopwords dropped, then
/// the plural strip.
pub fn terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    each_term(text, &mut String::new(), |t| out.push(t.to_string()));
    out
}

/// Every token of `text` normalized like [`terms`] but with stopwords
/// kept — for keyword triggers, which match whatever the user named.
pub fn raw_terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    each_token(text, &mut String::new(), |t| {
        out.push(plural_stem(t).to_string());
    });
    out
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
    /// Folded use history (activation), from the store journal at build
    /// time plus every use recorded through [`Index::record_use`].
    pub uses: activation::Uses,
    /// The compiled `trigger:` of a prospective note; None when absent,
    /// unparsable, or not prospective.
    pub trigger: Option<super::notice::Trigger>,
    /// `uses` holds journal history, not just the unused-note default.
    used: bool,
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
    /// Interned terms → their posting list in `postings`.
    vocab: FxMap<Box<str>, u32>,
    postings: Vec<Vec<(u32, [u16; 4])>>,
    avg: [f64; 4],
    /// Each doc's rank in `scope:rel` order — the deterministic tie-break.
    order: Vec<u32>,
    /// Per store (aligned with `stores`): INDEX.md's (size, mtime) and
    /// text, reused by a rebuild while the stat is unchanged.
    index_text: Vec<(Option<(u64, Duration)>, String)>,
}

/// One topic file as the directory walk saw it (one metadata call).
struct Walked {
    rel: String,
    path: PathBuf,
    size: u64,
    mtime: Duration,
}

/// Topic files of one store: the root and each layer dir, never
/// `proposals/`. Sorted for a deterministic doc order.
fn topic_files(dir: &Path) -> Vec<Walked> {
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
                && (sub.is_some() || (name != INDEX_NAME && name != CORE_NAME));
            if !is_topic {
                continue;
            }
            let Ok(md) = e.metadata() else {
                continue;
            };
            if !md.is_file() {
                continue;
            }
            out.push(Walked {
                rel: sub.map_or_else(|| name.clone(), |s| format!("{s}/{name}")),
                path: e.path(),
                size: md.len(),
                mtime: mtime_of(&md),
            });
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

fn mtime_of(md: &std::fs::Metadata) -> Duration {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default()
}

/// One store on disk: its walked topic files and INDEX.md's stat.
struct Snapshot {
    files: Vec<Walked>,
    index: Option<(u64, Duration)>,
}

fn snapshot(stores: &[(Scope, PathBuf)]) -> Vec<Snapshot> {
    stores
        .iter()
        .map(|(_, dir)| Snapshot {
            files: topic_files(dir),
            index: std::fs::metadata(dir.join(INDEX_NAME))
                .ok()
                .map(|m| (m.len(), mtime_of(&m))),
        })
        .collect()
}

/// Read a walked file with the walk's size as the buffer hint:
/// `fs::read_to_string` would stat the file a second time for it.
fn read_sized(path: &Path, size: u64) -> std::io::Result<String> {
    use std::io::Read;
    let cap = usize::try_from(size).unwrap_or(0).saturating_add(1);
    let mut buf = Vec::with_capacity(cap);
    std::fs::File::open(path)?
        .take(u64::MAX)
        .read_to_end(&mut buf)?;
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Hash over every topic file's path, size and mtime plus each INDEX.md.
fn fingerprint(snaps: &[Snapshot]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for snap in snaps {
        for w in &snap.files {
            (&w.path, w.size, w.mtime).hash(&mut h);
        }
        snap.index.hash(&mut h);
    }
    h.finish()
}

impl Index {
    /// Build over `stores` at clock `now` (liveness is evaluated at `now`).
    pub fn build(stores: &[(Scope, PathBuf)], now: u64) -> Index {
        Index::assemble(stores, snapshot(stores), now, Vec::new())
    }

    /// Build from `snaps`, reusing a cached INDEX.md text from `cache`
    /// (aligned with `stores`) whose stat is unchanged.
    fn assemble(
        stores: &[(Scope, PathBuf)],
        snaps: Vec<Snapshot>,
        now: u64,
        mut cache: Vec<(Option<(u64, Duration)>, String)>,
    ) -> Index {
        let stamp = fingerprint(&snaps);
        let mut docs: Vec<Doc> = Vec::new();
        let mut vocab: FxMap<Box<str>, u32> = FxMap::default();
        // Postings accumulate flat and in doc order, then scatter once into
        // exactly-sized per-term lists: no growth, no scattered pushes.
        let mut df: Vec<u32> = Vec::new();
        let mut flat: Vec<(u32, u32, [u16; 4])> = Vec::new();
        let mut tf: FxMap<u32, [u16; 4]> = FxMap::default();
        let mut scratch = String::new();
        let mut total = [0f64; 4];
        let mut expires: Option<u64> = None;
        let mut index_text = Vec::with_capacity(stores.len());
        for (k, ((scope, dir), snap)) in stores.iter().zip(snaps).enumerate() {
            let cached = cache
                .get_mut(k)
                .filter(|(stat, _)| stat.is_some() && *stat == snap.index)
                .map(|(_, text)| std::mem::take(text));
            let text = cached.unwrap_or_else(|| {
                std::fs::read_to_string(dir.join(INDEX_NAME)).unwrap_or_default()
            });
            let walked: HashSet<&str> = snap.files.iter().map(|w| w.rel.as_str()).collect();
            let lines: HashMap<String, String> =
                super::pointer_lines_in(&text, |rel| walked.contains(rel))
                    .into_iter()
                    .collect();
            let mut uses = activation::load(dir);
            for w in snap.files {
                let Ok(text) = read_sized(&w.path, w.size) else {
                    continue;
                };
                // Malformed headers fail closed: never served.
                let Ok((meta, body)) = super::parse_meta(&text) else {
                    continue;
                };
                let mtime = w.mtime.as_secs();
                if !super::current_at(&meta, &text, mtime, now) {
                    continue;
                }
                if let Some(t) = super::expires_at(&meta, mtime) {
                    expires = Some(expires.map_or(t, |e| e.min(t)));
                }
                let doc = docs.len() as u32;
                let mut len = [0u32; 4];
                tf.clear();
                let mut add = |field: usize, text: &str| {
                    each_term(text, &mut scratch, |t| {
                        let id = match vocab.get(t) {
                            Some(&id) => id,
                            None => {
                                let id = df.len() as u32;
                                vocab.insert(t.into(), id);
                                df.push(0);
                                id
                            }
                        };
                        let e = tf.entry(id).or_default();
                        e[field] = e[field].saturating_add(1);
                        len[field] += 1;
                    });
                };
                add(0, w.rel.trim_end_matches(".md"));
                for cue in &meta.cues {
                    add(1, cue);
                }
                add(2, lines.get(&w.rel).map_or("", String::as_str));
                add(3, &body);
                for (t, l) in total.iter_mut().zip(len) {
                    *t += f64::from(l);
                }
                for (t, counts) in tf.drain() {
                    df[t as usize] += 1;
                    flat.push((t, doc, counts));
                }
                let prospective = w.rel.starts_with("prospective/");
                let trigger = meta
                    .trigger
                    .as_deref()
                    .filter(|_| prospective)
                    .and_then(|t| super::notice::Trigger::parse(t).ok());
                let journal = uses.remove(&w.rel);
                docs.push(Doc {
                    scope: *scope,
                    title: super::title_of(&body).to_string(),
                    used: journal.is_some(),
                    uses: journal.unwrap_or_else(|| activation::Uses::once(mtime)),
                    trigger,
                    rel: w.rel,
                    path: w.path,
                    body,
                    meta,
                    mtime,
                    len,
                    links: Vec::new(),
                });
            }
            index_text.push((snap.index, text));
        }
        let mut postings: Vec<Vec<(u32, [u16; 4])>> =
            df.iter().map(|&n| Vec::with_capacity(n as usize)).collect();
        for (t, doc, counts) in flat {
            postings[t as usize].push((doc, counts));
        }
        let n = docs.len().max(1) as f64;
        let avg = total.map(|t| t / n);
        let mut by_id: Vec<u32> = (0..docs.len() as u32).collect();
        by_id.sort_by(|&a, &b| {
            let key = |d: u32| {
                let d = &docs[d as usize];
                (d.scope.name(), d.rel.as_str())
            };
            key(a).cmp(&key(b))
        });
        let mut order = vec![0u32; docs.len()];
        for (rank, d) in by_id.into_iter().enumerate() {
            order[d as usize] = rank as u32;
        }
        let mut index = Index {
            stores: stores.to_vec(),
            stamp,
            expires,
            docs,
            vocab,
            postings,
            avg,
            order,
            index_text,
        };
        index.link();
        index
    }

    /// Rebuild when a store changed on disk or an indexed note lapsed.
    /// INDEX.md is re-read only when its stat changed.
    pub fn refresh(&mut self, now: u64) {
        let lapsed = self.expires.is_some_and(|e| now >= e);
        let snaps = snapshot(&self.stores);
        if lapsed || fingerprint(&snaps) != self.stamp {
            let cache = std::mem::take(&mut self.index_text);
            let stores = std::mem::take(&mut self.stores);
            *self = Index::assemble(&stores, snaps, now, cache);
        }
    }

    /// Record a use of doc `i` at `t`: appended to its store's journal and
    /// folded into the doc, so ranking sees it without a reload.
    pub fn record_use(&mut self, i: usize, t: u64) -> std::io::Result<()> {
        let d = &mut self.docs[i];
        if let Some((_, dir)) = self.stores.iter().find(|(s, _)| *s == d.scope) {
            activation::record(dir, &d.rel, t)?;
        }
        if d.used {
            d.uses.add(t);
        } else {
            (d.uses, d.used) = (activation::Uses::once(t), true);
        }
        Ok(())
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
        let df = self.posting(term).map_or(0, <[_]>::len) as f64;
        (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
    }

    fn posting(&self, term: &str) -> Option<&[(u32, [u16; 4])]> {
        self.vocab
            .get(term)
            .map(|&t| self.postings[t as usize].as_slice())
    }

    /// Position in `docs` of [`Index::find`]'s note.
    pub fn position(&self, name: &str) -> Option<usize> {
        let d = self.find(name)?;
        self.docs.iter().position(|x| std::ptr::eq(x, d))
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

    /// The current note a qualified name ([`super::parse_qualified`])
    /// names. An unqualified or unknown name is an error listing the
    /// qualified ids whose file name matches.
    pub fn resolve_qualified(&self, name: &str) -> Result<&Doc, String> {
        let found = super::parse_qualified(name).and_then(|(scope, _, rel)| {
            self.docs.iter().find(|d| d.scope == scope && d.rel == rel)
        });
        if let Some(d) = found {
            return Ok(d);
        }
        let stem = name.trim().rsplit(['/', ':']).next().unwrap_or("");
        let stem = stem.strip_suffix(".md").unwrap_or(stem);
        let candidates: Vec<String> = self
            .docs
            .iter()
            .filter(|d| d.rel.rsplit('/').next() == Some(&format!("{stem}.md")))
            .map(Doc::id)
            .collect();
        let head = if super::parse_qualified(name).is_some() {
            format!("no current note named `{name}`")
        } else {
            format!("`{name}` is not qualified — use scope:layer/name.md or layer/name.md")
        };
        Err(if candidates.is_empty() {
            head
        } else {
            format!("{head}; candidates: {}", candidates.join(", "))
        })
    }

    /// BM25F over `query`: `(doc, score, matched terms, Σ idf matched)`.
    fn bm25f(&self, query: &[String]) -> HashMap<u32, (f64, usize, f64)> {
        let mut acc: HashMap<u32, (f64, usize, f64)> = HashMap::new();
        for t in query {
            let Some(list) = self.posting(t) else {
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
        let id = |d: u32| self.order[d as usize];
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

        let act = |d: u32| activation::base_level(&self.docs[d as usize].uses, now);
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
                .then_with(|| self.order[a.doc].cmp(&self.order[b.doc]))
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
        assert_eq!(STOPWORDS.iter().map(|w| w.len()).max(), Some(STOPWORD_MAX));
        for w in STOPWORDS {
            assert!(is_stopword(w), "{w}");
        }
        for w in ["", "zz", "x", "abouts", "É", "7", "betweenness"] {
            assert!(!is_stopword(w), "{w}");
        }
        assert_eq!(
            terms("The Deploys of ÉCOLE: glass buses, is it tests? cats's"),
            ["deploy", "école", "glass", "buse", "test", "cat", "s"]
        );
        // ≤3 chars keeps its s; `ss` keeps it too.
        assert_eq!(terms("gas bus boss"), ["gas", "bus", "boss"]);
        assert_eq!(raw_terms("The notes"), ["the", "note"]);
        assert_eq!(query_terms("deploy deploys Deploy"), ["deploy"]);
    }

    /// The slice tokenizer is the char tokenizer, minus the allocations.
    #[test]
    fn slice_tokenizer_matches_the_char_tokenizer() {
        let mut scratch = String::new();
        for text in [
            "The Deploys of ÉCOLE: glass buses, is it tests? cats's",
            "İstanbul STRASSE straße ΣΊΣΥΦΟΣ naïve café-bar v2.0_rc1",
            "   ",
            "x",
            "mixedCASE ascii123 ÅngströmS 日本語テキスト ﬁle",
            "tabs\tand\nnewlines—em–dash…ellipsis",
        ] {
            let want: Vec<String> = spans(text).into_iter().map(|(_, t)| t).collect();
            let mut got = Vec::new();
            each_token(text, &mut scratch, |t| got.push(t.to_string()));
            assert_eq!(got, want, "{text}");
            let want: Vec<String> = spans(text)
                .into_iter()
                .filter(|(_, t)| !is_stopword(t))
                .map(|(_, t)| strip_plural(t))
                .collect();
            assert_eq!(terms(text), want, "{text}");
        }
    }

    /// Activation is folded at build: search never reads the journal, and
    /// `record_use` updates the folded state in-process.
    #[test]
    fn search_uses_folded_activation() {
        let dir = store("uses");
        note(&dir, "semantic/a.md", "kiwi kiwi kiwi\n");
        note(&dir, "semantic/b.md", "kiwi and more words here\n");
        activation::record(&dir, "semantic/a.md", NOW).unwrap();
        let mut idx = Index::build(&[(Scope::Project, dir.clone())], NOW);
        let pos = |idx: &Index, rel: &str| idx.docs.iter().position(|d| d.rel == rel).unwrap();
        let (a, b) = (pos(&idx, "semantic/a.md"), pos(&idx, "semantic/b.md"));
        assert_eq!(
            idx.docs[a].uses.n, 1,
            "the journal use replaces the mtime default"
        );
        let before = idx.search("kiwi", NOW);
        for _ in 0..5 {
            activation::record(&dir, "semantic/b.md", NOW).unwrap();
        }
        assert_eq!(idx.search("kiwi", NOW), before, "journal not reloaded");
        assert_eq!(idx.docs[b].uses.n, 1);
        for _ in 0..5 {
            idx.record_use(b, NOW).unwrap();
        }
        assert_eq!(idx.docs[b].uses.n, 5);
        let after = idx.search("kiwi", NOW);
        let score = |hits: &[Hit]| hits.iter().find(|h| h.doc == b).unwrap().score;
        assert!(score(&after) > score(&before), "{before:?} → {after:?}");
        let rebuilt = Index::build(&[(Scope::Project, dir.clone())], NOW);
        assert_eq!(rebuilt.docs[b].uses.n, 10, "journal holds both writers");
        idx.record_use(a, NOW).unwrap();
        let rebuilt = Index::build(&[(Scope::Project, dir.clone())], NOW);
        assert_eq!(
            idx.docs[a].uses, rebuilt.docs[a].uses,
            "fold matches a reload"
        );
    }

    /// A rebuild reuses INDEX.md's text while its stat is unchanged.
    #[cfg(unix)]
    #[test]
    fn refresh_rereads_index_md_only_when_it_changed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = store("indexcache");
        note(&dir, "semantic/a.md", "alpha\n");
        note(&dir, "semantic/b.md", "beta\n");
        std::fs::write(dir.join(INDEX_NAME), "semantic/a.md — zucchini\n").unwrap();
        let mut idx = Index::build(&[(Scope::Project, dir.clone())], NOW);
        let hit = |idx: &Index, q: &str| {
            idx.search(q, NOW)
                .first()
                .map(|h| idx.docs[h.doc].rel.clone())
        };
        assert_eq!(hit(&idx, "zucchini").as_deref(), Some("semantic/a.md"));
        let index = dir.join(INDEX_NAME);
        let mtime = std::fs::metadata(&index).unwrap().modified().unwrap();
        std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = std::fs::read_to_string(&index).is_err();
        note(&dir, "semantic/b.md", "beta changed\n");
        idx.refresh(NOW);
        std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(idx.search("changed", NOW).len() == 1, "rebuilt");
        if unreadable {
            assert_eq!(
                hit(&idx, "zucchini").as_deref(),
                Some("semantic/a.md"),
                "cached INDEX text reused"
            );
        }
        std::fs::write(&index, "semantic/b.md — zucchini\n").unwrap();
        let f = std::fs::File::options().write(true).open(&index).unwrap();
        f.set_modified(mtime + std::time::Duration::from_secs(5))
            .unwrap();
        idx.refresh(NOW);
        assert_eq!(hit(&idx, "zucchini").as_deref(), Some("semantic/b.md"));
    }

    #[test]
    fn triggers_compile_once_at_build() {
        let dir = store("triggers");
        note(
            &dir,
            "prospective/ok.md",
            "---\ntrigger: kw:deploy\n---\nx\n",
        );
        note(
            &dir,
            "prospective/bad.md",
            "---\ntrigger: every:1d\n---\nx\n",
        );
        note(&dir, "semantic/s.md", "---\ntrigger: kw:deploy\n---\nx\n");
        let idx = Index::build(&[(Scope::Project, dir)], NOW);
        let t = |rel: &str| {
            idx.docs
                .iter()
                .find(|d| d.rel == rel)
                .unwrap()
                .trigger
                .clone()
        };
        assert!(
            matches!(t("prospective/ok.md"), Some(crate::memory::notice::Trigger::Kw(k)) if k == ["deploy"])
        );
        assert!(t("prospective/bad.md").is_none());
        assert!(
            t("semantic/s.md").is_none(),
            "only prospective notes trigger"
        );
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
            // I/O floor: the walk plus reading every file, no indexing.
            let t = std::time::Instant::now();
            let bytes: usize = snapshot(&stores)
                .iter()
                .flat_map(|s| &s.files)
                .map(|w| read_sized(&w.path, w.size).map_or(0, |t| t.len()))
                .sum();
            let floor = t.elapsed();
            let t = std::time::Instant::now();
            let idx = Index::build(&stores, NOW);
            let build = t.elapsed();
            let t = std::time::Instant::now();
            let hits = idx.search("w1aq w2bq c5 note", NOW);
            let search = t.elapsed();
            println!(
                "bench n={n:>6}: build {:>8.1} ms, search {:>7.2} ms ({} hits; \
                 walk+read floor {:.1} ms over {bytes} B)",
                build.as_secs_f64() * 1e3,
                search.as_secs_f64() * 1e3,
                hits.len(),
                floor.as_secs_f64() * 1e3,
            );
        }
    }
}
