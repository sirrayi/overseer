//! Audit (memory v2) benchmarks — `#[ignore]`d, not findings.
//!
//! `retrieval_benchmark`: a synthetic 2,000-note project store with 200
//! gold queries (8 kinds × 25), scored with recall@3 and MRR for the
//! shipped fusion (`Index::search`) and for ablations recomputed from the
//! same candidate set. `scale_benchmark`: build/refresh/search at
//! 5K/20K/50K notes.
//!
//! cargo test --release -p overseer-core --test audit_memory2_bench -- --ignored --nocapture

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use overseer_core::memory::{self, activation, index::Hit, index::Index, notice, Scope};

const DAY: u64 = 86_400;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ov-audit-m2b-{tag}-{}-{}",
        std::process::id(),
        now()
    ));
    let _ = std::fs::remove_dir_all(&d);
    memory::ensure(&d).unwrap();
    d
}

/// RFC3339 UTC (civil-from-days).
fn rfc3339(t: u64) -> String {
    let days = (t / DAY) as i64;
    let s = t % DAY;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Variant {
    /// Default confidence, no use journal, uniform mtimes.
    Flat,
    /// Confidence spread 0.5–0.9, 10% of notes used 3–15× recently,
    /// mtimes over a year.
    LivedIn,
}

struct Note {
    rel: String,
    text: String,
    /// Seconds before `now`.
    age: u64,
    valid_to: Option<u64>,
}

struct Query {
    kind: &'static str,
    text: String,
    gold: HashSet<String>,
    /// Notes that must never surface (expired).
    banned: HashSet<String>,
}

const SYLL: &[&str] = &[
    "ka", "lo", "mi", "ra", "ve", "zu", "to", "ne", "shi", "po", "da", "fe", "gu", "ri", "sa",
    "bo", "qui", "xan", "yel", "tor",
];
const DBS: &[&str] = &[
    "postgres",
    "mysql",
    "sqlite",
    "cassandra",
    "mongodb",
    "dynamodb",
];
const DAYS: &[&str] = &["monday", "tuesday", "wednesday", "thursday", "friday"];
const ATTRS: &[&str] = &["redis", "kafka", "docker", "ipv6", "grpc"];
const FILLER: &[&str] = &[
    "Last reviewed during the quarterly platform audit.",
    "See the runbook for the full checklist before changing this.",
    "This was decided after the incident review in spring.",
    "Ask in the platform channel if anything here looks stale.",
    "Numbers come from the staging environment measurements.",
    "Keep this in sync with the service catalog entry.",
    "The previous owner left notes in the wiki as well.",
    "Monitoring dashboards link back to this page.",
];

fn entity_names(n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    while out.len() < n {
        let s: String = (0..3).map(|_| SYLL[r.below(SYLL.len())]).collect();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    out
}

fn typo(s: &str) -> String {
    let mut c: Vec<char> = s.chars().collect();
    let i = c.len() / 2;
    c.swap(i - 1, i);
    c.into_iter().collect()
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
        .unwrap_or_default()
}

fn corpus() -> (Vec<Note>, Vec<Query>) {
    let ents = entity_names(360);
    let teams: Vec<String> = entity_names(400)[360..].to_vec();
    let mut r = Rng(42);
    let mut notes = Vec::new();
    let fill = |r: &mut Rng| FILLER[r.below(FILLER.len())].to_string();
    let base_age = |r: &mut Rng| DAY * (5 + r.below(360) as u64);
    let mut negative: HashMap<&str, Vec<String>> = HashMap::new();
    for (i, e) in ents.iter().enumerate() {
        let t = &teams[i % teams.len()];
        let port = 3000 + r.below(6000);
        let f = fill(&mut r);
        notes.push(Note {
            rel: format!("semantic/{e}-port.md"),
            text: format!(
                "# {e} port\n{e} listens on port {port} behind the internal gateway.\n{f}\n"
            ),
            age: base_age(&mut r),
            valid_to: None,
        });
        let db = DBS[r.below(DBS.len())];
        let f = fill(&mut r);
        notes.push(Note {
            rel: format!("semantic/{e}-db.md"),
            text: format!(
                "# {e} database\n{e} stores its records in {db}, version {}.\n{f}\n",
                10 + r.below(8)
            ),
            age: base_age(&mut r),
            valid_to: None,
        });
        notes.push(Note {
            rel: format!("semantic/{e}-owner.md"),
            text: format!(
                "# {e} ownership\n{e} is owned by team {t}. Escalations go through [[team-{t}]].\n"
            ),
            age: base_age(&mut r),
            valid_to: None,
        });
        let deps = if (200..250).contains(&i) {
            let a = ATTRS[(i - 200) / 10];
            negative
                .entry(a)
                .or_default()
                .push(format!("project:semantic/{e}-deps.md"));
            format!("{e} does not use {a}; the team removed {a} last year.")
        } else {
            let a = ATTRS[r.below(ATTRS.len())];
            let b = ATTRS[r.below(ATTRS.len())];
            format!("{e} uses {a} and {b} in production.")
        };
        notes.push(Note {
            rel: format!("semantic/{e}-deps.md"),
            text: format!("# {e} dependencies\n{deps}\n"),
            age: base_age(&mut r),
            valid_to: None,
        });
        let (deploy, age) = if i < 25 {
            (
                format!("{e} now deploys every friday at 16:00 UTC (moved from tuesday)."),
                2 * DAY,
            )
        } else {
            (
                format!(
                    "{e} deploys every {} at {}:00 UTC.",
                    DAYS[r.below(5)],
                    8 + r.below(10)
                ),
                base_age(&mut r),
            )
        };
        notes.push(Note {
            rel: format!("semantic/{e}-deploy.md"),
            text: format!("# {e} deploys\n{deploy}\n"),
            age,
            valid_to: None,
        });
        if i < 25 {
            notes.push(Note { rel: format!("semantic/{e}-deploy-schedule.md"), text: format!("# {e} deploy schedule\n{e} deploys every tuesday at 09:00 UTC; deploy freeze on holidays.\n"), age: 300 * DAY, valid_to: None });
        }
        if (25..50).contains(&i) {
            notes.push(Note {
                rel: format!("semantic/{e}-region-old.md"),
                text: format!(
                    "# {e} region\n{e} region is us-east-1; all {e} region traffic stays there.\n"
                ),
                age: 200 * DAY,
                valid_to: Some(30 * DAY),
            });
            notes.push(Note {
                rel: format!("semantic/{e}-region.md"),
                text: format!("# {e} hosting\n{e} moved to eu-west-2 in the region migration.\n"),
                age: 20 * DAY,
                valid_to: None,
            });
        }
    }
    for t in &teams {
        notes.push(Note { rel: format!("semantic/team-{t}.md"), text: format!("# Team {t}\nTeam {t} is on call via the {t}-oncall channel; the pager rotates weekly.\n"), age: base_age(&mut r), valid_to: None });
    }
    for k in 0..60 {
        let e = &ents[300 + k];
        notes.push(Note { rel: format!("semantic/code-{e}.md"), text: format!("# {e} client\nThe retry logic lives in `parse{}Config` inside src/{e}/client.rs; tune `{e}_client::connect_timeout` for slow links.\n", cap(e)), age: base_age(&mut r), valid_to: None });
    }
    let mut k = 0;
    while notes.len() < 2000 {
        let f = fill(&mut r);
        notes.push(Note {
            rel: format!("semantic/misc-{k}.md"),
            text: format!("# Misc {k}\n{f} The deploy port and database notes live elsewhere.\n"),
            age: base_age(&mut r),
            valid_to: None,
        });
        k += 1;
    }

    let g = |rel: String| -> HashSet<String> { [format!("project:{rel}")].into() };
    let mut qs = Vec::new();
    for (j, e) in ents[100..125].iter().enumerate() {
        let (q, rel) = match j % 3 {
            0 => (format!("{e} port"), format!("semantic/{e}-port.md")),
            1 => (format!("{e} database"), format!("semantic/{e}-db.md")),
            _ => (format!("{e} deploy day"), format!("semantic/{e}-deploy.md")),
        };
        qs.push(Query {
            kind: "keyword",
            text: q,
            gold: g(rel),
            banned: HashSet::new(),
        });
    }
    for (j, e) in ents[125..150].iter().enumerate() {
        let (q, rel) = match j % 3 {
            0 => (
                format!("which socket number does {e} bind to"),
                format!("semantic/{e}-port.md"),
            ),
            1 => (
                format!("where does {e} persist its data"),
                format!("semantic/{e}-db.md"),
            ),
            _ => (
                format!("what weekday does {e} ship releases"),
                format!("semantic/{e}-deploy.md"),
            ),
        };
        qs.push(Query {
            kind: "paraphrase",
            text: q,
            gold: g(rel),
            banned: HashSet::new(),
        });
    }
    for (j, e) in ents[150..175].iter().enumerate() {
        let te = typo(e);
        let (q, rel) = match j % 3 {
            0 => (format!("{te} port"), format!("semantic/{e}-port.md")),
            1 => (format!("{te} database"), format!("semantic/{e}-db.md")),
            _ => (
                format!("{te} deploy day"),
                format!("semantic/{e}-deploy.md"),
            ),
        };
        qs.push(Query {
            kind: "typo",
            text: q,
            gold: g(rel),
            banned: HashSet::new(),
        });
    }
    for (j, e) in ents[300..325].iter().enumerate() {
        let q = if j % 2 == 0 {
            format!("where is parse{}Config defined", cap(e))
        } else {
            format!("parse {e} config function")
        };
        qs.push(Query {
            kind: "code-id",
            text: q,
            gold: g(format!("semantic/code-{e}.md")),
            banned: HashSet::new(),
        });
    }
    for (i, e) in ents.iter().enumerate().take(200).skip(175) {
        let t = &teams[i % teams.len()];
        qs.push(Query {
            kind: "multi-hop",
            text: format!("which channel pages the on call engineer for {e}"),
            gold: g(format!("semantic/team-{t}.md")),
            banned: HashSet::new(),
        });
    }
    let phr = [
        "which service does not use {a}",
        "services without {a}",
        "who removed {a}",
        "service that never uses {a}",
        "no {a} dependency",
    ];
    for a in ATTRS {
        for p in phr {
            qs.push(Query {
                kind: "negation",
                text: p.replace("{a}", a),
                gold: negative[a].iter().cloned().collect(),
                banned: HashSet::new(),
            });
        }
    }
    for e in &ents[..25] {
        qs.push(Query {
            kind: "recency",
            text: format!("when does {e} deploy"),
            gold: g(format!("semantic/{e}-deploy.md")),
            banned: HashSet::new(),
        });
    }
    for e in &ents[25..50] {
        qs.push(Query {
            kind: "expired",
            text: format!("{e} region"),
            gold: g(format!("semantic/{e}-region.md")),
            banned: g(format!("semantic/{e}-region-old.md")),
        });
    }
    assert_eq!((notes.len(), qs.len()), (2000, 200));
    (notes, qs)
}

fn write_store(dir: &Path, notes: &[Note], v: Variant, now: u64) {
    let mut r = Rng(7);
    for n in notes {
        let age = if v == Variant::Flat
            && n.age > 2 * DAY
            && n.age != 300 * DAY
            && n.age != 200 * DAY
            && n.age != 20 * DAY
        {
            30 * DAY
        } else {
            n.age
        };
        let mut fm = String::new();
        if v == Variant::LivedIn {
            fm.push_str(&format!("confidence: 0.{}\n", 5 + r.below(5)));
        }
        if let Some(to) = n.valid_to {
            fm.push_str(&format!("valid_to: {}\n", rfc3339(now - to)));
        }
        let text = if fm.is_empty() {
            n.text.clone()
        } else {
            format!("---\n{fm}---\n{}", n.text)
        };
        let p = dir.join(&n.rel);
        std::fs::write(&p, text).unwrap();
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_secs(now - age))
            .unwrap();
    }
    if v == Variant::LivedIn {
        for _ in 0..200 {
            let n = &notes[r.below(notes.len())];
            for _ in 0..3 + r.below(13) {
                activation::record(dir, &n.rel, now - r.below(60) as u64 * DAY).unwrap();
            }
        }
    }
}

/// Competition ranks (ties share the better rank), 1-based from offset+1.
fn ranks(list: &[(usize, f64)], offset: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut rank = offset;
    for (i, (d, v)) in list.iter().enumerate() {
        if i == 0 || *v != list[i - 1].1 {
            rank = offset + i + 1;
        }
        out.push((*d, rank));
    }
    out
}

#[derive(Clone, Copy)]
struct Signals {
    lex: bool,
    links: bool,
    act: bool,
    conf: bool,
}

/// RRF over the shipped candidate set with the chosen signals. Link-only
/// candidates' association values are not public; they share one
/// lexical rank after every direct match (the fan-out ties they usually
/// have in this corpus).
fn fuse(idx: &Index, hits: &[Hit], s: Signals, now: u64) -> Vec<usize> {
    let id = |d: usize| idx.docs[d].id();
    let sort = |v: &mut Vec<(usize, f64)>| {
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| id(a.0).cmp(&id(b.0))))
    };
    let mut direct: Vec<(usize, f64)> = hits
        .iter()
        .filter(|h| h.matched > 0)
        .map(|h| (h.doc, h.bm25))
        .collect();
    sort(&mut direct);
    let linked: Vec<(usize, f64)> = if s.links {
        hits.iter()
            .filter(|h| h.matched == 0)
            .map(|h| (h.doc, 1.0))
            .collect()
    } else {
        Vec::new()
    };
    let cands: Vec<usize> = direct.iter().chain(&linked).map(|x| x.0).collect();
    let mut act: Vec<(usize, f64)> = cands
        .iter()
        .map(|&d| (d, activation::base_level(&idx.docs[d].uses, now)))
        .collect();
    sort(&mut act);
    let mut conf: Vec<(usize, f64)> = cands
        .iter()
        .map(|&d| (d, idx.docs[d].meta.confidence))
        .collect();
    sort(&mut conf);
    let mut fused: HashMap<usize, f64> = HashMap::new();
    let mut lists: Vec<(&Vec<(usize, f64)>, usize)> = Vec::new();
    if s.lex {
        lists.push((&direct, 0));
        lists.push((&linked, direct.len()));
    }
    if s.act {
        lists.push((&act, 0));
    }
    if s.conf {
        lists.push((&conf, 0));
    }
    for &d in &cands {
        fused.insert(d, 0.0);
    }
    for (l, off) in lists {
        for (d, r) in ranks(l, off) {
            *fused.get_mut(&d).unwrap() += 1.0 / (60.0 + r as f64);
        }
    }
    let mut out: Vec<(usize, f64)> = fused.into_iter().collect();
    sort(&mut out);
    out.into_iter().map(|x| x.0).collect()
}

