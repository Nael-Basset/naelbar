import { initializeApp } from 'https://www.gstatic.com/firebasejs/10.14.1/firebase-app.js';
import { getAuth, GoogleAuthProvider, signInWithPopup, signInWithRedirect, getRedirectResult, onAuthStateChanged, signOut, setPersistence, browserLocalPersistence } from 'https://www.gstatic.com/firebasejs/10.14.1/firebase-auth.js';
import { initializeFirestore, persistentLocalCache, persistentMultipleTabManager, collection, doc, setDoc, getDoc, getDocs, onSnapshot, writeBatch } from 'https://www.gstatic.com/firebasejs/10.14.1/firebase-firestore.js';
import { startPc } from './pc.js?v=14';
import { startMusic } from './music.js?v=14';

const app = initializeApp({
  apiKey: 'AIzaSyBvW_D77a2k3moCWtZMEREDh9dFdGXG0mM',
  authDomain: 'naelbar-agenda-eadafc.web.app',
  projectId: 'naelbar-agenda-eadafc',
  storageBucket: 'naelbar-agenda-eadafc.firebasestorage.app',
  messagingSenderId: '778992065062',
  appId: '1:778992065062:web:4fae63ce0cd8d7771364e7'
});
const auth = getAuth(app);
// La connexion reste sur l'appareil : une seule fois.
setPersistence(auth, browserLocalPersistence).catch(() => {});
const db = initializeFirestore(app, { localCache: persistentLocalCache({ tabManager: persistentMultipleTabManager() }) });

const $ = id => document.getElementById(id);
const MONTHS = ['janvier','février','mars','avril','mai','juin','juillet','août','septembre','octobre','novembre','décembre'];
const DOW = ['lun.','mar.','mer.','jeu.','ven.','sam.','dim.'];
const COLORS = ['#E5484D','#F59E42','#FFC53D','#4ADE80','#4C9BFF','#A78BFA','#F472B6'];
const HEX = /^#[0-9A-Fa-f]{6}$/;
const pad = n => String(n).padStart(2, '0');
const iso = d => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const fromIso = s => { const [y, m, d] = s.split('-').map(Number); return new Date(y, m - 1, d); };
const addDays = (d, n) => { const x = new Date(d); x.setDate(x.getDate() + n); return x; };
const monday = d => addDays(d, -((d.getDay() + 6) % 7));
const mins = t => t ? Number(t.slice(0, 2)) * 60 + Number(t.slice(3, 5)) : null;
const rid = () => Array.from(crypto.getRandomValues(new Uint8Array(16)), b => pad(b.toString(16))).join('');
const el = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text !== undefined) e.textContent = text; return e; };
const params = new URLSearchParams(location.search);
const DESKTOP_APP = params.get('desktop') === '1';
const PC_MODE = params.get('pc') === '1';
// Journal pour les fenêtres de naelbar (aucun identifiant, aucun contenu).
const nlog = m => { try { window.chrome?.webview?.postMessage({ naelbarLog: String(m) }); } catch (_) {} };

let user = null, unsubs = [];
let events = [], notes = [], courses = [], styles = [], coursesAt = 0;
let view = localStorage.getItem('nb.view') || (matchMedia('(min-width:620px)').matches ? 'week' : 'day');
let anchor = new Date(); anchor.setHours(0, 0, 0, 0);
let showHidden = false, selectedKey = null;

const col = name => collection(db, 'users', user.uid, name);
const metaDoc = name => doc(db, 'users', user.uid, 'meta', name);

