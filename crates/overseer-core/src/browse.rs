//! Browser/agent-surface patterns ported from the P8-C batch (arsenal B3).
//!
//! Three ports, all pure and dependency-free. No browser transport, no DOM,
//! no coordinate frame capture: what is ported is the *shape* each project
//! hands the model, so the surfaces that read them are testable without
//! opening a page.
//!
//! - **browser-use indexed DOM** — browser-use shows the model a numbered
//!   list of interactive nodes (`[1] button "Submit"`) and takes the number
//!   back. The port is the index (`index_elements`), the line list the model
//!   reads (`render_index`), and the lookup that turns a spoken name back
//!   into a number (`find`). An element the model cannot address — hidden,
//!   disabled, or carrying neither a name nor a role — is *not in the index*,
//!   because a number that cannot be acted on is a trap; the cap is reported,
//!   never applied silently.
//! - **stagehand triad** — stagehand splits a step into one of three kinds:
//!   `act` (mutates the page), `extract` (reads structured data), `observe`
//!   (reads the page for planning). The port is the kind (`Act`), its
//!   mutability invariant (`mutates`), and the deterministic planner
//!   (`plan_act`) that refuses to describe an act with no target.
//! - **ui-tars coordinate denormalisation** — UI-TARS emits coordinates on a
//!   normalised `0..=1000` grid; `denorm` maps them onto the native pixel
//!   frame of the observation the model was shown. This is the INVERSE
//!   direction of `tools::computer::scale_coords` (which maps model-frame
//!   coordinates *out* to native pixels using the observed frame); both sides
//!   round the same way and clamp to the same `0..=extent-1` range, so a
//!   round trip through the pair is stable. Rounding is `f64::round` —
//!   nearest, halves away from zero — applied after scaling and before
//!   clamping.
//!
//! Text discipline: the rendered index is line-oriented — every label is
//! whitespace-collapsed and its quotes folded — so a name can never forge an
//! index line and one node is exactly one line (see [`render_index`]).
//!
//! `// DEFERRED(owner): driving an actual browser (a CDP/WebDriver session,
//! DOM traversal, accessibility-tree capture, screenshot frames) — the index,
//! the step plan, and the coordinate math land here; the transport stays with
//! the operator-configured helper process, exactly as `tools::computer`
//! defers every platform binding to a named backend binary.`

/// Cap on indexed nodes: a page can carry thousands of interactive elements,
/// and the index is a prompt, so it is bounded at [`MAX_INDEXED`] entries.
/// The first `MAX_INDEXED` addressable elements in input order are kept and
/// the remainder is reported via [`Indexed::truncated`] plus a note — never
/// dropped in silence.
pub const MAX_INDEXED: usize = 200;

/// One element as the capture backend saw it, before indexing. Backends
/// report the raw attribute strings; [`index_elements`] owns normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Accessibility role (`button`, `textbox`, …). May be empty when the
    /// backend has no role for the node; then `name` alone must address it.
    pub role: String,
    /// Accessible name / visible label. May be empty for unlabelled nodes
    /// (e.g. an icon button), which stay addressable by role.
    pub name: String,
    /// Source tag (`a`, `input`, …), carried for the caller's use — the
    /// rendered index shows role + name, so `tag` never affects addressing.
    pub tag: String,
    /// Whether the node is actually painted. An invisible node cannot be
    /// clicked, so it is never indexed even when it is named.
    pub visible: bool,
    /// Whether the node accepts interaction. A disabled node is a dead
    /// target, so it is never indexed even when it is named.
    pub enabled: bool,
}

/// One addressable element, in the model's frame of reference. Every field is
/// normalized (see [`index_elements`]) so `name` and `role` are exactly the
/// strings the rendered line shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// 1-based position in the index; the number the model sends back.
    pub index: usize,
    /// Normalized role (trimmed, whitespace-collapsed, lowercased).
    pub role: String,
    /// Normalized name (trimmed, whitespace-collapsed, `"` folded to `'`);
    /// case is preserved for display.
    pub name: String,
    /// Normalized source tag (same fold as `role`).
    pub tag: String,
}

