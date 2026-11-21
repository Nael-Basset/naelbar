# naelbar

naelbar est une petite « île » flottante open source (licence MIT) pour Windows 11, écrite en Rust. Elle apparaît en haut de l'écran, au centre, et s'ouvre lorsque le pointeur passe dessus.

> Statut : le projet est en **phase de développement précoce**. Rien n'est terminé, les fonctionnalités et les API peuvent changer à tout moment.

## Objectif : zéro activité à l'arrêt

Quand rien n'est en lecture et que l'île est fermée, le processus ne doit effectuer **aucun travail périodique** : pas de polling, pas de minuterie récurrente, pas de boucle `sleep`. Le programme ne réagit qu'aux notifications du système (messages de fenêtre, callbacks COM/WinRT, notifications d'alimentation, presse-papiers, etc.). Les mesures se font avec `scripts/measure-idle.ps1`.

## Prérequis

- Windows 11
- Rust stable, cible MSVC (`x86_64-pc-windows-msvc`), installé via [rustup](https://rustup.rs)
- Visual Studio Build Tools avec le composant C++ (requis par la chaîne MSVC)

## Compilation

```powershell
cargo build --release -p naelbar
```

L'exécutable se trouve ensuite dans `target\release\naelbar.exe`.

## Installer sans compiler

Téléchargez `naelbar.exe` dans la dernière [Release](../../releases), placez-le où vous voulez et lancez-le. Les réglages s'ouvrent depuis l'icône de la zone de notification.

## Téléphone, agenda et cours

Un seul site, ouvert depuis le QR code de **Paramètres → Téléphone** : <https://naelbar-agenda-eadafc.web.app>.

- **Compte Google** : connectez le même compte dans naelbar (Paramètres → Téléphone → Compte Google) et sur le site. Chaque compte ne voit que ses propres données.
- **Agenda** : vues jour, semaine et mois, ajout d'événements, post-its. Fonctionne partout, même PC éteint.
- **Emploi du temps .ics** (ADE ou autre) : collez son adresse dans les paramètres. Les cours passés restent dans l'historique. Couleur et masquage par double-clic, sur le PC comme sur le téléphone, synchronisés dans les deux sens.
- **Sur le même Wi-Fi** (option « Lien avec le téléphone ») : souris, clavier, touches média et envoi de fichiers vers `Téléchargements\naelbar`. Le lien est direct et chiffré (WebRTC, adresses locales uniquement). Hors Wi-Fi, les fichiers attendent sur le téléphone.
- **Scanner un cours** : photos → un PDF (ou les images), rangé dans un sous-dossier du « Dossier des cours » choisi dans les paramètres.

Le site est dans [`web/`](web) : HTML, CSS et JavaScript sans build, hébergé sur Firebase.

## Architecture

Le dépôt est un workspace Cargo composé de plusieurs crates :

| Crate | Rôle |
|---|---|
| `crates/naelbar` | binaire : fenêtres, compositeur, animations, interface, raccourcis, icône de zone de notification |
| `crates/nb-core` | type `Event` et `Sink`, seul contrat partagé entre les services |
| `crates/nb-media` | sessions média (GSMTC) |
| `crates/nb-audio` | volume, sourdine, périphériques, micro en cours d'utilisation |
| `crates/nb-clipboard` | historique du presse-papiers |
| `crates/nb-system` | batterie, mode d'alimentation, radios, luminosité, fenêtre active, statistiques |
| `crates/nb-integrations` | point d'entrée HTTP local, hooks d'agents, relais des notifications, météo |

La spécification complète se trouve dans [`docs/SPEC.md`](docs/SPEC.md).

## Mesurer l'activité à l'arrêt

```powershell
.\scripts\measure-idle.ps1 -ProcessName naelbar -Seconds 30
```

Le script relève le temps CPU, la mémoire de travail, les octets privés, le nombre de handles et de threads avant et après l'intervalle, puis affiche un tableau récapitulatif.

## Contribuer

Les contributions sont les bienvenues, en particulier sur les points suivants :

1. Ouvrez une issue pour discuter d'un changement important avant de le développer.
2. Créez une branche, faites vos modifications, puis vérifiez que `cargo build` et `cargo test` passent.
3. Respectez les règles « zéro activité à l'arrêt » de `docs/SPEC.md` : aucun timer ou polling permanent.
4. Ouvrez une pull request décrivant le changement et son impact sur la consommation à l'arrêt.

Le code et les textes doivent être originaux : n'incluez aucun code, icône ou texte provenant d'un logiciel tiers sans licence compatible.

## Licence

MIT, voir [`LICENSE`](LICENSE). Copyright 2026 naelbar contributors.