// ---------------------------------------------------------------- Styles des cours
// Même règle que naelbar : le style du cours lui-même, sinon celui de sa matière.
function styleOf(c) {
  const t = c.t || '';
  return styles.find(s => s.title === c.ti && s.date === c.d && (s.time || '') === t)
    || styles.find(s => s.title === c.ti && !s.date) || null;
}
const colorOf = c => { const s = styleOf(c); return s && HEX.test(s.color || '') ? s.color : null; };
const hiddenOf = c => !!styleOf(c)?.hidden;
function applyStyle(c, all, color, hidden) {
  const t = c.t || '';
  const next = { ...(styleOf(c) || { color: '', hidden: false }), title: c.ti };
  if (color !== undefined) next.color = color;
  if (hidden !== undefined) next.hidden = hidden;
  if (all) {
    styles = styles.filter(s => s.title !== c.ti);
    if (next.hidden || next.color) styles.push({ title: c.ti, date: '', time: '', color: next.color || '', hidden: !!next.hidden });
  } else {
    styles = styles.filter(s => !(s.title === c.ti && s.date === c.d && (s.time || '') === t));
    const subj = styles.find(s => s.title === c.ti && !s.date) || { color: '', hidden: false };
    if ((next.color || '') !== (subj.color || '') || !!next.hidden !== !!subj.hidden) styles.push({ title: c.ti, date: c.d, time: t, color: next.color || '', hidden: !!next.hidden });
  }
  render();
  return setDoc(metaDoc('styles'), { items: styles, updated_at_ms: Date.now() });
}

// ---------------------------------------------------------------- Ce qu'il y a un jour donné
function itemsOf(date) {
  const out = [];
  for (const e of events) if (!e.deleted && e.date === date) out.push({ kind: 'local', key: 'e' + e.id, title: e.title, start: e.time, end: e.end, ev: e });
  for (const c of courses) {
    if (c.d !== date) continue;
    const hidden = hiddenOf(c);
    if (hidden && !showHidden) continue;
    const project = c.k === 'RN';
    out.push({ kind: 'course', key: `c${c.d}${c.t}${c.ti}`, title: c.ti, start: c.t, end: c.e, sub: project ? 'heures projet' : [c.k, c.p].filter(Boolean).join(' · '), color: colorOf(c), project, hidden, c });
  }
  return out.sort((a, b) => (a.start || '').localeCompare(b.start || '') || a.title.localeCompare(b.title));
}

// ---------------------------------------------------------------- Rendu
function setTitle() {
  const a = anchor;
  let t;
  if (view === 'month') t = `${MONTHS[a.getMonth()]} ${a.getFullYear()}`;
  else if (view === 'week') { const m = monday(a), s = addDays(m, 6); t = m.getMonth() === s.getMonth() ? `${m.getDate()} – ${s.getDate()} ${MONTHS[s.getMonth()]}` : `${m.getDate()} ${MONTHS[m.getMonth()].slice(0, 4)}. – ${s.getDate()} ${MONTHS[s.getMonth()].slice(0, 4)}.`; }
  else t = a.toLocaleDateString('fr-FR', { weekday: 'long', day: 'numeric', month: 'long' });
  $('period').textContent = t;
  document.querySelectorAll('.seg button[data-view]').forEach(b => b.classList.toggle('on', b.dataset.view === view));
}

function evBlock(it, compact) {
  const b = el('button', 'ev' + (it.kind === 'local' ? ' local' : '') + (it.project ? ' project' : '') + (it.hidden ? ' hidden' : '') + (it.key === selectedKey ? ' sel' : ''));
  b.type = 'button';
  if (it.color && !it.hidden) { b.style.borderLeftColor = it.color; b.style.background = it.color + '33'; }
  b.append(el('b', '', it.title));
  if (!compact) b.append(el('small', '', [it.start && (it.end ? `${it.start}–${it.end}` : it.start), it.hidden ? 'masqué' : it.sub].filter(Boolean).join(' · ')));
  b.setAttribute('aria-label', `${it.title}${it.start ? ', ' + it.start : ''}`);
  b.onclick = ev => { ev.stopPropagation(); tapItem(it); };
  return b;
}