/// The indexed DOM — what the model may address.
///
/// Invariants: `nodes` holds contiguous 1-based indices starting at 1, in
/// input order; `truncated` is the number of *addressable* elements left out
/// by the [`MAX_INDEXED`] cap; `note` is `Some` exactly when `truncated > 0`.
/// An empty input yields an empty index with `note == None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indexed {
    /// Indexed nodes, in input order, `nodes[i].index == i + 1`.
    pub nodes: Vec<Node>,
    /// Addressable elements beyond the cap that are not in `nodes`.
    pub truncated: usize,
    /// Human-readable truncation notice; `Some` iff `truncated > 0`.
    pub note: Option<String>,
}

/// Collapse every whitespace run to one space and trim both ends.
///
/// Invariant: the result contains no leading or trailing whitespace and no
/// two consecutive whitespace chars, so it can be embedded in one line.
fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for c in s.chars() {
        if c.is_whitespace() {
            pending = !out.is_empty();
            continue;
        }
        if pending {
            out.push(' ');
            pending = false;
        }
        out.push(c);
    }
    out
}

/// The structural fold applied to roles and tags: they are tokens, not prose,
/// so they compare and render case-insensitively.
fn fold_token(s: &str) -> String {
    squash(s).to_lowercase()
}

/// The display fold applied to names: whitespace is collapsed so one node is
/// one line, and `"` becomes `'` so the quoted label cannot close early.
fn fold_name(s: &str) -> String {
    squash(s).replace('"', "'")
}

/// The needle fold: a query is matched against normalized labels (names keep
/// their case on the way out, so both sides lowercase here).
fn fold_needle(s: &str) -> String {
    fold_name(&s.to_lowercase())
}

/// Build the index the model is shown.
///
/// Rules, in this order per element:
/// 1. an invisible or disabled element is skipped — it cannot be acted on, so
///    a number for it would be a trap;
/// 2. an element whose normalized `name` AND `role` are both empty is skipped
///    — the model cannot address it, so it is not in the index;
/// 3. otherwise it gets the next 1-based index, in input order, up to
///    [`MAX_INDEXED`] entries.
///
/// `role`/`tag` are trimmed, whitespace-collapsed and lowercased; `name` is
/// trimmed, whitespace-collapsed and has `"` folded to `'`. Because skipping
/// happens before the cap, `truncated` counts *addressable* elements only —
/// hidden elements never inflate the number.
///
/// The cap keeps the FIRST `MAX_INDEXED` and reports the rest in
/// [`Indexed::truncated`] and [`Indexed::note`]; nothing is dropped in
/// silence. An empty input is an empty index with no note (there is nothing
/// to report).
pub fn index_elements(elements: &[Element]) -> Indexed {
    let mut nodes: Vec<Node> = Vec::new();
    let mut addressable = 0usize;
    for el in elements {
        if !el.visible || !el.enabled {
            continue;
        }
        let role = fold_token(&el.role);
        let name = fold_name(&el.name);
        if role.is_empty() && name.is_empty() {
            continue;
        }
        addressable += 1;
        if nodes.len() >= MAX_INDEXED {
            continue;
        }
        nodes.push(Node {
            index: nodes.len() + 1,
            role,
            name,
            tag: fold_token(&el.tag),
        });
    }
    let truncated = addressable.saturating_sub(nodes.len());
    let note = if truncated > 0 {
        Some(format!(
            "index truncated: showing the first {MAX_INDEXED} of {addressable} addressable elements — {truncated} more not shown; scroll or re-observe to reach them"
        ))
    } else {
        None
    };
    Indexed {
        nodes,
        truncated,
        note,
    }
}

/// `[3] button "Submit"` — one node in the model's frame. A roleless node
/// renders `?` so the line stays parseable; an unlabelled node renders an
/// empty quoted string.
fn label(index: usize, role: &str, name: &str) -> String {
    let role = if role.is_empty() { "?" } else { role };
    format!("[{index}] {role} \"{name}\"")
}

/// The line list the model is shown, one line per node, `\n`-joined with no
/// trailing newline.
///
/// Invariants: byte-stable for equal input (no map iteration, no locale, no
/// hash order — the nodes are already ordered); exactly one line per node —
/// every label is whitespace-collapsed, so a name containing a newline cannot
/// forge a second index line; and each line names the node's own `index`, so
/// the model can send a number back that [`find`] resolves against the same
/// list. A truncated index appends one final `… N more` line; an empty index
/// renders as the empty string (nothing to show).
pub fn render_index(ix: &Indexed) -> String {
    let mut lines: Vec<String> = ix
        .nodes
        .iter()
        .map(|n| label(n.index, &n.role, &n.name))
        .collect();
    if ix.truncated > 0 {
        lines.push(format!("… {} more", ix.truncated));
    }
    lines.join("\n")
}

