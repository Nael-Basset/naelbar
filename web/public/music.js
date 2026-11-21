// Onglet Musique : la télécommande de l'extension YouTube Music (ytm-remote),
// intégrée telle quelle. On l'associe une fois avec le QR code de
// l'extension ; le salon est gardé sur l'appareil et dans le compte, pour
// que les autres appareils connectés le retrouvent.
import { doc, getDoc, setDoc } from 'https://www.gstatic.com/firebasejs/10.14.1/firebase-firestore.js';

const REMOTE = 'https://nael-basset.github.io/ytm-remote/';
const ROOM = /^ytm-[a-z0-9-]{4,64}$/;

// Le salon dans un lien de l'extension, un QR code ou un identifiant collé.
export function roomOf(text) {
  const t = String(text || '').trim();
  if (ROOM.test(t)) return t;
  try {
    const u = new URL(t);
    if (u.origin + u.pathname.replace(/index\.html$/, '') !== REMOTE) return null;
    const r = decodeURIComponent(u.hash.slice(1)) || u.searchParams.get('room') || '';
    return ROOM.test(r) ? r : null;
  } catch (_) { return null; }
}

export function startMusic({ db, uid, ui: { $, el } }) {
  const box = $('music');
  const ref = doc(db, 'users', uid, 'meta', 'music');
  const local = { get: () => { try { return localStorage.getItem('nb.ytmRoom'); } catch (_) { return null; } }, set: v => { try { v ? localStorage.setItem('nb.ytmRoom', v) : localStorage.removeItem('nb.ytmRoom'); } catch (_) {} } };
  let room = local.get(), shown = false, stream = null;

  const save = r => { room = r; local.set(r); setDoc(ref, { room: r || null, updated_at_ms: Date.now() }).catch(() => {}); render(); };
  // Un salon associé sur un autre appareil.
  if (!room) getDoc(ref).then(s => { const r = s.data()?.room; if (r && ROOM.test(r) && !room) { room = r; local.set(r); if (shown) render(); } }).catch(() => {});

  function stopCamera() { stream?.getTracks().forEach(t => t.stop()); stream = null; }

  function render() {
    stopCamera();
    box.replaceChildren();
    if (room) {
      const f = el('iframe', 'remote');
      f.src = REMOTE + '#' + encodeURIComponent(room);
      f.title = 'Télécommande YouTube Music';
      f.allow = 'autoplay; clipboard-write; screen-wake-lock';
      const change = el('button', 'ghost', 'Changer d\'ordinateur');
      change.style.margin = '8px 16px'; change.style.minHeight = '34px';
      change.onclick = () => { if (confirm('Oublier cette association ?')) save(null); };
      box.append(f, change);
      return;
    }
    const pad = el('div', 'pad');
    pad.append(el('p', 'lead', 'Sur le PC, ouvre music.youtube.com avec l\'extension, puis clique sur l\'icône téléphone : un QR code s\'affiche. Scanne-le ici une seule fois.'));
    const scan = el('button', 'primary', 'Scanner le QR code'); scan.style.width = '100%';
    const video = el('video'); video.playsInline = true; video.muted = true; video.className = 'scan-video'; video.hidden = true;
    const err = el('p', 'err');
    scan.onclick = async () => {
      err.textContent = '';
      if (!('BarcodeDetector' in window)) { err.textContent = 'Ce navigateur ne lit pas les QR codes : colle le lien ci-dessous (bouton « Copier le lien » de l\'extension).'; return; }
      try {
        const det = new BarcodeDetector({ formats: ['qr_code'] });
        stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: 'environment' } });
        video.srcObject = stream; video.hidden = false; await video.play();
        scan.hidden = true;
        const tick = async () => {
          if (!stream) return;
          try { for (const c of await det.detect(video)) { const r = roomOf(c.rawValue); if (r) return save(r); } } catch (_) {}
          setTimeout(tick, 250);
        };
        tick();
      } catch (_) { stopCamera(); video.hidden = true; scan.hidden = false; err.textContent = 'Caméra indisponible.'; }
    };
    const form = el('form', 'row');
    const input = el('input'); input.placeholder = 'Ou colle le lien de l\'extension'; input.autocomplete = 'off';
    const ok = el('button', 'ghost', 'OK'); ok.type = 'submit';
    form.append(input, ok);
    form.onsubmit = e => { e.preventDefault(); const r = roomOf(input.value); r ? save(r) : (err.textContent = 'Ce lien ne vient pas de l\'extension.'); };
    pad.append(scan, video, form, err);
    box.append(pad);
  }

  // Rien ne se charge tant que l'onglet n'est pas ouvert ; la caméra s'arrête
  // en le quittant.
  return {
    show() { if (!shown) { shown = true; render(); } },
    hide() { stopCamera(); if (!room && shown) { shown = false; box.replaceChildren(); } },
  };
}