function renderMonth(v) {
  const g = el('div', 'month');
  DOW.forEach(d => g.append(el('div', 'dow', d.slice(0, 1).toUpperCase())));
  const first = new Date(anchor.getFullYear(), anchor.getMonth(), 1);
  const start = monday(first), today = iso(new Date());
  for (let i = 0; i < 42; i++) {
    const d = addDays(start, i), s = iso(d);
    const cell = el('button', 'cell' + (d.getMonth() !== anchor.getMonth() ? ' other' : '') + (s === today ? ' today' : ''));
    cell.type = 'button';
    cell.append(el('span', 'num', String(d.getDate())));
    const items = itemsOf(s), shown = items.slice(0, 3);
    for (const it of shown) {
      const c = el('span', 'chip', it.title);
      if (it.color && !it.hidden) c.style.background = it.color + '55';
      if (it.project || it.hidden) c.style.opacity = '.5';
      cell.append(c);
    }
    if (items.length > shown.length) cell.append(el('span', 'more', `+${items.length - shown.length}`));
    cell.setAttribute('aria-label', d.toLocaleDateString('fr-FR', { weekday: 'long', day: 'numeric', month: 'long' }) + `, ${items.length} élément(s)`);
    cell.onclick = () => { anchor = d; setView('day'); };
    g.append(cell);
  }
  v.append(g);
}

const HOUR = 46;
function renderGrid(v, days) {
  const all = days.map(d => itemsOf(iso(d)));
  // Plage d'heures affichée : 7 h – 20 h au moins, élargie si besoin.
  let lo = 7 * 60, hi = 20 * 60;
  all.flat().forEach(it => { const s = mins(it.start); if (s == null) return; const e = mins(it.end) ?? s + 60; lo = Math.min(lo, Math.floor(s / 60) * 60); hi = Math.max(hi, Math.ceil(e / 60) * 60); });
  const g = el('div', 'grid' + (days.length === 1 ? ' day' : ''));
  g.style.setProperty('--cols', days.length);
  const today = iso(new Date());
  g.append(el('div', 'head'));
  days.forEach(d => {
    const h = el('div', 'head' + (iso(d) === today ? ' today' : ''));
    h.append(DOW[(d.getDay() + 6) % 7], el('b', '', String(d.getDate())));
    if (days.length > 1) { h.style.cursor = 'pointer'; h.onclick = () => { anchor = d; setView('day'); }; }
    g.append(h);
  });
  // Événements sans heure.
  const untimed = all.map(list => list.filter(it => !it.start));
  if (untimed.some(l => l.length)) {
    const strip = el('div', 'allday');
    strip.style.gridColumn = '1 / -1';
    untimed.forEach((list, i) => list.forEach(it => {
      const b = evBlock(it, true); b.style.position = 'static';
      if (days.length > 1) b.prepend(el('small', '', DOW[(days[i].getDay() + 6) % 7]));
      strip.append(b);
    }));
    g.append(strip);
  }
  const height = (hi - lo) / 60 * HOUR;
  const hours = el('div', 'hours'); hours.style.height = height + 'px';
  for (let m = lo + 60; m < hi; m += 60) { const t = el('span', 'h', `${m / 60}h`); t.style.top = (m - lo) / 60 * HOUR + 'px'; hours.append(t); }
  g.append(hours);
  const now = new Date(), nowMin = now.getHours() * 60 + now.getMinutes();
  days.forEach((d, i) => {
    const s = iso(d);
    const c = el('div', 'col' + (s === today ? ' today' : '')); c.style.height = height + 'px';
    for (let m = lo; m < hi; m += 60) { const l = el('div', 'line'); l.style.top = (m - lo) / 60 * HOUR + 'px'; c.append(l); }
    // Appui dans le vide : nouvel événement à cette demi-heure.
    c.onclick = ev => { if (ev.target !== c && !ev.target.classList.contains('line')) return; const r = c.getBoundingClientRect(); const m = lo + Math.floor((ev.clientY - r.top) / HOUR * 2) * 30; openEditor({ date: s, time: `${pad(Math.floor(m / 60))}:${pad(m % 60)}` }); };
    const timed = all[i].filter(it => it.start);
    // Colonnes pour les chevauchements.
    const lanes = [];
    const placed = timed.map(it => { const a = mins(it.start), b = Math.max(mins(it.end) ?? a + 60, a + 20); let k = lanes.findIndex(end => end <= a); if (k < 0) { k = lanes.length; lanes.push(b); } else lanes[k] = b; return { it, a, b, k }; });
    placed.forEach(p => {
      const n = Math.max(1, ...placed.filter(q => q.a < p.b && p.a < q.b).map(q => q.k + 1));
      const blk = evBlock(p.it, (p.b - p.a) < 40 || (days.length > 3 && (p.b - p.a) < 60));
      blk.style.top = (p.a - lo) / 60 * HOUR + 1 + 'px';
      blk.style.height = Math.max(18, (p.b - p.a) / 60 * HOUR - 2) + 'px';
      blk.style.left = `calc(${p.k / n * 100}% + 1px)`;
      blk.style.width = `calc(${100 / n}% - 2px)`;
      c.append(blk);
    });
    if (s === today && nowMin >= lo && nowMin <= hi) { const n = el('div', 'now'); n.style.top = (nowMin - lo) / 60 * HOUR + 'px'; c.append(n); }
    g.append(c);
  });
  v.append(g);
}

