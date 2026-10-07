#let ink = rgb("#17181b")
#let paper = rgb("#f5f5f6")
#let accent = rgb("#476a84")
#let muted = rgb("#6e7176")
#let hairline = rgb("#d6d8db")
#let zebra = rgb("#f5f6f7")
#let headfill = rgb("#e4e6e9")
#let codefill = rgb("#f0f1f3")
#let hfont = "Helvetica Neue"
#let mfont = "Menlo"

#set document(title: "$title$", author: "$author$")
#set text(font: "Charter", size: 10.5pt, fill: ink, lang: "en", hyphenate: auto)
#set par(leading: 4.7pt, justify: true)
#set list(indent: 1.1em, body-indent: 0.55em, spacing: 0.8em)
#set enum(indent: 1.1em, body-indent: 0.55em, spacing: 0.8em)
#set terms(hanging-indent: 1.5em)

#set page(
  paper: "a4",
  margin: (x: 21mm, top: 22mm, bottom: 20mm),
  header: context {
    let sel = selector(heading.where(level: 2))
    let nxt = query(sel.after(here()))
    let ch = if nxt.len() > 0 and nxt.first().location().page() == here().page() {
      nxt.first()
    } else {
      let prev = query(sel.before(here()))
      if prev.len() > 0 { prev.last() }
    }
    if ch != none {
      set text(font: hfont, size: 8pt, fill: muted)
      block(width: 100%)[
        #ch.body
        #v(2.5pt)
        #line(length: 100%, stroke: 0.4pt + hairline)
      ]
    }
  },
  footer: context {
    set text(size: 8.5pt, fill: muted)
    align(center)[#counter(page).display("1")]
  },
)

// Level 1 = part divider: full-bleed ink page, no header/footer. The real
// heading is placed hidden so the outline/bookmark still registers.
#show heading.where(level: 1): it => {
  page(fill: ink, margin: 25mm, header: none, footer: none)[
    #place(top + left, hide(it))
    #v(1fr)
    #align(center)[
      #image("/assets/mark.svg", width: 15mm)
      #v(9mm)
      #text(font: hfont, size: 26pt, weight: "bold", fill: paper, hyphenate: false)[#it.body]
    ]
    #v(1fr)
  ]
}

// Level 2 = chapter: new page, strong title, accent rule.
#show heading.where(level: 2): it => {
  pagebreak(weak: true)
  block(below: 1.4em)[
    #place(top + left, hide(it))
    #text(font: hfont, size: 19pt, weight: "bold", fill: ink, hyphenate: false)[#it.body]
    #v(4pt)
    #line(length: 30mm, stroke: 1.5pt + accent)
  ]
}

#show heading.where(level: 3): it => {
  block(above: 1.5em, below: 0.8em)[
    #set text(font: hfont, weight: "medium", size: 12pt, fill: ink, hyphenate: false)
    #it
  ]
}
#show heading.where(level: 4): it => {
  block(above: 1.3em, below: 0.7em)[
    #set text(font: hfont, weight: "medium", size: 10.5pt, fill: muted, hyphenate: false)
    #it
  ]
}

// Inline code: breakable after separator runs via zero-width spaces, so
// `::` and `--` split after the pair, never inside it.
#show raw.where(block: false): it => {
  let t = it.text
  for ch in ("/", "_", ".", "-", ":") {
    t = t.replace(regex("[" + ch + "]+"), m => m.text + "\u{200B}")
  }
  highlight(
    fill: codefill,
    extent: 1.2pt,
    radius: 2pt,
    top-edge: 0.9em,
    bottom-edge: -0.15em,
  )[#text(font: mfont, size: 9pt, fill: rgb("#3c4046"))[#t]]
}

// Code blocks: grey rounded panel; every grapheme gets a break point so
// long lines wrap instead of overflowing.
#show raw.where(block: true): it => {
  set text(font: mfont, size: 8.5pt)
  set par(leading: 0.55em, justify: false)
  let t = it.text.clusters().map(c => c + "\u{200B}").join()
  block(
    fill: codefill,
    inset: 9pt,
    radius: 4pt,
    width: 100%,
    above: 1.1em,
    below: 1.1em,
  )[#t]
}

