# naelbar agenda (web)

Site statique (HTML/CSS/JS, sans build) hébergé sur Firebase Hosting, projet `naelbar-agenda-eadafc`, plan Spark.

## Redéployer

```
cd web
firebase deploy --only hosting      # site
firebase deploy --only firestore    # règles
firebase deploy                     # tout
```

Après une modification des fichiers mis en cache, incrémenter `C` (`naelbar-vN`) dans `public/sw.js`.

## Données

`users/{uid}/events/{id}` : `{ id, title, date, time|null, end|null, updated_at_ms, deleted }`
`users/{uid}/notes/{id}` : `{ id, text, done, updated_at_ms, deleted }`
`users/{uid}/meta/courses` : `{ items: [{d, t, e, ti, k, p}], updated_at_ms }`, l'emploi du temps envoyé par le PC
`users/{uid}/meta/styles` : `{ items: [{title, date, time, color, hidden}], updated_at_ms }`, couleurs des cours
`users/{uid}/rtc/offer|answer` : mise en relation WebRTC du lien même Wi-Fi
Les suppressions sont des tombstones (`deleted: true`).

## Pont Windows

La page connectée expose `window.naelbarSync.pull()` et `push({events, notes, courses, styles})` et `signOut()` (fusion par id, `updated_at_ms` le plus récent gagne).

`?pc=1` ouvre le rôle « PC » du lien téléphone, chargé par le processus caché `naelbar.exe --phone-link`.

## Connexion Google

Console Firebase > Authentication > Sign-in method > Google > Activer.