function render() {
  if (!user) return;
  setTitle();
  const v = $('view'); v.replaceChildren();
  if (view === 'month') renderMonth(v);
  else if (view === 'week') { const m = monday(anchor); renderGrid(v, [0, 1, 2, 3, 4, 5, 6].map(i => addDays(m, i))); }
  else renderGrid(v, [anchor]);
  const bar = document.querySelector('#tab-agenda .bar');
  document.documentElement.style.setProperty('--bar-h', (bar?.offsetHeight || 98) + 'px');
  const meta = $('meta'); meta.replaceChildren();
  if (coursesAt) meta.append(`Cours de la fac mis à jour ${new Date(coursesAt).toLocaleString('fr-FR', { weekday: 'short', day: 'numeric', month: 'short', hour: '2-digit', minute: '2-digit' })}, quand le PC est allumé. `);
  if (styles.some(s => s.hidden)) {
    const b = el('button', 'ghost', showHidden ? 'Cacher les cours masqués' : 'Voir les cours masqués');
    b.style.minHeight = '32px'; b.style.marginTop = '6px';
    b.onclick = () => { showHidden = !showHidden; render(); };
    meta.append(b);
  }
  renderNotes();
}

function renderNotes() {
  const ul = $('notes'); ul.replaceChildren();
  const list = notes.filter(n => !n.deleted).sort((a, b) => (a.done - b.done) || (b.updated_at_ms - a.updated_at_ms));
  if (!list.length) ul.append(el('li', 'empty', 'Aucun post-it.'));
  for (const n of list) {
    const li = el('li', n.done ? 'done' : '');
    const c = el('input'); c.type = 'checkbox'; c.checked = n.done; c.setAttribute('aria-label', 'Terminé');
    c.onchange = () => saveNote({ ...n, done: c.checked });
    const x = el('button', 'x', '×'); x.setAttribute('aria-label', 'Supprimer');
    x.onclick = () => saveNote({ ...n, deleted: true });
    li.append(c, el('span', 'txt', n.text), x); ul.append(li);
  }
}

// ---------------------------------------------------------------- Appuis
let lastTap = { key: null, at: 0 };
function tapItem(it) {
  const now = Date.now();
  const double = lastTap.key === it.key && now - lastTap.at < 450;
  lastTap = double ? { key: null, at: 0 } : { key: it.key, at: now };
  selectedKey = it.key;
  if (it.kind === 'local') { openEditor(it.ev); return; }
  if (double) openStyle(it); else openCourse(it);
}