/// Resolve a spoken target to the node the model should address.
///
/// The needle is normalized the same way labels are (trimmed, whitespace
/// collapsed, lowercased, `"` folded to `'`). The `name` of every node is
/// searched first, in index order; only if no name matches is `role` searched,
/// also in index order. The LOWEST index always wins, and that determinism is
/// the point: the same phrase against the same page must resolve to the same
/// number, or a replayed plan drifts between runs.
///
/// A blank (or whitespace-only) needle addresses nothing and returns `None` —
/// refusing a valueless query beats returning node 1 and acting on it.
pub fn find<'a>(ix: &'a Indexed, needle: &str) -> Option<&'a Node> {
    let needle = fold_needle(needle);
    if needle.is_empty() {
        return None;
    }
    ix.nodes
        .iter()
        .find(|n| n.name.to_lowercase().contains(&needle))
        .or_else(|| ix.nodes.iter().find(|n| n.role.contains(&needle)))
}

/// The three stagehand step kinds. `Act` is the only one that touches the
/// page; `Extract` and `Observe` only read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// Mutates the page (click, type, submit, navigate).
    Act,
    /// Reads structured data off the page.
    Extract,
    /// Reads the page to plan the next step.
    Observe,
}

impl Act {
    /// Every kind, in the order the error and the docs list them.
    pub const ALL: [Act; 3] = [Act::Act, Act::Extract, Act::Observe];

    /// The bare kind name as the model writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Act::Act => "Act",
            Act::Extract => "Extract",
            Act::Observe => "Observe",
        }
    }

    /// Parse a kind from model text. Case is ignored and `-`/`_` separators
    /// are dropped, so `act`, `ACT`, `Extract` and `ex-tract` all resolve.
    ///
    /// The error names the offending value and lists every accepted kind — a
    /// near-miss like `Action` must not silently become `Act`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let want = fold_needle(s).replace(['-', '_'], "");
        if want.is_empty() {
            return Err(format!(
                "browse: empty step kind — expected one of {}",
                Self::kinds()
            ));
        }
        Self::ALL
            .into_iter()
            .find(|a| fold_needle(a.as_str()).replace(['-', '_'], "") == want)
            .ok_or_else(|| {
                format!(
                    "browse: unknown step kind \"{s}\" — expected one of {}",
                    Self::kinds()
                )
            })
    }

    /// Whether the step can change the page. True ONLY for [`Act::Act`]:
    /// `Extract` and `Observe` are read-only, and that is the invariant the
    /// permission gate and the audit trail key off.
    pub fn mutates(self) -> bool {
        matches!(self, Act::Act)
    }

    /// `Act|Extract|Observe`, built from [`Act::ALL`] so the list cannot drift
    /// from the enum.
    fn kinds() -> String {
        Self::ALL
            .iter()
            .map(|a| a.as_str())
            .collect::<Vec<_>>()
            .join("|")
    }
}

/// The deterministic description of one step, e.g. `act on [3] button
/// "Submit"`.
///
/// Invariants: the string is a pure function of `(kind, target index, role,
/// name)` — no clock, no page state — so a logged plan replays identically.
/// `Act` REQUIRES a resolved target: `None` is an error naming why (an
/// unaddressed act is a blind click at a number the model never chose).
/// `Extract` and `Observe` accept `None`, which means the page as a whole
/// rather than a single node, and say so in the description.
pub fn plan_act(act: Act, target: Option<&Node>) -> Result<String, String> {
    match (act, target) {
        (Act::Act, None) => Err(
            "browse: act needs a resolved target — an unaddressed act is a blind click; call find() first and pass the node"
                .to_string(),
        ),
        (Act::Act, Some(n)) => Ok(format!("act on {}", label(n.index, &n.role, &n.name))),
        (Act::Extract, Some(n)) => Ok(format!("extract from {}", label(n.index, &n.role, &n.name))),
        (Act::Extract, None) => Ok("extract from the page".to_string()),
        (Act::Observe, Some(n)) => Ok(format!("observe {}", label(n.index, &n.role, &n.name))),
        (Act::Observe, None) => Ok("observe the page".to_string()),
    }
}