#[derive(Default, Clone)]
struct Score {
    n: usize,
    r3: usize,
    rr: f64,
    banned: usize,
}

impl Score {
    fn add(&mut self, ranked: &[String], q: &Query) {
        self.n += 1;
        if let Some(p) = ranked.iter().position(|id| q.gold.contains(id)) {
            if p < 3 {
                self.r3 += 1;
            }
            self.rr += 1.0 / (p + 1) as f64;
        }
        self.banned += ranked
            .iter()
            .take(3)
            .filter(|id| q.banned.contains(*id))
            .count();
    }
    fn fmt(&self) -> String {
        format!(
            "R@3 {:.3}  MRR {:.3}",
            self.r3 as f64 / self.n as f64,
            self.rr / self.n as f64
        )
    }
}

#[test]
#[ignore = "benchmark: memory v2 retrieval quality"]
fn retrieval_benchmark() {
    let (notes, qs) = corpus();
    for v in [Variant::Flat, Variant::LivedIn] {
        let t = now();
        let dir = tmp(if v == Variant::Flat { "flat" } else { "lived" });
        write_store(&dir, &notes, v, t);
        let idx = Index::build(&[(Scope::Project, dir.clone())], t);
        let all = Signals {
            lex: true,
            links: true,
            act: true,
            conf: true,
        };
        let configs: [(&str, Option<Signals>); 7] = [
            ("shipped Index::search", None),
            ("reimpl (all signals)", Some(all)),
            (
                "BM25F only",
                Some(Signals {
                    links: false,
                    act: false,
                    conf: false,
                    ..all
                }),
            ),
            (
                "BM25F + links",
                Some(Signals {
                    act: false,
                    conf: false,
                    ..all
                }),
            ),
            ("- activation", Some(Signals { act: false, ..all })),
            ("- confidence", Some(Signals { conf: false, ..all })),
            (
                "- links",
                Some(Signals {
                    links: false,
                    ..all
                }),
            ),
        ];
        let mut table: Vec<(String, HashMap<&str, Score>)> = Vec::new();
        let mut agree = 0;
        let mut recall_fired = 0;
        let mut recall_gold = 0;
        let mut recall_kind: HashMap<&str, (usize, usize)> = HashMap::new();
        for (name, sig) in &configs {
            let mut per: HashMap<&str, Score> = HashMap::new();
            for q in &qs {
                let hits = idx.search(&q.text, t);
                let order: Vec<usize> = match sig {
                    None => hits.iter().map(|h| h.doc).collect(),
                    Some(s) => fuse(&idx, &hits, *s, t),
                };
                if name.starts_with("reimpl") {
                    let shipped: Vec<usize> = hits.iter().take(3).map(|h| h.doc).collect();
                    if order.iter().take(3).copied().collect::<Vec<_>>() == shipped {
                        agree += 1;
                    }
                }
                let ids: Vec<String> = order.iter().map(|&d| idx.docs[d].id()).collect();
                per.entry(q.kind).or_default().add(&ids, q);
                per.entry("ALL").or_default().add(&ids, q);
                if sig.is_none() {
                    let e = recall_kind.entry(q.kind).or_default();
                    if let Some(n) = notice::recall(&idx, &q.text, &HashSet::new(), t) {
                        recall_fired += 1;
                        if n.notes.iter().any(|id| q.gold.contains(id)) {
                            recall_gold += 1;
                            e.1 += 1;
                        }
                        e.0 += 1;
                    }
                }
            }
            table.push((name.to_string(), per));
        }
        println!(
            "\n=== variant {} ({} notes, {} queries) ===",
            if v == Variant::Flat {
                "flat"
            } else {
                "lived-in"
            },
            idx.docs.len(),
            qs.len()
        );
        let kinds = [
            "ALL",
            "keyword",
            "paraphrase",
            "typo",
            "code-id",
            "multi-hop",
            "negation",
            "recency",
            "expired",
        ];
        for (name, per) in &table {
            println!("{name:24} {}", per["ALL"].fmt());
            for k in &kinds[1..] {
                println!("    {k:12} {}", per[k].fmt());
            }
            let leaks: usize = per.values().map(|s| s.banned).sum::<usize>() / 2;
            println!("    expired-in-top3 {leaks}");
        }
        println!("reimpl top-3 agreement with shipped: {agree}/200");
        println!("recall notice fired {recall_fired}/200, carried a gold note {recall_gold}/200");
        for k in &kinds[1..] {
            let (f, g) = recall_kind.get(k).copied().unwrap_or_default();
            println!("    recall {k:12} fired {f}/25 gold {g}/25");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

const VOCAB: usize = 6000;

fn word(i: usize) -> String {
    let s = [
        "ba", "ce", "di", "fo", "gu", "ha", "ji", "ko", "lu", "ma", "ne", "pi", "qo", "ru", "se",
        "ti", "vu", "wa", "xe", "yo",
    ];
    format!("{}{}{}", s[i % 20], s[(i / 20) % 20], s[(i / 400) % 20])
}

#[test]
#[ignore = "benchmark: memory v2 index scale"]
fn scale_benchmark() {
    let sizes: Vec<usize> = std::env::var("AUDIT_SIZES")
        .map(|s| s.split(',').map(|x| x.parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![5_000, 20_000, 50_000]);
    for n in sizes {
        let dir = tmp(&format!("scale{n}"));
        let mut r = Rng(n as u64 | 1);
        for i in 0..n {
            let words: Vec<String> = (0..150)
                .map(|_| {
                    // Zipf-ish: square the uniform draw toward common words.
                    let u = r.below(VOCAB);
                    word(u * u / VOCAB)
                })
                .collect();
            std::fs::write(
                dir.join(format!("semantic/n{i}.md")),
                format!("# Note {i} {}\n{}\n", word(i % VOCAB), words.join(" ")),
            )
            .unwrap();
        }
        let t = now();
        // Warm the page cache once, then time builds.
        let _ = Index::build(&[(Scope::Project, dir.clone())], t);
        let mut builds = Vec::new();
        for _ in 0..3 {
            let s = Instant::now();
            let idx = Index::build(&[(Scope::Project, dir.clone())], t);
            builds.push(s.elapsed());
            drop(idx);
        }
        let mut idx = Index::build(&[(Scope::Project, dir.clone())], t);
        let s = Instant::now();
        for _ in 0..10 {
            idx.refresh(t);
        }
        let refresh = s.elapsed() / 10;
        let qs: Vec<String> = (0..50)
            .map(|i| {
                format!(
                    "{} {} {}",
                    word((i * 37) % VOCAB),
                    word((i * 101) % 300),
                    word(i % 30)
                )
            })
            .collect();
        let s = Instant::now();
        let mut hits = 0;
        for q in &qs {
            hits += idx.search(q, t).len();
        }
        let search = s.elapsed() / qs.len() as u32;
        let s = Instant::now();
        let common = idx.search(&word(0), t).len();
        let one_common = s.elapsed();
        builds.sort();
        println!(
            "n={n:6} build median {:7.1} ms (min {:.1})  refresh(no-op) {:6.2} ms  search(3 terms) {:6.2} ms avg ({} hits avg)  common-term search {:6.2} ms ({common} hits)",
            builds[1].as_secs_f64() * 1e3,
            builds[0].as_secs_f64() * 1e3,
            refresh.as_secs_f64() * 1e3,
            search.as_secs_f64() * 1e3,
            hits / qs.len(),
            one_common.as_secs_f64() * 1e3,
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