// ---------------------------------------------------------------- Feuilles
function closeSheet() { $('sheet').hidden = true; $('sheet-bg').hidden = true; selectedKey = null; render(); }
function sheet(build) {
  const s = $('sheet'); s.replaceChildren(el('div', 'grab'));
  build(s);
  s.hidden = false; $('sheet-bg').hidden = false;
}
$('sheet-bg').onclick = closeSheet;
document.addEventListener('keydown', e => { if (e.key === 'Escape' && !$('sheet').hidden) closeSheet(); });

function field(parent, label, input) { parent.append(el('label', '', label), input); return input; }
function inp(type, value, attrs = {}) { const i = el('input'); i.type = type; if (value) i.value = value; Object.assign(i, attrs); return i; }

function openEditor(e) {
  const existing = !!e.id;
  sheet(s => {
    s.append(el('h2', '', existing ? 'Modifier' : 'Nouvel événement'));
    const form = el('form');
    const title = field(form, 'Titre', inp('text', e.title, { maxLength: 200, required: true, placeholder: 'Par exemple Réunion projet' }));
    const date = field(form, 'Date', inp('date', e.date || iso(anchor), { required: true }));
    const two = el('div', 'two'), a = el('div'), b = el('div');
    const start = field(a, 'Début', inp('time', e.time || ''));
    const end = field(b, 'Fin', inp('time', e.end || ''));
    two.append(a, b); form.append(two);
    const err = el('p', 'err'); form.append(err);
    const actions = el('div', 'actions');
    if (existing) { const del = el('button', 'danger', 'Supprimer'); del.type = 'button'; del.onclick = () => { saveEvent({ ...e, deleted: true }); closeSheet(); }; actions.append(del); }
    const ok = el('button', 'primary', existing ? 'Enregistrer' : 'Ajouter'); ok.type = 'submit';
    actions.append(ok); form.append(actions);
    form.onsubmit = ev => {
      ev.preventDefault();
      const t = title.value.trim();
      if (!t || !date.value) return;
      if (end.value && !start.value) { err.textContent = 'Indique aussi l\'heure de début.'; return; }
      if (end.value && end.value <= start.value) { err.textContent = 'La fin doit suivre le début.'; return; }
      saveEvent({ id: e.id || rid(), title: t, date: date.value, time: start.value || null, end: end.value || null, deleted: false });
      closeSheet();
    };
    s.append(form);
    if (!existing) setTimeout(() => title.focus(), 50);
  });
}

function openCourse(it) {
  sheet(s => {
    s.append(el('h2', '', it.title));
    const when = fromIso(it.c.d).toLocaleDateString('fr-FR', { weekday: 'long', day: 'numeric', month: 'long' });
    s.append(el('p', 'sub', [when, it.start && (it.end ? `${it.start}–${it.end}` : it.start), it.sub].filter(Boolean).join(' · ')));
    s.append(el('p', 'sub', 'Cours de la fac, en lecture seule. Touche deux fois un cours pour choisir sa couleur.'));
    const actions = el('div', 'actions');
    const st = el('button', 'primary', 'Couleur et masquage'); st.onclick = () => openStyle(it);
    actions.append(st); s.append(actions);
  });
}

function openStyle(it) {
  let all = false;
  sheet(s => {
    s.append(el('h2', '', it.title));
    s.append(el('p', 'sub', 'Couleur et visibilité'));
    const scope = el('div', 'seg');
    const b1 = el('button', 'on', 'Ce cours'), b2 = el('button', '', `Tous « ${it.title.slice(0, 26)} »`);
    const setScope = v => { all = v; b1.classList.toggle('on', !v); b2.classList.toggle('on', v); };
    b1.onclick = () => setScope(false); b2.onclick = () => setScope(true);
    scope.append(b1, b2); s.append(scope);
    s.append(el('label', '', 'Couleur'));
    const sw = el('div', 'swatches');
    const current = colorOf(it.c) || '';
    ['', ...COLORS].forEach(c => {
      const b = el('button', 'sw' + (c ? '' : ' none') + (c.toLowerCase() === current.toLowerCase() ? ' on' : ''));
      b.setAttribute('aria-label', c ? `Couleur ${c}` : 'Aucune couleur');
      if (c) b.style.background = c;
      b.onclick = () => { applyStyle(it.c, all, c); closeSheet(); };
      sw.append(b);
    });
    s.append(sw);
    const actions = el('div', 'actions');
    const hide = el('button', 'ghost', it.hidden ? 'Réafficher' : 'Masquer');
    hide.style.flex = '1';
    hide.onclick = () => { applyStyle(it.c, all, undefined, !it.hidden); closeSheet(); };
    actions.append(hide); s.append(actions);
  });
}