/// One axis of [`denorm`]: normalized value in, native pixel out.
fn denorm_axis(v: f64, extent: u32) -> i64 {
    if !v.is_finite() {
        return 0;
    }
    if extent == 0 {
        return v.round() as i64;
    }
    let scaled = v / 1000.0 * f64::from(extent);
    (scaled.round() as i64).clamp(0, i64::from(extent) - 1)
}

/// Map a UI-TARS coordinate (normalised to `0..=1000`, where 1000 is the far
/// edge) onto the native pixel frame `width`×`height`.
///
/// This is the INVERSE direction of `tools::computer::scale_coords`: that one
/// takes a coordinate the model sent in the frame it was *shown* and scales it
/// to native pixels; this one takes the model's normalised grid and lands it
/// on the frame the observation actually has. Both round the same way
/// (`f64::round`, halves away from zero) and clamp to `0..=extent-1`, so a
/// pixel that goes out through one and back through the other returns
/// unchanged.
///
/// Edge rules, both mirrored from `scale_coords`:
/// - a zero dimension means the frame is unknown: the value is passed through
///   unscaled and unclamped, and the caller reports the unknown frame rather
///   than clamping into a frame nobody measured;
/// - a non-finite component contributes `0` on its own axis — an
///   unrepresentable coordinate is a coordinate we do not have, never a guess
///   at the origin of the other axis.
pub fn denorm(x: f64, y: f64, width: u32, height: u32) -> (i64, i64) {
    (denorm_axis(x, width), denorm_axis(y, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(role: &str, name: &str, tag: &str, visible: bool, enabled: bool) -> Element {
        Element {
            role: role.to_string(),
            name: name.to_string(),
            tag: tag.to_string(),
            visible,
            enabled,
        }
    }

    fn shown(role: &str, name: &str) -> Element {
        el(role, name, "div", true, true)
    }

    fn many(n: usize) -> Vec<Element> {
        (0..n)
            .map(|i| shown("button", &format!("Item {i}")))
            .collect()
    }

    #[test]
    fn invisible_disabled_and_unaddressable_elements_are_skipped() {
        let ix = index_elements(&[
            shown("button", "Submit"),
            el("button", "Ghost", "button", false, true),
            el("button", "Dead", "button", true, false),
            el("", "", "div", true, true),
            el("   ", "\n\t ", "div", true, true),
            shown("textbox", "Email"),
        ]);
        assert_eq!(
            ix.nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            vec!["Submit", "Email"]
        );
        assert_eq!(ix.truncated, 0);
        assert_eq!(ix.note, None);
    }

    #[test]
    fn indices_are_contiguous_from_one_in_input_order() {
        let ix = index_elements(&[
            shown("link", "Home"),
            el("button", "Hidden", "button", false, true),
            shown("textbox", "Query"),
            shown("button", "Go"),
        ]);
        let indices: Vec<usize> = ix.nodes.iter().map(|n| n.index).collect();
        assert_eq!(indices, vec![1, 2, 3]);
        assert_eq!(
            ix.nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
            vec!["Home", "Query", "Go"]
        );
    }

    #[test]
    fn cap_keeps_the_first_max_and_reports_truncation() {
        let ix = index_elements(&many(MAX_INDEXED + 3));
        assert_eq!(ix.nodes.len(), MAX_INDEXED);
        assert_eq!(ix.nodes[0].name, "Item 0");
        assert_eq!(
            ix.nodes[MAX_INDEXED - 1].name,
            format!("Item {}", MAX_INDEXED - 1)
        );
        assert_eq!(ix.nodes[MAX_INDEXED - 1].index, MAX_INDEXED);
        assert_eq!(ix.truncated, 3);
        let note = ix.note.expect("truncation must be reported, never silent");
        assert!(note.contains("truncated"), "{note}");
        assert!(note.contains("200"), "{note}");
        assert!(note.contains("3 more"), "{note}");
    }

    #[test]
    fn skipped_elements_do_not_inflate_the_truncation_count() {
        let mut els = many(MAX_INDEXED + 1);
        els.insert(0, el("button", "Ghost", "button", false, true));
        els.push(el("", "", "div", true, true));
        let ix = index_elements(&els);
        assert_eq!(ix.nodes.len(), MAX_INDEXED);
        assert_eq!(ix.truncated, 1);
        assert!(ix.note.unwrap().contains("201 addressable"));
    }

    #[test]
    fn empty_input_yields_an_empty_index_with_no_note() {
        let ix = index_elements(&[]);
        assert!(ix.nodes.is_empty());
        assert_eq!(ix.truncated, 0);
        assert_eq!(ix.note, None);
        assert_eq!(render_index(&ix), "");
        assert_eq!(find(&ix, "anything"), None);
    }

    #[test]
    fn normalization_folds_whitespace_case_and_quotes() {
        let ix = index_elements(&[el(
            "  BUTTON\n",
            "  Say \"hi\"\tthere  ",
            " INPUT ",
            true,
            true,
        )]);
        let n = &ix.nodes[0];
        assert_eq!(n.role, "button");
        assert_eq!(n.name, "Say 'hi' there");
        assert_eq!(n.tag, "input");
    }

    #[test]
    fn render_is_one_line_per_node_and_byte_stable() {
        let els = [
            shown("button", "Submit"),
            el("link", "Docs", "a", true, true),
        ];
        let a = render_index(&index_elements(&els));
        let b = render_index(&index_elements(&els));
        assert_eq!(a, b);
        assert_eq!(a, "[1] button \"Submit\"\n[2] link \"Docs\"");
        assert_eq!(a.lines().count(), 2);
    }

    #[test]
    fn render_collapses_whitespace_so_a_label_cannot_forge_a_line() {
        let ix = index_elements(&[el(
            "button",
            "Evil\n[9] button \"Admin\"",
            "button",
            true,
            true,
        )]);
        let out = render_index(&ix);
        assert_eq!(out.lines().count(), 1);
        assert_eq!(out, "[1] button \"Evil [9] button 'Admin'\"");
    }

    #[test]
    fn render_marks_a_roleless_node_and_appends_the_truncation_line() {
        let mut els = vec![el("", "Unlabelled icon", "img", true, true)];
        els.extend(many(MAX_INDEXED + 2));
        let ix = index_elements(&els);
        assert_eq!(ix.truncated, 3);
        let out = render_index(&ix);
        assert_eq!(out.lines().next().unwrap(), "[1] ? \"Unlabelled icon\"");
        assert_eq!(out.lines().last().unwrap(), "… 3 more");
        assert_eq!(out.lines().count(), MAX_INDEXED + 1);
    }

    #[test]
    fn find_prefers_the_lowest_index_then_the_role() {
        let ix = index_elements(&[
            shown("button", "Docs page"),
            shown("link", "Documentation"),
            shown("button", "Documentation"),
        ]);
        // The name pass runs whole-index first, so node 2 wins over node 3 even
        // though node 3 comes later in index order; among name matches the
        // lowest index wins.
        assert_eq!(find(&ix, "documentation").map(|n| n.index), Some(2));
        assert_eq!(find(&ix, "DOCS").map(|n| n.index), Some(1));
        // No name contains these, so the role pass answers at the lowest index.
        assert_eq!(find(&ix, "link").map(|n| n.index), Some(2));
        assert_eq!(find(&ix, "button").map(|n| n.index), Some(1));
        assert_eq!(find(&ix, "nowhere"), None);
    }

    #[test]
    fn find_rejects_a_valueless_needle() {
        let ix = index_elements(&[shown("button", "Submit")]);
        assert_eq!(find(&ix, ""), None);
        assert_eq!(find(&ix, "   \t\n"), None);
    }

    #[test]
    fn find_normalizes_the_needle_like_a_label() {
        let ix = index_elements(&[shown("button", "Say \"hi\" now")]);
        assert_eq!(find(&ix, "  say \"HI\"  now ").map(|n| n.index), Some(1));
    }

    #[test]
    fn act_parse_accepts_case_and_separator_variants() {
        let cases = [
            ("Act", Act::Act),
            ("act", Act::Act),
            ("ACT", Act::Act),
            ("  Act ", Act::Act),
            ("Extract", Act::Extract),
            ("extract", Act::Extract),
            ("ex-tract", Act::Extract),
            ("EXTRACT", Act::Extract),
            ("Observe", Act::Observe),
            ("observe", Act::Observe),
            ("OB_SERVE", Act::Observe),
            ("ob-serve", Act::Observe),
        ];
        for (text, want) in cases {
            assert_eq!(Act::parse(text).unwrap(), want, "{text} should parse");
        }
    }

    #[test]
    fn act_parse_unknown_lists_every_kind() {
        let err = Act::parse("Action").expect_err("a near miss must not silently become Act");
        assert!(err.contains("Action"), "{err}");
        assert!(err.contains("Act|Extract|Observe"), "{err}");
        let empty = Act::parse("  ").expect_err("an empty kind is not a kind");
        assert!(empty.contains("Act|Extract|Observe"), "{empty}");
    }

    #[test]
    fn mutates_is_true_only_for_act() {
        let mutating: Vec<Act> = Act::ALL.into_iter().filter(|a| a.mutates()).collect();
        assert_eq!(mutating, vec![Act::Act]);
        assert!(Act::Act.mutates());
        assert!(!Act::Extract.mutates());
        assert!(!Act::Observe.mutates());
    }

    #[test]
    fn plan_act_requires_a_target_for_act() {
        let ix = index_elements(&[shown("button", "Submit")]);
        let n = find(&ix, "submit").unwrap();
        let err = plan_act(Act::Act, None).expect_err("an unaddressed act is a blind click");
        assert!(err.contains("blind click"), "{err}");
        assert!(err.contains("target"), "{err}");
        assert_eq!(
            plan_act(Act::Act, Some(n)).unwrap(),
            "act on [1] button \"Submit\""
        );
    }

    #[test]
    fn plan_descriptions_are_deterministic_text() {
        let ix = index_elements(&[shown("button", "Submit"), shown("link", "Docs")]);
        let n = find(&ix, "docs").unwrap();
        assert_eq!(
            plan_act(Act::Extract, Some(n)).unwrap(),
            "extract from [2] link \"Docs\""
        );
        assert_eq!(
            plan_act(Act::Extract, None).unwrap(),
            "extract from the page"
        );
        assert_eq!(
            plan_act(Act::Observe, Some(n)).unwrap(),
            "observe [2] link \"Docs\""
        );
        assert_eq!(plan_act(Act::Observe, None).unwrap(), "observe the page");
        let again = plan_act(Act::Extract, find(&ix, "DOCS")).unwrap();
        assert_eq!(again, "extract from [2] link \"Docs\"");
    }

    #[test]
    fn denorm_maps_the_normalized_frame_to_pixels() {
        assert_eq!(denorm(500.0, 500.0, 1280, 800), (640, 400));
        assert_eq!(denorm(0.0, 0.0, 1280, 800), (0, 0));
        assert_eq!(denorm(500.0, 500.0, 1000, 1000), (500, 500));
    }

    #[test]
    fn denorm_clamps_to_the_frame_edges_and_rejects_garbage() {
        assert_eq!(denorm(1000.0, 1000.0, 1280, 800), (1279, 799));
        assert_eq!(denorm(4000.0, 4000.0, 1280, 800), (1279, 799));
        assert_eq!(denorm(-5.0, -5.0, 1280, 800), (0, 0));
        assert_eq!(denorm(-900.0, 250.0, 1280, 800), (0, 200));
        assert_eq!(denorm(f64::NAN, f64::INFINITY, 1280, 800), (0, 0));
        assert_eq!(denorm(f64::NEG_INFINITY, 500.0, 1280, 800), (0, 400));
    }

    #[test]
    fn denorm_rounds_halves_away_from_zero() {
        // 0.5/1000*1280 = 0.64 -> 1; and a half lands on the far edge.
        assert_eq!(denorm(0.5, 0.0, 1280, 800), (1, 0));
        assert_eq!(denorm(1000.0, 1000.0, 4, 2), (3, 1));
        assert_eq!(denorm(500.0, 500.0, 5, 3), (3, 2));
    }

    #[test]
    fn denorm_passes_an_unknown_frame_through_unclamped() {
        assert_eq!(denorm(500.0, 500.0, 0, 0), (500, 500));
        assert_eq!(denorm(1500.0, -5.0, 0, 0), (1500, -5));
        assert_eq!(denorm(500.0, 500.0, 1280, 0), (640, 500));
        assert_eq!(denorm(1000.0, 1000.0, 1, 1), (0, 0));
    }

    #[test]
    fn denorm_round_trips_a_pixel_through_the_normalized_frame() {
        let (w, h) = (1280u32, 800u32);
        for px in (0..w as i64).step_by(7) {
            let v = px as f64 / f64::from(w) * 1000.0;
            assert_eq!(denorm(v, 0.0, w, h).0, px, "column {px} drifted");
        }
        for py in (0..h as i64).step_by(7) {
            let v = py as f64 / f64::from(h) * 1000.0;
            assert_eq!(denorm(0.0, v, w, h).1, py, "row {py} drifted");
        }
    }
}
