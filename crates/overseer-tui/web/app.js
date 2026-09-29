// overseer web surface — renders the ratatui buffer streamed over SSE.
// Layout math is server-side (same draw code as the terminal); this is
// a dumb grid renderer + input forwarder.

// ── auth ───────────────────────────────────────────────────────────
// The token arrives in the URL fragment (`#t=`): fragments never ride
// a request line or a Referer header. We lift it into localStorage
// (a returning tab keeps working) and strip it from the address bar.
let TOKEN = '';
const hashT = location.hash.match(/[#&]t=([0-9a-f]+)/);
if (hashT) {
  TOKEN = hashT[1];
  localStorage.setItem('overseer.token', TOKEN);
  history.replaceState(null, '', location.pathname + location.search);
} else {
  TOKEN = localStorage.getItem('overseer.token') || '';
}

const screen = document.getElementById('screen');
const cursor = document.createElement('div');
cursor.id = 'cursor';
screen.appendChild(cursor);

function locked() {
  document.getElementById('lock').classList.add('on');
}

// ── cell metrics ──────────────────────────────────────────────────
let CH = 8, LH = 18;
function measure() {
  const s = document.createElement('span');
  s.style.cssText = 'position:absolute;visibility:hidden;white-space:pre;';
  s.textContent = 'M'.repeat(20);
  screen.appendChild(s);
  CH = s.getBoundingClientRect().width / 20;
  LH = s.getBoundingClientRect().height;
  s.remove();
}

// ── xterm 256 palette ──────────────────────────────────────────────
const PAL = [];
(function () {
  const base = ['#000000','#cd0000','#00cd00','#cdcd00','#0000ee','#cd00cd','#00cdcd','#e5e5e5',
                '#7f7f7f','#ff0000','#00ff00','#ffff00','#5c5cff','#ff00ff','#00ffff','#ffffff'];
  for (let i = 0; i < 16; i++) PAL[i] = base[i];
  const lv = n => n === 0 ? 0 : 55 + 40 * n;
  for (let i = 0; i < 216; i++) {
    const r = lv(Math.floor(i / 36)), g = lv(Math.floor(i / 6) % 6), b = lv(i % 6);
    PAL[16 + i] = '#' + [r, g, b].map(v => v.toString(16).padStart(2, '0')).join('');
  }
  for (let i = 0; i < 24; i++) {
    const v = (8 + 10 * i).toString(16).padStart(2, '0');
    PAL[232 + i] = '#' + v + v + v;
  }
})();
const col = c => c && c[0] === 'i' ? PAL[+c.slice(1)] : c;

// ratatui Modifier bits
const M_BOLD = 1, M_DIM = 2, M_ITALIC = 4, M_UNDER = 8, M_REV = 64, M_CROSS = 256;

// One span per styled run — DOM APIs + CSSOM writes only (never
// innerHTML / setAttribute('style')), so `style-src 'self'` holds.
function applySpan(el, s) {
  let f = col(s.f), b = col(s.b);
  if (s.m & M_REV) [f, b] = [b || '#1e1f24', f || '#d4d6db'];
  if (f) el.style.color = f;
  if (b) el.style.background = b;
  if (s.m & M_BOLD) el.style.fontWeight = 'bold';
  if (s.m & M_DIM) el.style.opacity = '.55';
  if (s.m & M_ITALIC) el.style.fontStyle = 'italic';
  const td = [];
  if (s.m & M_UNDER) td.push('underline');
  if (s.m & M_CROSS) td.push('line-through');
  if (td.length) el.style.textDecoration = td.join(' ');
}

// Whole-grid vertical nudge inside the window (tuning pass).
const TOP_OFF = 10.5;
// Prompt rows sit a further 6px low — visual breathing room above
// the (now chromeless) footer row.
const PROMPT_OFF = 6;
// …and 3px right, so the ❯'s left edge meets the divider's left
// inset (left:3px on #divider).
const PROMPT_L = 3;

const divider = document.createElement('div');
divider.id = 'divider';
screen.appendChild(divider);

// ── frame rendering ────────────────────────────────────────────────
const rowEls = [];
const rowCache = [];

function render(f) {
  if (f.bye) { document.getElementById('bye').classList.add('on'); es.close(); return; }
  // §5: the server flags a fresh session — overlay the real mark.
  document.getElementById('empty').classList.toggle('on', !!f.e);
  screen.style.width = (f.w * CH) + 'px';
  screen.style.height = (f.h * LH) + 'px';
  while (rowEls.length < f.h) {
    const d = document.createElement('div');
    d.className = 'row';
    screen.insertBefore(d, cursor);
    rowEls.push(d);
  }
  // Rows past the frame height are ghosts from a taller grid — hide
  // them and drop their cache keys so a shrink never leaves stale cells.
  for (let y = f.h; y < rowEls.length; y++) {
    rowEls[y].style.display = 'none';
    rowCache[y] = null;
  }
  // f.p = the prompt band's first grid row — the server reports it so
  // the dip+divider track the layout whether or not the panel band is
  // open below it.
  const pt = f.p ?? f.h - 3;
  const frag = document.createDocumentFragment();
  for (let y = 0; y < f.h; y++) {
    const row = f.rows[y];
    const el = rowEls[y];
    const inPrompt = y >= pt && y < pt + 2;
    // Always reposition — resize shifts which band a row belongs to
    // even when its content is identical.
    el.style.display = '';
    el.style.top = (y * LH + TOP_OFF + (inPrompt ? PROMPT_OFF : 0)) + 'px';
    el.style.left = (inPrompt ? PROMPT_L : 0) + 'px';
    const key = JSON.stringify(row);
    if (rowCache[y] === key) continue;
    rowCache[y] = key;
    el.style.height = LH + 'px';
    for (const s of row) {
      const sp = document.createElement('span');
      sp.textContent = s.t;
      applySpan(sp, s);
      frag.appendChild(sp);
    }
    el.replaceChildren(frag);
  }
  // 1px separator 3px above the prompt arrow, inset 3px each side.
  divider.style.top = (pt * LH + TOP_OFF + PROMPT_OFF - 4) + 'px';
  const curInPrompt = f.cur[1] >= pt && f.cur[1] < pt + 2;
  cursor.style.left = (f.cur[0] * CH + (curInPrompt ? PROMPT_L : 0)) + 'px';
  cursor.style.top = (f.cur[1] * LH + TOP_OFF + (curInPrompt ? PROMPT_OFF : 0)) + 'px';
  cursor.style.width = CH + 'px';
  cursor.style.height = LH + 'px';
}

// ── server link ────────────────────────────────────────────────────
let es;
// POST with the bearer header first; some preview proxies forward
// POSTs but drop bodies, so a failed POST falls back to GET ?d=&t= —
// the token in a URL is acceptable here (fetch, not navigation, and
// Referrer-Policy: no-referrer keeps it out of Referer headers).
const post = o => {
  const body = JSON.stringify(o);
  fetch('/input', {
    method: 'POST',
    headers: { 'X-Overseer-Token': TOKEN },
    body,
  }).catch(() =>
    fetch('/input?t=' + encodeURIComponent(TOKEN) + '&d=' + encodeURIComponent(body))
  );
};

// §6 installed PWA is full-bleed: no title bar, no page margin.
const standalone = matchMedia('(display-mode: standalone)').matches || navigator.standalone === true;

function fit() {
  const cols = Math.max(40, Math.floor((innerWidth - (standalone ? 0 : 20)) / CH));
  const rows = Math.max(10, Math.floor((innerHeight - (standalone ? 0 : 30 + 20)) / LH));
  post({ type: 'resize', cols, rows });
}

async function connect() {
  if (!TOKEN) { locked(); return; }
  // EventSource can't set headers or report a 401 — probe the route
  // with fetch first so a bad/expired token lands on the locked screen
  // instead of an invisible reconnect loop.
  try {
    const probe = await fetch('/events?t=' + encodeURIComponent(TOKEN));
    if (probe.status === 401 || probe.status === 403) { probe.body.cancel(); locked(); return; }
    await probe.body.cancel();
  } catch {
    locked();
    return;
  }
  es = new EventSource('/events?t=' + encodeURIComponent(TOKEN));
  es.onmessage = e => render(JSON.parse(e.data));
  fit();
  // Re-fit once layout/fonts settle — first measure can run before
  // the preview iframe reaches its final size.
  requestAnimationFrame(fit);
  if (document.fonts && document.fonts.ready) document.fonts.ready.then(fit);
}

// ── input ──────────────────────────────────────────────────────────
const KEYS = {
  Enter: 'enter', Escape: 'esc', Backspace: 'backspace', Delete: 'delete',
  Tab: 'tab', ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left',
  ArrowRight: 'right', Home: 'home', End: 'end', PageUp: 'pageup',
  PageDown: 'pagedown', F1: 'f1',
};

// The bottom mark is the panel button — same as F1 / ↑ on empty input.
// §10 swap point: /mark.svg is the ONE geometry file; it's injected
// inline (never innerHTML strings we build) into the button and the
// empty-state mark so CSS currentColor controls each copy.
const markBtn = document.getElementById('mark');
fetch('/mark.svg')
  .then(r => r.text())
  .then(svg => {
    markBtn.innerHTML = svg;
    const em = document.getElementById('emark');
    if (em) em.innerHTML = svg;
  })
  .catch(() => {});
markBtn.addEventListener('click', e => {
  post({ type: 'panel' });
  e.stopPropagation();
});

addEventListener('keydown', e => {
  // Focused on the mark button? Enter/Space belongs to it — let the
  // browser produce the click instead of forwarding a transcript key.
  if (e.target === markBtn) return;
  if (e.metaKey) return; // leave cmd-* to the browser
  const mods = { ctrl: e.ctrlKey, alt: e.altKey, shift: e.shiftKey };
  if (e.key === 'Tab' && e.shiftKey) {
    post({ type: 'key', code: 'backtab', ctrl: e.ctrlKey, alt: e.altKey, shift: false });
    e.preventDefault();
    return;
  }
  const code = KEYS[e.key];
  if (code) {
    post({ type: 'key', code, ...mods });
    e.preventDefault();
  } else if (e.key.length === 1) {
    post({ type: 'key', code: 'char', ch: e.key, ...mods });
    e.preventDefault();
  }
});

// Clicks → grid coords → the same hit regions the terminal uses.
screen.addEventListener('click', e => {
  const r = screen.getBoundingClientRect();
  const row = Math.floor((e.clientY - r.top - TOP_OFF) / LH);
  const col = Math.floor((e.clientX - r.left) / CH);
  if (row >= 0 && col >= 0) post({ type: 'click', col, row });
});

addEventListener('paste', e => {
  const t = e.clipboardData.getData('text');
  if (t) { post({ type: 'paste', text: t }); e.preventDefault(); }
});

let wheelAcc = 0;
addEventListener('wheel', e => {
  wheelAcc += e.deltaY;
  while (Math.abs(wheelAcc) > LH) {
    post({ type: 'scroll', up: wheelAcc < 0 });
    wheelAcc += wheelAcc < 0 ? LH : -LH;
  }
}, { passive: true });

addEventListener('resize', fit);
addEventListener('focus', () => {
  document.body.classList.remove('blur');
  post({ type: 'focus', gained: true });
});
addEventListener('blur', () => {
  document.body.classList.add('blur');
  post({ type: 'focus', gained: false });
});
// A click anywhere reclaims focus — the screen element is tabindex=0
// so this gives the document a real focus target in iframes.
addEventListener('mousedown', () => screen.focus());
if (!document.hasFocus()) document.body.classList.add('blur');
addEventListener('load', () => screen.focus());

measure();
connect();