// ---------------------------------------------------------------- Écritures
const EK = ['id', 'title', 'date', 'time', 'end', 'updated_at_ms', 'deleted'];
const NK = ['id', 'text', 'done', 'updated_at_ms', 'deleted'];
const clean = (o, keys) => Object.fromEntries(keys.map(k => [k, o[k] ?? null]));
function saveEvent(e) { const v = clean({ ...e, updated_at_ms: Date.now(), deleted: !!e.deleted }, EK); return setDoc(doc(col('events'), v.id), v); }
function saveNote(n) { const v = clean({ ...n, updated_at_ms: Date.now(), deleted: !!n.deleted, done: !!n.done }, NK); return setDoc(doc(col('notes'), v.id), v); }

// ---------------------------------------------------------------- Navigation
function setView(v) { view = v; localStorage.setItem('nb.view', v); render(); window.scrollTo(0, 0); }
document.querySelectorAll('.seg button[data-view]').forEach(b => b.onclick = () => setView(b.dataset.view));
const step = dir => { if (view === 'month') anchor = new Date(anchor.getFullYear(), anchor.getMonth() + dir, 1); else anchor = addDays(anchor, dir * (view === 'week' ? 7 : 1)); render(); };
$('prev').onclick = () => step(-1);
$('next').onclick = () => step(1);
$('today').onclick = () => { anchor = new Date(); anchor.setHours(0, 0, 0, 0); render(); };
$('add').onclick = () => openEditor({ date: iso(anchor) });
// Balayage gauche / droite pour changer de période.
let touchX = null, touchY = null;
$('view').addEventListener('touchstart', e => { touchX = e.touches[0].clientX; touchY = e.touches[0].clientY; }, { passive: true });
$('view').addEventListener('touchend', e => {
  if (touchX == null) return;
  const dx = e.changedTouches[0].clientX - touchX, dy = e.changedTouches[0].clientY - touchY; touchX = null;
  if (Math.abs(dx) > 70 && Math.abs(dx) > 2 * Math.abs(dy)) step(dx < 0 ? 1 : -1);
}, { passive: true });

document.querySelectorAll('.tabs button').forEach(b => b.onclick = () => {
  document.querySelectorAll('.tabs button').forEach(x => x.classList.toggle('on', x === b));
  for (const t of ['agenda', 'notes', 'music', 'pc']) $('tab-' + t).hidden = t !== b.dataset.tab;
  $('add').hidden = b.dataset.tab !== 'agenda';
  if (b.dataset.tab === 'music') music?.show(); else music?.hide();
});
$('note-form').onsubmit = ev => {
  ev.preventDefault();
  const text = $('note-text').value.trim(); if (!text) return;
  saveNote({ id: rid(), text, done: false, deleted: false });
  $('note-text').value = '';
};
$('btn-logout').onclick = () => signOut(auth);
// La ligne rouge de maintenant avance avec l'heure.
setInterval(() => { if (user && !PC_MODE && !DESKTOP_APP && view !== 'month' && $('sheet').hidden && !document.hidden) render(); }, 60_000);