#show link: it => {
  if type(it.dest) == str {
    text(fill: accent)[#it.body]
  } else {
    it
  }
}

#show quote.where(block: true): it => {
  block(
    stroke: (left: 2pt + hairline),
    inset: (left: 10pt, y: 2pt),
    above: 1.1em,
    below: 1.1em,
    text(fill: rgb("#4b4e53"), style: "italic")[#it.body],
  )
}

// Fenced-div callouts, emitted by filters/callouts.lua.
#let callout-style(kind) = (
  decision: (label: "Decision", bar: accent, fill: rgb("#eff3f6")),
  risk: (label: "What could break", bar: rgb("#a8593f"), fill: rgb("#f9f1ef")),
  gate: (label: "Gate", bar: rgb("#67795f"), fill: rgb("#f0f3ee")),
  note: (label: "Note", bar: rgb("#9a9da3"), fill: rgb("#f5f6f7")),
  principle: (label: "Principle", bar: ink, fill: rgb("#f2f2f3")),
).at(kind)

#let callout(kind: "note", body) = {
  let s = callout-style(kind)
  block(
    fill: s.fill,
    stroke: (left: 2.2pt + s.bar),
    radius: (top-right: 3pt, bottom-right: 3pt),
    inset: (left: 10pt, right: 10pt, y: 8pt),
    width: 100%,
    above: 1.1em,
    below: 1.1em,
  )[
    #text(font: hfont, size: 8pt, weight: "bold", tracking: 0.1em, fill: s.bar)[#upper(s.label)]
    #v(3pt)
    #body
  ]
}

// Tables, emitted by filters/tables.lua. Fractional columns sized from
// content, repeated header row, zebra striping, breakable across pages.
#let roadmap-table(cols: (), header: (), cells: (), caption: none) = {
  block(breakable: true, above: 1.2em, below: 1.2em)[
    #set text(size: 8.5pt)
    #set par(justify: false, leading: 0.55em)
    #table(
      columns: cols.map(w => w * 1fr),
      align: left,
      inset: (x: 6pt, y: 4pt),
      stroke: none,
      fill: (_, y) => if y == 0 { headfill } else if calc.rem(y, 2) == 0 { zebra },
      table.header(repeat: true, ..header.map(h => [#strong[#h]])),
      table.hline(stroke: 0.5pt + hairline),
      ..cells
    )
    #if caption != none [
      #v(5pt)
      #text(size: 8.5pt, style: "italic", fill: muted)[#caption]
    ]
  ]
}

$for(header-includes)$
$header-includes$
$endfor$

// Cover.
#page(fill: ink, margin: 0mm, header: none, footer: none)[
  #place(center + horizon)[
    #align(center)[
      #image("/assets/mark.svg", width: 24mm)
      #v(14mm)
      #text(font: hfont, size: 31pt, weight: "bold", fill: paper)[$title$]
      #v(8mm)
      #block(width: 120mm)[
        #set par(justify: false)
        #text(font: "Iowan Old Style", size: 12.5pt, fill: rgb("#c3c6cb"))[$subtitle$]
      ]
      #v(18mm)
      #line(length: 26mm, stroke: 0.6pt + rgb("#565a5f"))
      #v(7mm)
      #text(font: hfont, size: 9.5pt, tracking: 0.2em, fill: rgb("#92959b"))[#upper[$version$]#h(1.2em)·#h(1.2em)#upper[$date$]]
    ]
  ]
]

// Contents, parts + chapters.
#pagebreak()
#block(width: 100%)[
  #text(font: hfont, size: 19pt, weight: "bold", fill: ink)[Contents]
  #v(4pt)
  #line(length: 30mm, stroke: 1.5pt + accent)
  #v(1.5em)
  #show outline.entry.where(level: 1): set text(font: hfont, size: 10.5pt, weight: "medium")
  #show outline.entry.where(level: 2): set text(size: 10pt)
  #show outline.entry: set block(above: 0.6em)
  #outline(title: none, depth: 2)
]

$body$
