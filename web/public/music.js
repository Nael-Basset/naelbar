// Onglet Musique : la télécommande de l'extension YouTube Music (ytm-remote),
// intégrée telle quelle. Pas de second QR code : quand un onglet YouTube
// Music s'ouvre sur le PC, l'extension signale son salon à naelbar, qui le
// range dans le compte (users/{uid}/meta/music). Cet onglet le suit.
import { doc, onSnapshot } from 'https://www.gstatic.com/firebasejs/10.14.1/firebase-firestore.js';

const REMOTE = 'https://nael-basset.github.io/ytm-remote/';
const ROOM = /^ytm-[a-f0-9]{24}$/;

export function startMusic({ db, uid, ui: { $, el } }) {
  const box = $('music');
  let room = null, shown = false, unsub = null;

  function render() {
    if (!shown) return;
    const f = box.querySelector('iframe');
    if (room && f?.dataset.room === room) return;
    box.replaceChildren();
    if (room) {
      const frame = el('iframe', 'remote');
      frame.dataset.room = room;
      frame.src = REMOTE + '#' + room;
      frame.title = 'Télécommande YouTube Music';
      frame.allow = 'autoplay; clipboard-write; screen-wake-lock';
      box.append(frame);
      return;
    }
    const pad = el('div', 'pad');
    pad.append(
      el('p', 'lead', 'Ouvre music.youtube.com sur le PC, avec l\'extension et naelbar lancés : la télécommande apparaît ici toute seule.'),
      el('p', 'meta', 'Il faut le « Lien avec le téléphone » activé dans les paramètres de naelbar. La première fois, accepte la nouvelle télécommande sur le PC.'),
    );
    box.append(pad);
  }

  // L'écoute ne commence qu'à la première ouverture de l'onglet.
  return {
    show() {
      shown = true;
      if (!unsub) unsub = onSnapshot(doc(db, 'users', uid, 'meta', 'music'), s => {
        const r = s.data()?.room;
        room = typeof r === 'string' && ROOM.test(r) ? r : null;
        render();
      }, () => {});
      render();
    },
    hide() {},
  };
}