// ---------------------------------------------------------------- Connexion
const isMobile = () => /Android|iPhone|iPad|iPod/i.test(navigator.userAgent) || matchMedia('(pointer:coarse)').matches;
nlog('page chargée, desktop=' + DESKTOP_APP + ', pc=' + PC_MODE);
$('btn-login').onclick = async () => {
  $('login-err').textContent = '';
  const p = new GoogleAuthProvider();
  // Laisse choisir le compte, même si un seul est déjà ouvert.
  p.setCustomParameters({ prompt: 'select_account' });
  try {
    if (isMobile() || DESKTOP_APP || PC_MODE) await signInWithRedirect(auth, p);
    else await signInWithPopup(auth, p);
  } catch (e) {
    if (e.code === 'auth/popup-blocked' || e.code === 'auth/operation-not-supported-in-this-environment') {
      try { await signInWithRedirect(auth, p); } catch (_) { $('login-err').textContent = 'Connexion impossible.'; }
    } else if (e.code !== 'auth/popup-closed-by-user' && e.code !== 'auth/cancelled-popup-request') {
      $('login-err').textContent = 'Connexion impossible.';
    }
  }
};
getRedirectResult(auth).then(r => nlog('résultat de redirection: ' + (r ? 'utilisateur reçu' : 'aucun'))).catch(e => { nlog('erreur de redirection: ' + (e && e.code)); $('login-err').textContent = 'Connexion impossible.'; });

const readDoc = d => d.data();
function listen() {
  unsubs.push(onSnapshot(col('events'), s => { events = s.docs.map(readDoc); render(); }));
  unsubs.push(onSnapshot(col('notes'), s => { notes = s.docs.map(readDoc); renderNotes(); }));
  unsubs.push(onSnapshot(metaDoc('courses'), s => { const d = s.data(); courses = Array.isArray(d?.items) ? d.items : []; coursesAt = d?.updated_at_ms || 0; render(); }));
  unsubs.push(onSnapshot(metaDoc('styles'), s => { const d = s.data(); styles = Array.isArray(d?.items) ? d.items : []; render(); }));
}

let resolveReady; const ready = new Promise(r => resolveReady = r);
let pcStarted = false, music = null;
onAuthStateChanged(auth, u => {
  nlog('état de connexion: ' + (u ? 'connecté' : 'déconnecté'));
  user = u;
  $('login').hidden = !!u; $('app').hidden = !u;
  unsubs.forEach(f => f()); unsubs = [];
  events = []; notes = []; courses = []; styles = [];
  if (u && !DESKTOP_APP) {
    if (!PC_MODE) { listen(); render(); if (!music) music = startMusic({ db, uid: u.uid, ui: { $, el } }); }
    if (!pcStarted) { pcStarted = true; startPc({ db, uid: u.uid, pcMode: PC_MODE, log: nlog, ui: { $, el } }); }
  }
  resolveReady();
});

