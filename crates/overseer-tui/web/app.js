// overseer web surface — renders the ratatui buffer streamed over SSE.
// Layout math is server-side (same draw code as the terminal); this is
// a dumb grid renderer + input forwarder.

const screen = document.getElementById('screen');
const cursor = document.createElement('div');
cursor.id = 'cursor';
screen.appendChild(cursor);

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

function spanCss(s) {
  let f = col(s.f), b = col(s.b);
  let css = '';
  if (s.m & M_REV) [f, b] = [b || '#1e1f24', f || '#abb2bf'];
  if (f) css += 'color:' + f + ';';
  if (b) css += 'background:' + b + ';';
  if (s.m & M_BOLD) css += 'font-weight:bold;';
  if (s.m & M_DIM) css += 'opacity:.55;';
  if (s.m & M_ITALIC) css += 'font-style:italic;';
  let td = [];
  if (s.m & M_UNDER) td.push('underline');
  if (s.m & M_CROSS) td.push('line-through');
  if (td.length) css += 'text-decoration:' + td.join(' ') + ';';
  return css;
}

const esc = t => t.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

// ── frame rendering ────────────────────────────────────────────────
const rowEls = [];
const rowCache = [];

function render(f) {
  if (f.bye) { document.getElementById('bye').classList.add('on'); es.close(); return; }
  screen.style.width = (f.w * CH) + 'px';
  screen.style.height = (f.h * LH) + 'px';
  while (rowEls.length < f.h) {
    const d = document.createElement('div');
    d.className = 'row';
    screen.insertBefore(d, cursor);
    rowEls.push(d);
  }
  for (let y = 0; y < f.h; y++) {
    const row = f.rows[y];
    const key = JSON.stringify(row);
    if (rowCache[y] === key) continue;
    rowCache[y] = key;
    const el = rowEls[y];
    el.style.top = (y * LH) + 'px';
    el.style.height = LH + 'px';
    let html = '';
    for (const s of row) html += '<span style="' + spanCss(s) + '">' + esc(s.t) + '</span>';
    el.innerHTML = html;
  }
  cursor.style.left = (f.cur[0] * CH) + 'px';
  cursor.style.top = (f.cur[1] * LH) + 'px';
  cursor.style.width = CH + 'px';
  cursor.style.height = LH + 'px';
}

// ── server link ────────────────────────────────────────────────────
let es;
const post = o => fetch('/input', { method: 'POST', body: JSON.stringify(o) });

function fit() {
  const cols = Math.max(40, Math.floor((innerWidth * 0.94) / CH));
  const rows = Math.max(10, Math.floor((innerHeight * 0.88 - 30) / LH));
  post({ type: 'resize', cols, rows });
}

function connect() {
  es = new EventSource('/events');
  es.onmessage = e => render(JSON.parse(e.data));
  fit();
}

// ── input ──────────────────────────────────────────────────────────
const KEYS = {
  Enter: 'enter', Escape: 'esc', Backspace: 'backspace', Delete: 'delete',
  Tab: 'tab', ArrowUp: 'up', ArrowDown: 'down', ArrowLeft: 'left',
  ArrowRight: 'right', Home: 'home', End: 'end', PageUp: 'pageup',
  PageDown: 'pagedown',
};

addEventListener('keydown', e => {
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
addEventListener('focus', () => post({ type: 'focus', gained: true }));
addEventListener('blur', () => post({ type: 'focus', gained: false }));

measure();
connect();
