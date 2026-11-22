// Lien direct téléphone – PC, seulement sur le même réseau.
//
// WebRTC sans serveur STUN ni TURN : seules les adresses locales sont
// proposées, donc le canal ne s'ouvre que si les deux appareils se voient
// directement. C'est aussi le test « même Wi-Fi ». La mise en relation passe
// par deux documents Firestore privés (users/{uid}/rtc/offer et answer) ; les
// données, elles, vont en direct et chiffrées (DTLS).
//
// Rôle PC (?pc=1) : la page tourne dans une WebView2 cachée de naelbar et
// relaie les messages au programme par chrome.webview.postMessage.
import { doc, setDoc, onSnapshot } from 'https://www.gstatic.com/firebasejs/10.14.1/firebase-firestore.js';

const RTC = { iceServers: [] };
const OPEN_TIMEOUT = 9000;
const OFFER_MAX_AGE = 60_000;
const CHUNK = 16 * 1024;
const HIGH_WATER = 1 << 20;

const rid = () => Array.from(crypto.getRandomValues(new Uint8Array(12)), b => b.toString(16).padStart(2, '0')).join('');

// Attend la fin de la collecte des adresses locales (rapide : pas de STUN).
function gathered(pc, ms = 2500) {
  if (pc.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise(res => {
    const t = setTimeout(res, ms);
    pc.addEventListener('icegatheringstatechange', () => { if (pc.iceGatheringState === 'complete') { clearTimeout(t); res(); } });
  });
}

export function startPc(opts) {
  // Sans ça, l'onglet PC resterait sur « recherche… » sans dire pourquoi.
  const fail = why => {
    opts.log?.(why);
    const $ = opts.ui?.$; if (!$ || opts.pcMode) return;
    $('pc-state').textContent = 'indisponible';
    $('pc-why').textContent = why;
  };
  if (!('RTCPeerConnection' in window)) return fail('Ce navigateur bloque WebRTC, nécessaire au lien avec le PC. Essaie avec Chrome, ou réactive WebRTC dans les réglages de confidentialité.');
  try { if (opts.pcMode) pcRole(opts); else phoneRole(opts); }
  catch (e) { fail('Lien avec le PC impossible : ' + (e?.message || e)); }
}

// ================================================================ PC
function pcRole({ db, uid, log }) {
  const host = window.chrome?.webview;
  if (!host) return;
  const post = m => host.postMessage(m);
  const offerRef = doc(db, 'users', uid, 'rtc', 'offer');
  const answerRef = doc(db, 'users', uid, 'rtc', 'answer');
  let pc = null, files = null, lastSid = null;

  // Réponses du programme : fichier enregistré ou refusé.
  host.addEventListener('message', e => {
    const m = e.data;
    if (m && typeof m.fileDone === 'string' && files?.readyState === 'open') files.send(JSON.stringify({ t: 'ok', id: m.fileDone, ok: !!m.ok }));
    if (m && m.dirsReply && files?.readyState === 'open') files.send(JSON.stringify({ ...m.dirsReply, t: 'dirs' }));
    // Salon de la télécommande YouTube Music, signalé par l'extension à
    // naelbar : l'onglet Musique du téléphone s'y connecte tout seul.
    if (m && typeof m.ytmRoom === 'string' && /^ytm-[a-f0-9]{24}$/.test(m.ytmRoom)) {
      setDoc(doc(db, 'users', uid, 'meta', 'music'), { room: m.ytmRoom, updated_at_ms: Date.now() }).catch(() => log('salon musique non enregistré'));
    }
  });
  // Connecté et à l'écoute : demande le dernier salon connu, sinon il n'arrive
  // que quand l'extension en ouvre un nouveau.
  post({ naelbarYtm: 'get' });

  const b64 = buf => {
    const bytes = new Uint8Array(buf); let s = '';
    for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    return btoa(s);
  };

  function close() {
    if (!pc) return;
    try { pc.close(); } catch (_) {}
    pc = null; files = null;
    post({ naelbarPeer: 'closed' });
  }

  onSnapshot(offerRef, async snap => {
    const o = snap.data();
    if (!o || typeof o.sdp !== 'string' || typeof o.sid !== 'string' || o.sid === lastSid) return;
    if (Date.now() - (Number(o.at) || 0) > OFFER_MAX_AGE) { log('offre ignorée, trop ancienne de ' + Math.round((Date.now() - (Number(o.at) || 0)) / 1000) + ' s'); return; }
    lastSid = o.sid;
    close();
    log('offre reçue');
    const p = pc = new RTCPeerConnection(RTC);
    p.ondatachannel = ({ channel: ch }) => {
      ch.binaryType = 'arraybuffer';
      if (ch.label === 'input') {
        ch.onopen = () => { post({ naelbarPeer: 'open' }); log('canal ouvert'); };
        ch.onmessage = e => { if (typeof e.data === 'string' && e.data.length < 2048) post({ naelbarInput: e.data }); };
        ch.onclose = () => { if (pc === p) close(); };
      } else if (ch.label === 'files') {
        files = ch;
        ch.onmessage = e => {
          if (typeof e.data === 'string') {
            try {
              const m = JSON.parse(e.data);
              if (m && (m.t === 'start' || m.t === 'end')) {
                const f = { op: m.t, id: String(m.id), name: String(m.name || ''), size: Number(m.size) || 0 };
                if (typeof m.dir === 'string') f.dir = m.dir;
                post({ naelbarFile: f });
              } else if (m && m.t === 'dirs') post({ naelbarDirs: { q: Number(m.q) || 0, op: String(m.op), path: String(m.path || ''), name: String(m.name || '') } });
            } catch (_) {}
          } else post({ naelbarFile: { op: 'data', b64: b64(e.data) } });
        };
      }
    };
    p.onconnectionstatechange = () => { log('connexion: ' + p.connectionState); if (['failed', 'closed', 'disconnected'].includes(p.connectionState) && pc === p) close(); };
    try {
      await p.setRemoteDescription({ type: 'offer', sdp: o.sdp });
      await p.setLocalDescription(await p.createAnswer());
      await gathered(p);
      const hosts = (p.localDescription.sdp.match(/^a=candidate:.*$/gm) || []).length;
      await setDoc(answerRef, { sdp: p.localDescription.sdp, sid: o.sid, at: Date.now() });
      log('réponse envoyée, ' + hosts + ' adresse(s) locale(s)');
    } catch (e) { log('réponse impossible: ' + (e?.message || e)); if (pc === p) close(); }
  }, e => log('écoute impossible: ' + (e?.code || e)));
  log('PC prêt, en attente du téléphone');
}

// ================================================================ Téléphone
function phoneRole({ db, uid, ui: { $, el } }) {
  const offerRef = doc(db, 'users', uid, 'rtc', 'offer');
  const answerRef = doc(db, 'users', uid, 'rtc', 'answer');
  let pc = null, input = null, files = null, state = 'idle', unsubAnswer = null, timer = 0;

  const tabBtn = document.querySelector('.tabs button[data-tab="pc"]');
  function setState(s, why = '') {
    state = s;
    const live = s === 'open';
    $('pc-state').textContent = live ? 'connecté' : s === 'connecting' ? 'recherche…' : 'hors de portée';
    $('pc-state').classList.toggle('ok', live);
    $('pc-locked').hidden = live;
    $('pc-live').hidden = !live;
    $('pc-why').textContent = why;
    tabBtn.querySelector('.dot')?.remove();
    if (live) tabBtn.append(el('span', 'dot'));
    retryBtn.hidden = s !== 'off';
    renderScan();
  }
  const retryBtn = el('button', 'ghost', 'Réessayer');
  retryBtn.style.marginTop = '10px';
  retryBtn.onclick = () => connect();
  $('pc-locked').append(retryBtn);

  function teardown() {
    clearTimeout(timer);
    unsubAnswer?.(); unsubAnswer = null;
    try { pc?.close(); } catch (_) {}
    pc = input = files = null;
    sending = false;
  }

  async function connect() {
    if (state === 'connecting' || state === 'open' || !navigator.onLine) { if (!navigator.onLine) setState('off', 'Pas de réseau.'); return; }
    teardown();
    setState('connecting');
    const sid = rid();
    const p = pc = new RTCPeerConnection(RTC);
    input = p.createDataChannel('input', { ordered: true });
    files = p.createDataChannel('files', { ordered: true });
    files.binaryType = 'arraybuffer';
    files.bufferedAmountLowThreshold = HIGH_WATER / 2;
    files.onopen = () => pump();
    files.onmessage = e => { try { const m = JSON.parse(e.data); if (m.t === 'ok') onSaved(m.id, m.ok); else if (m.t === 'dirs') onDirs(m); } catch (_) {} };
    input.onopen = () => { if (pc !== p) return; clearTimeout(timer); unsubAnswer?.(); unsubAnswer = null; setState('open'); buildLive(); pump(); };
    input.onclose = () => { if (pc === p) { teardown(); setState('off', 'Connexion au PC perdue.'); } };
    p.onconnectionstatechange = () => { if (p.connectionState === 'failed' && pc === p) { teardown(); setState('off', 'Le PC ne répond pas sur ce réseau.'); } };
    timer = setTimeout(() => { if (pc === p && state === 'connecting') { teardown(); setState('off', 'Le PC est éteint, naelbar n\'a pas le lien téléphone activé, ou vous n\'êtes pas sur le même Wi-Fi.'); } }, OPEN_TIMEOUT);
    try {
      await p.setLocalDescription(await p.createOffer());
      await gathered(p);
      if (pc !== p) return;
      unsubAnswer = onSnapshot(answerRef, s => {
        const a = s.data();
        if (pc === p && a && a.sid === sid && typeof a.sdp === 'string' && !p.currentRemoteDescription) p.setRemoteDescription({ type: 'answer', sdp: a.sdp }).catch(() => {});
      });
      await setDoc(offerRef, { sdp: p.localDescription.sdp, sid, at: Date.now() });
    } catch (_) { if (pc === p) { teardown(); setState('off', 'Mise en relation impossible.'); } }
  }

  const send = m => { if (input?.readyState === 'open') input.send(JSON.stringify(m)); };
  const buzz = ms => { try { navigator.vibrate?.(ms); } catch (_) {} };

  // -------------------------------------------------------------- commandes
  let built = false;
  function buildLive() {
    if (built) return; built = true;
    const live = $('pc-live');
    const surface = el('div', 'pad-area', 'Glisse pour bouger · touche pour cliquer · deux doigts pour défiler');
    const btns = el('div', 'btns');
    const bl = el('button', '', 'Clic gauche'), br = el('button', '', 'Clic droit');
    bl.onclick = () => { send({ t: 'click', b: 'left' }); buzz(6); };
    br.onclick = () => { send({ t: 'click', b: 'right' }); buzz(6); };
    btns.append(bl, br);
    const kb = el('input'); kb.placeholder = 'Taper au clavier du PC'; kb.autocomplete = 'off'; kb.autocapitalize = 'off'; kb.spellcheck = false; kb.style.marginTop = '10px';
    const keys = el('div', 'keys');
    [['Échap', 'esc'], ['Tab', 'tab'], ['⌫', 'back'], ['Entrée', 'enter'], ['←', 'left'], ['↑', 'up'], ['↓', 'down'], ['→', 'right'], ['Préc.', 'prev'], ['Lecture', 'play'], ['Suiv.', 'next'], ['Muet', 'mute'], ['Vol −', 'voldown'], ['Vol +', 'volup'], ['Suppr', 'del'], ['Win', 'win']].forEach(([label, k]) => {
      const b = el('button', '', label); b.onclick = () => { send({ t: 'key', k }); buzz(4); }; keys.append(b);
    });
    live.append(surface, btns, kb, keys);
    trackpad(surface);
    keyboard(kb);
  }

  function trackpad(surface) {
    const TAP_MS = 220, TAP_SLOP = 9, DRAG_GAP = 260;
    const pointers = new Map();
    let gesture = null, lastTapEnd = 0, frame = 0;
    const acc = { dx: 0, dy: 0, sx: 0, sy: 0 };
    const round = v => Math.round(v * 100) / 100;
    const gain = d => 1.4 * (1 + Math.min(d, 36) / 24);
    function flush() {
      frame = 0;
      if (acc.dx || acc.dy) { send({ t: 'move', dx: round(acc.dx), dy: round(acc.dy) }); acc.dx = acc.dy = 0; }
      if (acc.sx || acc.sy) { send({ t: 'scroll', dx: round(acc.sx), dy: round(acc.sy) }); acc.sx = acc.sy = 0; }
    }
    surface.addEventListener('pointerdown', e => {
      e.preventDefault();
      surface.setPointerCapture(e.pointerId);
      pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
      const now = performance.now();
      if (pointers.size === 1) {
        gesture = { start: now, most: 1, moved: 0, drag: now - lastTapEnd < DRAG_GAP };
        if (gesture.drag) send({ t: 'btn', b: 'left', down: true });
      } else if (gesture) gesture.most = Math.max(gesture.most, pointers.size);
    });
    surface.addEventListener('pointermove', e => {
      const p = pointers.get(e.pointerId);
      if (!p || !gesture) return;
      const dx = e.clientX - p.x, dy = e.clientY - p.y;
      p.x = e.clientX; p.y = e.clientY;
      const d = Math.hypot(dx, dy);
      gesture.moved += d;
      if (pointers.size === 1 && gesture.most === 1) { const g = gain(d); acc.dx += dx * g; acc.dy += dy * g; }
      else if (pointers.size === 2) { acc.sx += dx / 2; acc.sy += dy / 2; }
      if (!frame) frame = requestAnimationFrame(flush);
    });
    function release(e) {
      if (!pointers.delete(e.pointerId) || pointers.size || !gesture) return;
      flush();
      const g = gesture; gesture = null;
      const now = performance.now();
      if (g.drag) { send({ t: 'btn', b: 'left', down: false }); lastTapEnd = 0; return; }
      if (g.moved < TAP_SLOP * g.most && now - g.start < TAP_MS + 80 * (g.most - 1)) {
        send({ t: 'click', b: g.most === 1 ? 'left' : g.most === 2 ? 'right' : 'middle' });
        buzz(6);
        lastTapEnd = g.most === 1 ? now : 0;
      }
    }
    surface.addEventListener('pointerup', release);
    surface.addEventListener('pointercancel', release);
    surface.addEventListener('contextmenu', e => e.preventDefault());
  }

  // Le champ garde ce qui a été tapé ; on envoie la différence, ce qui
  // marche aussi avec la saisie prédictive des claviers Android.
  function keyboard(kb) {
    let prev = '';
    kb.addEventListener('input', () => {
      const now = kb.value;
      let i = 0;
      while (i < prev.length && i < now.length && prev[i] === now[i]) i++;
      const removed = [...prev.slice(i)].length, added = now.slice(i);
      for (let n = 0; n < Math.min(removed, 200); n++) send({ t: 'key', k: 'back' });
      for (let j = 0; j < added.length; j += 200) send({ t: 'text', s: added.slice(j, j + 200) });
      prev = now;
      if (now.length > 400) { kb.value = ''; prev = ''; }
    });
    kb.addEventListener('keydown', e => {
      if (e.key === 'Enter') { e.preventDefault(); send({ t: 'key', k: 'enter' }); kb.value = ''; prev = ''; }
      else if (e.key === 'Backspace' && !kb.value) send({ t: 'key', k: 'back' });
    });
  }

  // -------------------------------------------------------------- fichiers
  // File d'attente dans IndexedDB : elle survit à la fermeture de la page et
  // part dès que le PC est joignable.
  const idb = new Promise((res, rej) => {
    const r = indexedDB.open('naelbar-pc', 1);
    r.onupgradeneeded = () => r.result.createObjectStore('queue', { keyPath: 'id' });
    r.onsuccess = () => res(r.result); r.onerror = () => rej(r.error);
  });
  const tx = async (mode, f) => { const d = await idb; return new Promise((res, rej) => { const t = d.transaction('queue', mode); const out = f(t.objectStore('queue')); t.oncomplete = () => res(out?.result); t.onerror = () => rej(t.error); }); };
  const listQueue = () => tx('readonly', s => s.getAll());
  const progress = new Map();
  let sending = false, waiting = null;

  async function renderQueue() {
    const items = await listQueue().catch(() => []);
    const ul = $('queue'); ul.replaceChildren();
    for (const f of items.sort((a, b) => a.at - b.at)) {
      const li = el('li');
      const box = el('div', 'txt');
      box.append(el('div', '', f.name), el('div', 'sub', `${(f.size / 1048576).toFixed(1)} Mo · ${progress.has(f.id) ? 'envoi…' : f.err ? 'refusé par le PC' : 'en attente du PC'}`));
      if (progress.has(f.id)) { const b = el('div', 'bar-prog'); const i = el('i'); i.style.width = Math.round(progress.get(f.id) * 100) + '%'; b.append(i); box.append(b); }
      const x = el('button', 'x', '×'); x.setAttribute('aria-label', 'Retirer'); x.onclick = () => tx('readwrite', s => s.delete(f.id)).then(renderQueue);
      li.append(box, x); ul.append(li);
    }
  }

  // items: [{ name, blob, dir? }]. `dir` files a scan under the lessons folder.
  async function enqueue(items) {
    for (const f of items) {
      const rec = { id: rid(), name: f.name, size: f.blob.size, type: f.blob.type, blob: f.blob, at: Date.now() };
      if (typeof f.dir === 'string') rec.dir = f.dir;
      await tx('readwrite', s => s.put(rec)).catch(() => {});
    }
    renderQueue();
    if (state === 'open') pump(); else connect();
  }
  $('file-pick').onchange = async e => {
    const list = [...e.target.files]; e.target.value = '';
    enqueue(list.map(f => ({ name: f.name, blob: f })));
  };

  // -------------------------------------------------------------- scan de cours
  // Photos -> un PDF (ou les images), rangé dans un dossier du « Dossier des
  // cours » choisi dans naelbar. Le dossier se choisit quand le PC est joignable
  // et reste mémorisé ; hors Wi-Fi, le scan attend dans la file.
  const pending = new Map();
  let nextQ = 1;
  function ask(op, path, name = '') {
    if (files?.readyState !== 'open') return Promise.reject(new Error('PC hors de portée'));
    const q = nextQ++;
    return new Promise((res, rej) => {
      pending.set(q, { res, rej });
      files.send(JSON.stringify({ t: 'dirs', q, op, path, name }));
      setTimeout(() => { if (pending.delete(q)) rej(new Error('Le PC ne répond pas')); }, 8000);
    });
  }
  function onDirs(m) {
    const p = pending.get(m.q); if (!p) return;
    pending.delete(m.q);
    m.ok ? p.res(m) : p.rej(new Error('Dossier introuvable sur le PC'));
  }

  const store = { get: k => { try { return JSON.parse(localStorage.getItem(k)); } catch (_) { return null; } }, set: (k, v) => { try { localStorage.setItem(k, JSON.stringify(v)); } catch (_) {} } };
  let dest = store.get('nb.scanDest') || { path: '', label: 'Dossier des cours' };
  let pages = [], browsing = null, scanMsg = '', building = false;

  function renderScan() {
    const box = $('scan'); if (!box) return;
    box.replaceChildren();
    if (browsing) return renderBrowser(box);
    const row = el('div', 'dest');
    row.append(el('span', '', 'Dossier : ' + dest.label));
    const chg = el('button', 'ghost', 'Changer'); chg.style.minHeight = '34px';
    chg.disabled = state !== 'open';
    chg.title = state === 'open' ? '' : 'Disponible sur le même Wi-Fi que le PC';
    chg.onclick = () => { browsing = { cwd: dest.path ? dest.path.split('/') : [] }; renderScan(); };
    row.append(chg); box.append(row);
    const shoot = el('label', 'file'); const cam = el('input'); cam.type = 'file'; cam.accept = 'image/*'; cam.capture = 'environment';
    shoot.append(cam, el('span', '', 'Photographier une page'));
    const gal = el('label', 'file'); const gi = el('input'); gi.type = 'file'; gi.accept = 'image/*'; gi.multiple = true;
    gal.append(gi, el('span', '', 'Choisir dans la galerie'));
    gal.style.marginTop = '8px';
    const add = async list => { for (const f of list) pages.push({ file: f, url: URL.createObjectURL(f) }); renderScan(); };
    cam.onchange = e => { add([...e.target.files]); e.target.value = ''; };
    gi.onchange = e => { add([...e.target.files]); e.target.value = ''; };
    box.append(shoot, gal);
    if (pages.length) {
      const grid = el('div', 'thumbs');
      pages.forEach((p, i) => {
        const t = el('div', 'th'); const img = el('img'); img.src = p.url; img.alt = `Page ${i + 1}`;
        const x = el('button', '', '×'); x.setAttribute('aria-label', `Retirer la page ${i + 1}`);
        x.onclick = () => { URL.revokeObjectURL(p.url); pages.splice(i, 1); renderScan(); };
        t.append(img, el('i', '', String(i + 1)), x); grid.append(t);
      });
      box.append(grid);
      const name = el('input'); name.placeholder = 'Nom du fichier (ex : Cours 3 - Optique)'; name.maxLength = 100; name.style.marginTop = '12px';
      name.value = store.get('nb.scanName') || '';
      const fmt = el('div', 'seg'); fmt.style.marginTop = '8px';
      let asPdf = true;
      const bp = el('button', 'on', 'Un PDF'), bi = el('button', '', 'Images séparées');
      bp.onclick = () => { asPdf = true; bp.classList.add('on'); bi.classList.remove('on'); };
      bi.onclick = () => { asPdf = false; bi.classList.add('on'); bp.classList.remove('on'); };
      fmt.append(bp, bi);
      const go = el('button', 'primary', `Envoyer ${pages.length} page${pages.length > 1 ? 's' : ''}`); go.style.width = '100%'; go.style.marginTop = '10px';
      go.disabled = building;
      go.onclick = async () => {
        if (building) return;
        building = true; go.disabled = true; go.textContent = 'Préparation…';
        const base = (name.value.trim() || 'Scan ' + new Date().toLocaleDateString('fr-FR').replace(/\//g, '-')).replace(/[<>:"/\\|?*\x00-\x1f]/g, '_');
        store.set('nb.scanName', name.value.trim());
        try {
          let items;
          if (asPdf) items = [{ name: base + '.pdf', blob: await makePdf(pages, (i, n) => { go.textContent = `Page ${i}/${n}…`; }), dir: dest.path }];
          else items = pages.map((p, i) => ({ name: `${base} - ${String(i + 1).padStart(2, '0')}.${(p.file.name.match(/\.(\w{2,5})$/) || [, 'jpg'])[1].toLowerCase()}`, blob: p.file, dir: dest.path }));
          await enqueue(items);
          pages.forEach(p => URL.revokeObjectURL(p.url)); pages = [];
          scanMsg = state === 'open' ? `Envoi vers ${dest.label}.` : `En attente du PC, partira vers ${dest.label}.`;
        } catch (e) { scanMsg = 'Création impossible : ' + (e.message || e); }
        building = false; renderScan();
      };
      box.append(name, fmt, go);
    }
    if (scanMsg) { const m = el('p', 'meta', scanMsg); m.style.padding = '8px 0 0'; box.append(m); }
  }

  async function renderBrowser(box) {
    const cwd = browsing.cwd;
    box.append(el('p', 'meta', 'Chargement…'));
    let data;
    try { data = await ask('list', cwd.join('/')); }
    catch (e) { if (cwd.length) { browsing.cwd = []; return renderScan(); } browsing = null; scanMsg = e.message; return renderScan(); }
    if (!browsing) return;
    box.replaceChildren();
    const crumbs = el('div', 'crumbs');
    [data.root, ...cwd].forEach((c, i) => {
      if (i) crumbs.append(' / ');
      const a = el('button', 'link', c); a.onclick = () => { browsing.cwd = cwd.slice(0, i); renderScan(); };
      crumbs.append(a);
    });
    const list = el('ul', 'list');
    if (!data.dirs.length) list.append(el('li', 'empty', 'Aucun sous-dossier ici.'));
    data.dirs.forEach(d => {
      const li = el('li'); const b = el('button', 'item', d);
      b.onclick = () => { browsing.cwd = [...cwd, d]; renderScan(); };
      li.append(b); list.append(li);
    });
    const actions = el('div', 'btns');
    const mk = el('button', '', '+ Nouveau dossier');
    mk.onclick = async () => {
      const n = prompt('Nom du nouveau dossier'); if (!n || !n.trim()) return;
      try { const r = await ask('mkdir', cwd.join('/'), n.trim()); browsing.cwd = [...cwd, r.name]; } catch (e) { alert(e.message); }
      renderScan();
    };
    const here = cwd.length ? cwd[cwd.length - 1] : data.root;
    const ok = el('button', 'primary', `Choisir « ${here} »`);
    ok.onclick = () => { dest = { path: cwd.join('/'), label: cwd.length ? cwd.join(' / ') : data.root }; store.set('nb.scanDest', dest); browsing = null; scanMsg = ''; renderScan(); };
    const cancel = el('button', 'ghost', 'Annuler'); cancel.style.marginTop = '8px'; cancel.style.width = '100%';
    cancel.onclick = () => { browsing = null; renderScan(); };
    actions.append(mk, ok);
    box.append(crumbs, list, actions, cancel);
  }

  // PDF minimal : une page par photo, chaque image en JPEG (DCTDecode). La
  // photo est redessinée dans le bon sens (EXIF) et limitée à 3000 px.
  async function makePdf(list, onPage) {
    const enc = new TextEncoder();
    const parts = []; let len = 0; const offs = [];
    const push = x => { const b = typeof x === 'string' ? enc.encode(x) : x; parts.push(b); len += b.length; };
    const obj = (id, ...body) => { offs[id] = len; push(`${id} 0 obj\n`); body.forEach(push); push('\nendobj\n'); };
    push('%PDF-1.4\n%âãÏÓ\n');
    const n = list.length, kids = list.map((_, i) => `${3 + 3 * i} 0 R`).join(' ');
    obj(1, '<< /Type /Catalog /Pages 2 0 R >>');
    obj(2, `<< /Type /Pages /Kids [${kids}] /Count ${n} >>`);
    for (let i = 0; i < n; i++) {
      onPage?.(i + 1, n);
      const bmp = await createImageBitmap(list[i].file, { imageOrientation: 'from-image' });
      const k = Math.min(1, 3000 / Math.max(bmp.width, bmp.height));
      const w = Math.round(bmp.width * k), h = Math.round(bmp.height * k);
      const c = document.createElement('canvas'); c.width = w; c.height = h;
      const g = c.getContext('2d'); g.fillStyle = '#fff'; g.fillRect(0, 0, w, h); g.drawImage(bmp, 0, 0, w, h); bmp.close?.();
      const jpeg = new Uint8Array(await (await new Promise((r, j) => c.toBlob(b => b ? r(b) : j(new Error('image')), 'image/jpeg', 0.9))).arrayBuffer());
      // Largeur A4 (595 pt), hauteur proportionnelle.
      const pw = 595, ph = Math.round(595 * h / w);
      const [pg, im, ct] = [3 + 3 * i, 4 + 3 * i, 5 + 3 * i];
      const draw = `q ${pw} 0 0 ${ph} 0 0 cm /Im0 Do Q`;
      obj(pg, `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 ${pw} ${ph}] /Resources << /XObject << /Im0 ${im} 0 R >> >> /Contents ${ct} 0 R >>`);
      obj(im, `<< /Type /XObject /Subtype /Image /Width ${w} /Height ${h} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length ${jpeg.length} >>\nstream\n`, jpeg, '\nendstream');
      obj(ct, `<< /Length ${draw.length} >>\nstream\n${draw}\nendstream`);
    }
    const count = 3 + 3 * n, xref = len;
    let table = `xref\n0 ${count}\n0000000000 65535 f \n`;
    for (let id = 1; id < count; id++) table += String(offs[id]).padStart(10, '0') + ' 00000 n \n';
    push(table + `trailer\n<< /Size ${count} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`);
    return new Blob(parts, { type: 'application/pdf' });
  }

  function onSaved(id, ok) {
    if (waiting?.id === id) { const w = waiting; waiting = null; w.done(ok); }
  }
  const drained = () => files.bufferedAmount <= HIGH_WATER ? Promise.resolve() : new Promise(r => { files.onbufferedamountlow = () => { files.onbufferedamountlow = null; r(); }; });

  async function pump() {
    if (sending) return;
    sending = true;
    try {
      for (;;) {
        if (state !== 'open' || files?.readyState !== 'open') break;
        const next = (await listQueue()).filter(f => !f.err).sort((a, b) => a.at - b.at)[0];
        if (!next) break;
        progress.set(next.id, 0); renderQueue();
        files.send(JSON.stringify({ t: 'start', id: next.id, name: next.name, size: next.size, ...(typeof next.dir === 'string' ? { dir: next.dir } : {}) }));
        for (let off = 0; off < next.size; off += CHUNK) {
          if (files?.readyState !== 'open') throw new Error('closed');
          await drained();
          files.send(await next.blob.slice(off, off + CHUNK).arrayBuffer());
          progress.set(next.id, Math.min(1, (off + CHUNK) / next.size));
          if ((off / CHUNK) % 32 === 0) renderQueue();
        }
        const ok = await new Promise(done => { waiting = { id: next.id, done }; files.send(JSON.stringify({ t: 'end', id: next.id })); setTimeout(() => onSaved(next.id, false), 30_000); });
        progress.delete(next.id);
        if (ok) await tx('readwrite', s => s.delete(next.id));
        else await tx('readwrite', s => s.put({ ...next, err: true }));
        renderQueue();
      }
    } catch (_) { /* la file reste, elle repartira à la prochaine connexion */ }
    progress.clear(); sending = false; renderQueue();
  }

  // -------------------------------------------------------------- quand essayer
  // Au chargement, en revenant sur la page, au retour du réseau et en ouvrant
  // l'onglet PC. Jamais en boucle.
  connect();
  renderQueue();
  document.addEventListener('visibilitychange', () => { if (!document.hidden && state === 'off') connect(); });
  addEventListener('online', () => { if (state !== 'open') { state = 'off'; connect(); } });
  tabBtn.addEventListener('click', () => { if (state === 'off') connect(); });
}