// ---------------------------------------------------------------- Pont pour naelbar (synchro)
async function fetchAll() {
  const [e, n, st] = await Promise.all([getDocs(col('events')), getDocs(col('notes')), getDoc(metaDoc('styles'))]);
  return { events: e.docs.map(readDoc), notes: n.docs.map(readDoc), styles: st.data() || null };
}
window.naelbarSync = {
  ready: () => ready,
  signOut: () => signOut(auth),
  async pull() {
    await ready;
    if (!user) return { signedIn: false, events: [], notes: [] };
    return { signedIn: true, ...(await fetchAll()) };
  },
  async push({ events: ie = [], notes: inn = [], courses: ic = null, styles: is = null } = {}) {
    await ready;
    if (!user) throw new Error('not signed in');
    const cur = await fetchAll();
    const merge = (incoming, existing, keys, name) => {
      const m = new Map(existing.map(x => [x.id, x]));
      const out = [];
      for (const x of incoming) {
        if (!x || typeof x.id !== 'string' || !/^[0-9a-f]{1,64}$/.test(x.id)) continue;
        const old = m.get(x.id);
        if (!old || (Number(x.updated_at_ms) || 0) > (old.updated_at_ms || 0)) out.push(clean({ ...x, deleted: !!x.deleted }, keys));
      }
      return out.map(v => [doc(col(name), v.id), v]);
    };
    const ops = [...merge(ie, cur.events, EK, 'events'), ...merge(inn, cur.notes, NK, 'notes')];
    for (let i = 0; i < ops.length; i += 400) {
      const b = writeBatch(db);
      ops.slice(i, i + 400).forEach(([r, v]) => b.set(r, v));
      await b.commit();
    }
    if (ic && Array.isArray(ic.items)) {
      // Une seule écriture, et seulement si l'emploi du temps a changé.
      const items = ic.items.slice(0, 1500).map(c => ({ d: String(c.d || ''), t: c.t || null, e: c.e || null, ti: String(c.ti || '').slice(0, 120), k: c.k || null, p: String(c.p || '').slice(0, 40) }));
      const old = (await getDoc(metaDoc('courses'))).data();
      if (JSON.stringify(old?.items || []) !== JSON.stringify(items)) await setDoc(metaDoc('courses'), { items, updated_at_ms: Date.now() });
    }
    if (is && Array.isArray(is.items) && Number(is.updated_at_ms) > (cur.styles?.updated_at_ms || 0)) {
      const items = is.items.slice(0, 2000).map(s => ({ title: String(s.title || '').slice(0, 120), date: String(s.date || '').slice(0, 10), time: String(s.time || '').slice(0, 5), color: HEX.test(s.color || '') ? s.color : '', hidden: !!s.hidden })).filter(s => s.title);
      await setDoc(metaDoc('styles'), { items, updated_at_ms: Number(is.updated_at_ms) });
    }
    return { written: ops.length };
  }
};

if ('serviceWorker' in navigator && !DESKTOP_APP && !PC_MODE) navigator.serviceWorker.register('sw.js').catch(() => {});

// Dans un navigateur : plein écran au premier toucher (et de nouveau si on en
// sort), et proposition d'installer le site comme appli.
if (!DESKTOP_APP && !PC_MODE) {
  const installed = () => ['fullscreen', 'standalone'].some(m => matchMedia(`(display-mode: ${m})`).matches) || navigator.standalone === true;
  const root = document.documentElement;
  // Le plein écran consomme le geste : pas sur ce qui en a besoin
  // (connexion Google, installation, choix de fichiers).
  const SKIP = '#btn-login, #install, input, label, select, textarea';
  addEventListener('pointerup', e => {
    if (installed() || document.fullscreenElement || !root.requestFullscreen || e.target.closest?.(SKIP)) return;
    root.requestFullscreen({ navigationUI: 'hide' }).catch(() => {});
  }, { capture: true, passive: true });

  const box = $('install');
  const later = () => { try { return Number(localStorage.getItem('nb.install-later')) > Date.now(); } catch (_) { return false; } };
  let ask = null;
  addEventListener('beforeinstallprompt', e => {
    e.preventDefault();
    ask = e;
    if (!later()) box.hidden = false;
  });
  addEventListener('appinstalled', () => { ask = null; box.hidden = true; });
  $('btn-install').onclick = async () => {
    if (!ask) return;
    box.hidden = true;
    ask.prompt();
    await ask.userChoice.catch(() => {});
    ask = null;
  };
  $('btn-install-no').onclick = () => {
    box.hidden = true;
    try { localStorage.setItem('nb.install-later', String(Date.now() + 7 * 864e5)); } catch (_) {}
  };
  // iPhone : pas d'invite automatique, on explique le geste.
  if (/iPhone|iPad|iPod/.test(navigator.userAgent) && !installed() && !later()) {
    $('install-text').textContent = "Pour l'avoir en plein écran comme une appli : Partager, puis « Sur l'écran d'accueil ».";
    $('btn-install').hidden = true;
    box.hidden = false;
  }
}
