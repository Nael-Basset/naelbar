//! Settings window. It runs as its own short-lived process (`--settings`), so
//! the island carries none of this while nobody is changing a setting. Changes
//! are written to the config file and the island is told to reload it.

use std::cell::RefCell;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PCWSTR, Result, w};

use crate::config::{self, Config, WM_CONFIG_CHANGED};
use crate::monitors::{self, Monitor};
use crate::render::Renderer;
use crate::ui::theme::*;
use crate::ui::{Align, Cmd, Id, Input, Rect, TextStyle, Ui, WidgetState, id, tr};

const W: f32 = 820.0;
const H: f32 = 580.0;
const NAV_W: f32 = 210.0;
const ROW_H: f32 = 76.0;
const CTRL_W: f32 = 250.0;
const PAGE_PAD: f32 = 28.0;

const WM_GEOCODED: u32 = WM_APP + 1;
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_CAPTURECHANGED: u32 = 0x0215;

const SECTIONS: &[(&str, &str)] = &[
    ("Général", "General"),
    ("Îlot", "Island"),
    ("Apparence", "Appearance"),
    ("Mouvement", "Motion"),
    ("Widgets", "Widgets"),
    ("À propos", "About"),
    ("Dock", "Dock"),
    ("Lanceur", "Launcher"),
    ("Musique", "Music"),
    ("Activités", "Activities"),
    ("Téléphone", "Phone"),
    ("Onglets", "Tabs"),
];

/// Index of the Music page in [`SECTIONS`].
const MUSIC_SECTION: usize = 8;
/// Index of the Activities page in [`SECTIONS`].
const ACTIVITY_SECTION: usize = 9;
/// Index of the Phone page in [`SECTIONS`].
const PHONE_SECTION: usize = 10;
/// Index of the island tabs page in [`SECTIONS`].
const TABS_SECTION: usize = 11;

/// One row per island tab: shown or hidden.
macro_rules! tab_row {
    ($name:literal, $fr:literal, $en:literal) => {
        Row {
            section: TABS_SECTION,
            only: Only::Any,
            title: ($fr, $en),
            desc: ("Onglet de l'îlot. Masqué, il disparaît de la barre d'onglets.", "Island tab. Hidden, it leaves the tab bar."),
            ctl: Ctl::Choice(
                |c| if c.hidden_tabs.iter().any(|t| t == $name) { "off" } else { "on" },
                |c, v| {
                    c.hidden_tabs.retain(|t| t != $name);
                    if v == "off" {
                        c.hidden_tabs.push($name.into());
                    }
                },
                &[("off", "Masqué", "Hidden"), ("on", "Affiché", "Shown")],
            ),
        }
    };
}

const WM_DOCK_SCANNED: u32 = WM_APP + 40;
const DOCK_ROW: f32 = 40.0;

/// Index of the Dock page, wherever it sits in the list.
/// Index of the Launcher page.
fn launcher_section() -> usize {
    SECTIONS.iter().position(|s| s.1 == "Launcher").unwrap_or(0)
}

const LAUNCH_ROW: f32 = 46.0;

/// The end of a long target, which is what changes most.
fn short_tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(n - max + 1).collect::<String>())
    }
}

pub fn dock_section() -> usize {
    SECTIONS.iter().position(|s| s.1 == "Dock").unwrap_or(0)
}

/// Apps found for the Dock page, and the pins being edited.
struct DockPage {
    store: crate::dock::Store,
    apps: Vec<String>,
    started: bool,
}

static DOCK_APPS: Mutex<Option<Vec<String>>> = Mutex::new(None);

const ACCENTS: [&str; 7] = ["#F5F5F5", "#4C9BFF", "#A78BFA", "#F472B6", "#F59E42", "#4ADE80", "#2DD4BF"];

#[derive(Clone, Copy)]
enum Unit {
    Px,
    Percent,
    Minutes,
}

type GetS = fn(&Config) -> &str;
type SetS = fn(&mut Config, &str);
type GetF = fn(&Config) -> f32;
type SetF = fn(&mut Config, f32);

/// Stored value, French label, English label.
type Opt = (&'static str, &'static str, &'static str);

enum Ctl {
    YtmExtension,
    Choice(GetS, SetS, &'static [Opt]),
    Slider { get: GetF, set: SetF, min: f32, max: f32, step: f32, unit: Unit },
    Autostart,
    StartMenu,
    Monitor,
    Accent,
    City,
    /// Text field for the .ics address; Ctrl+V pastes.
    IcsUrl,
    OpenConfig,
    Version,
    /// Count of ignored apps and a button that opens the list editor.
    IgnoreApps,
    /// Copies the command that adds the user to Performance Log Users.
    FpsRights,
    /// Folder for lesson scans, chosen with the system folder picker.
    CoursesRoot,
    /// The Google account of the agenda: state, sign in, sign out.
    GoogleAccount,
}

/// Which island shape a row applies to.
#[derive(Clone, Copy, PartialEq)]
enum Only {
    Any,
    Island,
    Notch,
}

impl Only {
    fn applies(self, c: &Config) -> bool {
        match self {
            Only::Any => true,
            Only::Island => c.shape != "notch",
            Only::Notch => c.shape == "notch",
        }
    }
}

struct Row {
    section: usize,
    only: Only,
    title: (&'static str, &'static str),
    desc: (&'static str, &'static str),
    ctl: Ctl,
}

static ROWS: &[Row] = &[
    tab_row!("media", "Média", "Media"),
    tab_row!("quick", "Raccourcis", "Shortcuts"),
    tab_row!("widgets", "Widgets", "Widgets"),
    tab_row!("agenda", "Agenda", "Calendar"),
    tab_row!("claude", "Claude", "Claude"),
    tab_row!("clipboard", "Presse-papiers", "Clipboard"),
    tab_row!("notifications", "Notifications", "Notifications"),
    tab_row!("shelf", "Étagère", "Shelf"),
    tab_row!("dock", "Dock", "Dock"),
    Row {
        section: 0,
        only: Only::Any,
        title: ("Langue", "Language"),
        desc: ("Celle de l'îlot et de cette fenêtre.", "Used by the island and this window."),
        ctl: Ctl::Choice(|c| &c.language, |c, v| c.language = v.into(), &[("auto", "Système", "System"), ("fr", "Français", "Français"), ("en", "English", "English")]),
    },
    Row {
        section: 0,
        only: Only::Any,
        title: ("Lancer au démarrage", "Start with Windows"),
        desc: ("Ouvre naelbar à l'ouverture de ta session.", "Opens naelbar when you sign in."),
        ctl: Ctl::Autostart,
    },
    Row {
        section: 0,
        only: Only::Any,
        title: ("Raccourci du menu Démarrer", "Start menu shortcut"),
        desc: ("Permet de lancer naelbar en le cherchant dans le menu Démarrer.", "Lets you launch naelbar by searching for it in the Start menu."),
        ctl: Ctl::StartMenu,
    },
    Row {
        section: 0,
        only: Only::Any,
        title: ("Invisible en capture", "Hidden from capture"),
        desc: ("Absent des captures d'écran, enregistrements et partages d'écran.", "Left out of screenshots, recordings and screen sharing."),
        ctl: Ctl::Choice(|c| if c.hide_from_capture { "on" } else { "off" }, |c, v| c.hide_from_capture = v == "on", &[("off", "Non", "No"), ("on", "Oui", "Yes")]),
    },
    Row {
        section: 0,
        only: Only::Any,
        title: ("Captures sur l'étagère", "Captures on the shelf"),
        desc: ("Chaque nouvelle capture d'écran est ajoutée à l'étagère.", "Each new screenshot is added to the shelf."),
        ctl: Ctl::Choice(|c| if c.shelf_captures { "on" } else { "off" }, |c, v| c.shelf_captures = v == "on", &[("on", "Oui", "Yes"), ("off", "Non", "No")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Position", "Position"),
        desc: ("Où l'îlot se place sur le bord haut.", "Where the island sits on the top edge."),
        ctl: Ctl::Choice(|c| &c.position, |c, v| c.position = v.into(), &[("left", "Gauche", "Left"), ("center", "Centre", "Center"), ("right", "Droite", "Right")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Forme", "Shape"),
        desc: ("Flottante et arrondie, ou encoche collée au bord.", "Floating and rounded, or a notch against the edge."),
        ctl: Ctl::Choice(|c| &c.shape, |c, v| c.shape = v.into(), &[("island", "Îlot", "Island"), ("notch", "Encoche", "Notch")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Marge du bord", "Edge margin"),
        desc: ("Distance au bord de l'écran, à gauche ou à droite.", "Distance from the screen edge, left or right."),
        ctl: Ctl::Slider { get: |c| c.edge_margin, set: |c, v| c.edge_margin = v, min: 0.0, max: 200.0, step: 1.0, unit: Unit::Px },
    },
    Row {
        section: 1,
        only: Only::Island,
        title: ("Distance du haut", "Top distance"),
        desc: ("Espace au-dessus de l'îlot.", "Space above the island."),
        ctl: Ctl::Slider { get: |c| c.top_distance, set: |c, v| c.top_distance = v, min: 0.0, max: 40.0, step: 1.0, unit: Unit::Px },
    },
    Row {
        section: 1,
        only: Only::Notch,
        title: ("Hauteur de l'encoche", "Notch height"),
        desc: ("Jusqu'où l'encoche descend depuis le haut de l'écran.", "How far the notch reaches down from the top of the screen."),
        ctl: Ctl::Slider { get: |c| c.notch_height, set: |c, v| c.notch_height = v, min: 14.0, max: 44.0, step: 1.0, unit: Unit::Px },
    },
    Row {
        section: 1,
        only: Only::Notch,
        title: ("Largeur de l'encoche", "Notch width"),
        desc: ("Sa largeur quand elle est fermée.", "Its width while closed."),
        ctl: Ctl::Slider { get: |c| c.notch_width, set: |c, v| c.notch_width = v, min: 60.0, max: 240.0, step: 1.0, unit: Unit::Px },
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Écran", "Display"),
        desc: ("Celui qui affiche l'îlot. S'il est débranché, l'écran principal prend le relais.", "The one showing the island. The primary display takes over if it is unplugged."),
        ctl: Ctl::Monitor,
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Un îlot par écran", "One island per display"),
        desc: ("Tous les écrans : chacun a son îlot, avec les mêmes réglages.", "All displays: each one gets its own island, same settings."),
        ctl: Ctl::Choice(|c| &c.islands, |c, v| c.islands = v.into(), &[("chosen", "Écran choisi", "Chosen display"), ("all", "Tous les écrans", "All displays")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Au repos, afficher", "When idle, show"),
        desc: ("Sur le bureau seulement. Devant une appli, l'îlot devient une barre fine.", "On the desktop only. In front of an app, the island becomes a thin bar."),
        ctl: Ctl::Choice(|c| &c.idle_show, |c, v| c.idle_show = v.into(), &[("nothing", "Rien", "Nothing"), ("clock", "Heure", "Time"), ("date", "Date", "Date")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("HUD", "HUD"),
        desc: ("Volume, luminosité, batterie et micro, un instant dans l'îlot fermé.", "Volume, brightness, battery and mic, briefly in the closed island."),
        ctl: Ctl::Choice(|c| &c.hud, |c, v| c.hud = v.into(), &[("all", "Tous", "All"), ("none", "Aucun", "None")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Touches de verrouillage", "Lock keys"),
        desc: ("Verr. maj., Verr. num. et Arrêt défil. dans le HUD.", "Caps, Num and Scroll Lock in the HUD."),
        ctl: Ctl::Choice(|c| if c.hud_locks { "on" } else { "off" }, |c, v| c.hud_locks = v == "on", &[("off", "Non", "No"), ("on", "Oui", "Yes")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Alertes batterie des appareils", "Device battery alerts"),
        desc: ("Souris, écouteurs, casque : 20 %, 10 %, connexion et déconnexion.", "Mouse, earbuds, headset: 20%, 10%, connect, disconnect."),
        ctl: Ctl::Choice(|c| if c.hud_devices { "on" } else { "off" }, |c, v| c.hud_devices = v == "on", &[("off", "Non", "No"), ("on", "Oui", "Yes")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Bande de jeu", "Game strip"),
        desc: ("CPU, GPU, RAM et réseau en surimpression devant une appli plein écran. Ctrl+Alt+G l'affiche ou la masque.", "CPU, GPU, RAM and network over a fullscreen app. Ctrl+Alt+G shows or hides it."),
        ctl: Ctl::Choice(|c| if c.game_strip { "on" } else { "off" }, |c, v| c.game_strip = v == "on", &[("off", "Non", "No"), ("on", "Oui", "Yes")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Coin de la bande de jeu", "Game strip corner"),
        desc: ("Où placer la bande, en haut de l'écran.", "Where the strip sits, at the top of the screen."),
        ctl: Ctl::Choice(|c| &c.game_strip_corner, |c, v| c.game_strip_corner = v.into(), &[("left", "Haut gauche", "Top left"), ("right", "Haut droite", "Top right")]),
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Droits pour les FPS", "Rights for FPS"),
        desc: (
            "Une fois : lancer la commande copiée dans un terminal admin, puis se reconnecter. Groupe anglais : Performance Log Users.",
            "Once: run the copied command in an admin terminal, then sign out and in. French group: Utilisateurs du journal de performances.",
        ),
        ctl: Ctl::FpsRights,
    },
    Row {
        section: 1,
        only: Only::Any,
        title: ("Durée des notifications", "Notification length"),
        desc: ("Temps d'affichage d'une notification Windows dans l'îlot fermé.", "How long a Windows notification stays in the closed island."),
        ctl: Ctl::Choice(
            |c| match c.notification_seconds {
                3 => "3",
                8 => "8",
                _ => "5",
            },
            |c, v| c.notification_seconds = v.parse().unwrap_or(5),
            &[("3", "3 s", "3 s"), ("5", "5 s", "5 s"), ("8", "8 s", "8 s")],
        ),
    },
    Row {
        section: 2,
        only: Only::Island,
        title: ("Taille de la pilule", "Pill size"),
        desc: ("L'îlot fermé et son contenu.", "The closed island and its content."),
        ctl: Ctl::Slider { get: |c| c.pill_scale, set: |c, v| c.pill_scale = v, min: 0.8, max: 1.5, step: 0.01, unit: Unit::Percent },
    },
    Row {
        section: 2,
        only: Only::Any,
        title: ("Taille de la carte", "Card size"),
        desc: ("La carte ouverte, son texte et ses commandes.", "The open card, its text and its controls."),
        ctl: Ctl::Slider { get: |c| c.card_scale, set: |c, v| c.card_scale = v, min: 0.8, max: 1.25, step: 0.01, unit: Unit::Percent },
    },
    Row {
        section: 2,
        only: Only::Any,
        title: ("Coins", "Corners"),
        desc: ("De carrés à entièrement arrondis.", "From square to fully rounded."),
        ctl: Ctl::Slider { get: |c| c.corners, set: |c, v| c.corners = v, min: 0.0, max: 1.0, step: 0.01, unit: Unit::Percent },
    },
    Row {
        section: 2,
        only: Only::Any,
        title: ("Surface", "Surface"),
        desc: ("Verre : fond translucide, plus clair, avec un liseré.", "Glass: translucent, lighter surface with a highlight."),
        ctl: Ctl::Choice(|c| &c.surface, |c, v| c.surface = v.into(), &[("opaque", "Opaque", "Opaque"), ("glass", "Verre", "Glass")]),
    },
    Row {
        section: 2,
        only: Only::Any,
        title: ("Opacité", "Opacity"),
        desc: ("Opacité de la surface en mode Verre.", "Surface opacity in Glass mode."),
        ctl: Ctl::Slider { get: |c| c.glass_opacity, set: |c, v| c.glass_opacity = v, min: 0.6, max: 1.0, step: 0.01, unit: Unit::Percent },
    },
    Row {
        section: 2,
        only: Only::Any,
        title: ("Couleur d'accent", "Accent color"),
        desc: ("Barres, curseurs et éléments actifs. Blanc par défaut.", "Bars, sliders and active items. White by default."),
        ctl: Ctl::Accent,
    },
    Row {
        section: 3,
        only: Only::Any,
        title: ("Vitesse d'animation", "Animation speed"),
        desc: ("La vitesse à laquelle l'îlot s'ouvre et se ferme.", "How fast the island opens and closes."),
        ctl: Ctl::Choice(|c| &c.anim_speed, |c, v| c.anim_speed = v.into(), &[("calm", "Tranquille", "Calm"), ("normal", "Normale", "Normal"), ("fast", "Rapide", "Fast")]),
    },
    Row {
        section: 3,
        only: Only::Any,
        title: ("Fluidité", "Smoothness"),
        desc: ("Économie limite les animations à 30 images par seconde. Équilibré le fait seulement sur batterie.", "Saver limits animations to 30 frames per second. Balanced only does so on battery."),
        ctl: Ctl::Choice(|c| &c.motion, |c, v| c.motion = v.into(), &[("smooth", "Fluide", "Smooth"), ("balanced", "Équilibré", "Balanced"), ("saver", "Économie", "Saver")]),
    },
    Row {
        section: 4,
        only: Only::Any,
        title: ("Ville de la météo", "Weather city"),
        desc: ("Tape une ville, puis Entrée. Seul son nom est envoyé à Open-Meteo.", "Type a city, then press Enter. Only its name is sent to Open-Meteo."),
        ctl: Ctl::City,
    },
    Row {
        section: 4,
        only: Only::Any,
        title: ("Agenda .ics", ".ics agenda"),
        desc: (
            "Adresse ou fichier .ics, lu en lecture seule dans l'agenda. Colle avec Ctrl+V, Entrée pour valider, champ vide pour retirer.",
            "Address or .ics file, shown read-only in the agenda. Paste with Ctrl+V, Enter to confirm, empty field to remove.",
        ),
        ctl: Ctl::IcsUrl,
    },
    Row {
        section: 4,
        only: Only::Any,
        title: ("Durée du focus", "Focus length"),
        desc: ("Temps de concentration du minuteur.", "Concentration time of the timer."),
        ctl: Ctl::Slider { get: |c| c.focus_minutes as f32, set: |c, v| c.focus_minutes = v as u32, min: 5.0, max: 90.0, step: 5.0, unit: Unit::Minutes },
    },
    Row {
        section: 4,
        only: Only::Any,
        title: ("Durée de la pause", "Break length"),
        desc: ("Temps de pause après le focus.", "Break time after a focus session."),
        ctl: Ctl::Slider { get: |c| c.break_minutes as f32, set: |c, v| c.break_minutes = v as u32, min: 1.0, max: 30.0, step: 1.0, unit: Unit::Minutes },
    },
    Row {
        section: 4,
        only: Only::Any,
        title: ("Widget sur le bureau", "Desktop widget"),
        desc: ("Un widget posé sur le bureau, déplaçable. Sa croix le retire.", "One widget placed on the desktop, draggable. Its cross removes it."),
        ctl: Ctl::Choice(
            |c| &c.desktop_widget,
            |c, v| c.desktop_widget = v.into(),
            &[("none", "Non", "Off"), ("focus", "Focus", "Focus"), ("elapsed", "Écoulé", "Elapsed"), ("batteries", "Batt.", "Battery"), ("clock", "Heure", "Clock")],
        ),
    },
    Row {
        section: 5,
        only: Only::Any,
        title: ("Version", "Version"),
        desc: ("naelbar, logiciel libre sous licence MIT.", "naelbar, free software under the MIT license."),
        ctl: Ctl::Version,
    },
    Row {
        section: 5,
        only: Only::Any,
        title: ("Mises à jour automatiques", "Automatic updates"),
        desc: ("Une fois par jour, installe la dernière version publiée sur GitHub et redémarre l'îlot.", "Once a day, installs the latest version published on GitHub and restarts the island."),
        ctl: Ctl::Choice(
            |c| if c.auto_update { "on" } else { "off" },
            |c, v| c.auto_update = v == "on",
            &[("off", "Non", "No"), ("on", "Oui", "Yes")],
        ),
    },
    Row {
        section: 5,
        only: Only::Any,
        title: ("Fichier de configuration", "Configuration file"),
        desc: ("Tous ces réglages, au format JSON.", "All of these settings, as JSON."),
        ctl: Ctl::OpenConfig,
    },
    Row {
        section: MUSIC_SECTION,
        only: Only::Any,
        title: ("Extension YouTube Music", "YouTube Music extension"),
        desc: ("La télécommande du téléphone. naelbar la tient à jour : charge son dossier une fois dans le navigateur (Mode développeur, Charger l'extension non empaquetée).", "The phone remote. naelbar keeps it up to date: load its folder once in the browser (Developer mode, Load unpacked)."),
        ctl: Ctl::YtmExtension,
    },
    Row {
        section: MUSIC_SECTION,
        only: Only::Any,
        title: ("Visualiseur", "Visualizer"),
        desc: ("Dans l'îlot fermé, pendant la lecture.", "In the closed island, while music plays."),
        ctl: Ctl::Choice(|c| &c.visualizer, |c, v| c.visualizer = v.into(), &[("bars", "Barres", "Bars"), ("pixels", "Pixels", "Pixels"), ("none", "Aucun", "None")]),
    },
    Row {
        section: MUSIC_SECTION,
        only: Only::Any,
        title: ("Priorité", "Priority"),
        desc: ("L'appli affichée quand plusieurs jouent.", "The app shown when several are playing."),
        ctl: Ctl::Choice(
            |c| if c.media_preferred.is_empty() { "recent" } else { "preferred" },
            |c, v| c.media_preferred = if v == "recent" { String::new() } else { crate::views::media::first_enabled(c) },
            &[("recent", "La plus récente", "Most recent"), ("preferred", "App préférée", "Preferred app")],
        ),
    },
    Row {
        section: ACTIVITY_SECTION,
        only: Only::Any,
        title: ("Téléchargements", "Downloads"),
        desc: ("Nom et taille d'un téléchargement de n'importe quel navigateur, dans l'îlot fermé.", "Name and size of a download from any browser, in the closed island."),
        ctl: Ctl::Choice(|c| if c.activity_downloads { "on" } else { "off" }, |c, v| c.activity_downloads = v == "on", &[("on", "Oui", "Yes"), ("off", "Non", "No")]),
    },
    Row {
        section: ACTIVITY_SECTION,
        only: Only::Any,
        title: ("Micro, caméra, écran", "Mic, camera, screen"),
        desc: ("Un point vert, orange ou bleu quand une appli utilise l'un d'eux.", "A green, orange or blue dot when an app uses one of them."),
        ctl: Ctl::Choice(|c| if c.activity_indicators { "on" } else { "off" }, |c, v| c.activity_indicators = v == "on", &[("on", "Oui", "Yes"), ("off", "Non", "No")]),
    },
    Row {
        section: ACTIVITY_SECTION,
        only: Only::Any,
        title: ("Applications ignorées", "Ignored apps"),
        desc: ("Leurs notifications, indicateurs et minuteur du micro sont masqués. Nom de l'exécutable (ex. Discord) ou AUMID.", "Their notifications, indicators and mic timer are hidden. Executable name (e.g. Discord) or AUMID."),
        ctl: Ctl::IgnoreApps,
    },
    Row {
        section: PHONE_SECTION,
        only: Only::Any,
        title: ("Lien avec le téléphone", "Phone link"),
        desc: ("Sur le même Wi-Fi, le site débloque souris, clavier et envoi de fichiers (dans Téléchargements\\naelbar).", "On the same Wi-Fi, the site unlocks mouse, keyboard and file sending (to Downloads\\naelbar)."),
        ctl: Ctl::Choice(
            |c| if c.phone_remote { "on" } else { "off" },
            |c, v| c.phone_remote = v == "on",
            &[("off", "Non", "No"), ("on", "Oui", "Yes")],
        ),
    },
    Row {
        section: PHONE_SECTION,
        only: Only::Any,
        title: ("Compte Google", "Google account"),
        desc: ("Le même compte sur naelbar et sur le site du téléphone : agenda, post-its, couleurs et lien.", "The same account in naelbar and on the phone site: agenda, notes, colours and link."),
        ctl: Ctl::GoogleAccount,
    },
    Row {
        section: PHONE_SECTION,
        only: Only::Any,
        title: ("Dossier des cours", "Lessons folder"),
        desc: ("Où ranger les cours scannés depuis le téléphone. Sinon, Téléchargements\\naelbar.", "Where lessons scanned on the phone are filed. Otherwise Downloads\\naelbar."),
        ctl: Ctl::CoursesRoot,
    },
];

/// Installing the extension, and what came of it.
static EXT_STATUS: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn install_extension(hwnd: HWND) {
    *EXT_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = tr("Téléchargement…", "Downloading…").into();
    let hwnd = hwnd.0 as isize;
    std::thread::spawn(move || {
        let msg = match crate::ytmext::refresh(true) {
            Ok(_) => {
                crate::ytmext::reveal();
                tr("Installée, chemin copié", "Installed, path copied").to_string()
            }
            Err(_) => tr("Pas de connexion à GitHub", "Cannot reach GitHub").to_string(),
        };
        *EXT_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = msg;
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(HWND(hwnd as *mut _)), None, false);
        }
    });
}

/// What the "Check" button found, shown next to the version.
static UPDATE_STATUS: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Asks GitHub from here, to say at once whether there is something new; the
/// island does the install (it is the one that restarts).
fn check_update(hwnd: HWND) {
    let set = |s: String| *UPDATE_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = s;
    set(tr("Recherche…", "Checking…").into());
    let hwnd = hwnd.0 as isize;
    std::thread::spawn(move || {
        // The extension too, when naelbar looks after it.
        let _ = crate::ytmext::refresh(false);
        let msg = match nb_integrations::check_update(env!("CARGO_PKG_VERSION")) {
            Ok(None) => tr("À jour", "Up to date").to_string(),
            Ok(Some(r)) => unsafe {
                match FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
                    Ok(island) => {
                        let _ = PostMessageW(Some(island), crate::updater::WM_UPDATE_NOW, WPARAM(0), LPARAM(0));
                        format!("{} {}…", tr("Installation de la", "Installing"), r.version)
                    }
                    Err(_) => format!("{} {}", r.version, tr("disponible, lance naelbar", "available, start naelbar")),
                }
            },
            Err(_) => tr("Pas de connexion à GitHub", "Cannot reach GitHub").to_string(),
        };
        *UPDATE_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = msg;
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(HWND(hwnd as *mut _)), None, false);
        }
    });
}

static ACCOUNT_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Runs the sign-in window or the sign-out, then has the island sync and
/// this page redraw. A thread only waits for the child.
fn account_action(hwnd: HWND, flag: &'static str) {
    let Ok(exe) = std::env::current_exe() else { return };
    let Ok(mut child) = std::process::Command::new(exe).arg(flag).spawn() else { return };
    ACCOUNT_BUSY.store(true, std::sync::atomic::Ordering::Release);
    let hwnd = hwnd.0 as isize;
    std::thread::spawn(move || {
        let _ = child.wait();
        ACCOUNT_BUSY.store(false, std::sync::atomic::Ordering::Release);
        unsafe {
            if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
                let _ = PostMessageW(Some(island), crate::agenda::WM_AGENDA_CHANGED, WPARAM(0), LPARAM(0));
            }
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(HWND(hwnd as *mut _)), None, false);
        }
    });
}

/// The system folder picker. Blocks this window until it closes.
fn pick_folder(owner: HWND) -> Option<String> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
    use windows::Win32::UI::Shell::{FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dialog.GetOptions().ok()?;
        dialog.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        dialog.Show(Some(owner)).ok()?;
        let item = dialog.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// Lowercases and removes French accents, so "ecran" finds "Écran".
fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            c => c,
        })
        .collect()
}

fn row_matches(r: &Row, query: &str) -> bool {
    let s = SECTIONS[r.section];
    [r.title.0, r.title.1, r.desc.0, r.desc.1, s.0, s.1].iter().any(|t| fold(t).contains(query))
}

fn snap_value(v: f32, min: f32, max: f32, step: f32) -> f32 {
    (min + ((v - min) / step).round() * step).clamp(min, max)
}

fn unit_label(v: f32, unit: Unit) -> String {
    match unit {
        Unit::Px => format!("{} px", v.round() as i32),
        Unit::Percent => format!("{} %", (v * 100.0).round() as i32),
        Unit::Minutes => format!("{} min", v.round() as i32),
    }
}

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("naelbar");

fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None).is_ok() }
}

fn set_autostart(on: bool) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key).is_err() {
            return;
        }
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let command = format!("\"{}\"", exe.display());
                let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
                let _ = RegSetValueExW(key, RUN_VALUE, None, REG_SZ, Some(bytes));
            }
        } else {
            let _ = RegDeleteValueW(key, RUN_VALUE);
        }
        let _ = RegCloseKey(key);
    }
}

fn start_menu_link() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("APPDATA")?).join("Microsoft").join("Windows").join("Start Menu").join("Programs");
    Some(dir.join("naelbar.lnk"))
}

fn start_menu_enabled() -> bool {
    start_menu_link().is_some_and(|p| p.exists())
}

/// Creates or removes `naelbar.lnk` in the current user's Start menu.
fn set_start_menu(on: bool) {
    let Some(link) = start_menu_link() else { return };
    if !on {
        let _ = std::fs::remove_file(link);
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    unsafe {
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile};
        use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
        let Ok(shell) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) else { return };
        let target = HSTRING::from(exe.as_os_str());
        let _ = shell.SetPath(&target);
        if let Some(dir) = exe.parent() {
            let _ = shell.SetWorkingDirectory(&HSTRING::from(dir.as_os_str()));
        }
        let _ = shell.SetDescription(w!("naelbar"));
        let _ = shell.SetIconLocation(&target, 0);
        if let Ok(file) = windows::core::Interface::cast::<IPersistFile>(&shell) {
            let _ = file.Save(&HSTRING::from(link.as_os_str()), true);
        }
    }
}

fn open_config_file() {
    let Some(path) = config::path() else { return };
    if !path.exists() {
        Config::load().save();
    }
    unsafe {
        let _ = ShellExecuteW(None, w!("open"), &HSTRING::from(path.as_os_str()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Search,
    City,
    Ics,
    DockSearch,
}

type Geocoded = std::result::Result<(f64, f64, String), ()>;
static GEOCODED: Mutex<Option<Geocoded>> = Mutex::new(None);

struct Settings {
    hwnd: HWND,
    renderer: Renderer,
    cfg: Config,
    input: Input,
    widgets: WidgetState,
    last: Vec<Cmd>,
    scale: f32,
    section: usize,
    search: String,
    city: String,
    city_status: String,
    ics: String,
    focus: Focus,
    scroll: f32,
    dirty: bool,
    tracking: bool,
    autostart: bool,
    start_menu: bool,
    monitors: Vec<Monitor>,
    dock: DockPage,
    dock_query: String,
}

thread_local! {
    static STATE: RefCell<Option<Settings>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Settings) -> R) -> Option<R> {
    STATE.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn text_field(ui: &mut Ui, w: Id, r: Rect, text: &str, hint: &str, focused: bool) -> bool {
    let resp = ui.interact(w, r);
    ui.fill(r, R_SMALL, if resp.hovered { TILE_HOVER } else { TILE });
    if focused {
        ui.stroke(r.inset(0.5), R_SMALL, 1.0, TEXT_FAINT);
    }
    let inner = r.inset_xy(12.0, 0.0);
    if text.is_empty() {
        ui.text(inner, hint, TextStyle::new(13.0, 400, TEXT_FAINT));
    } else {
        let shown = if focused { format!("{text}|") } else { text.to_string() };
        ui.text(inner, &shown, TextStyle::new(13.0, 400, TEXT));
    }
    resp.clicked
}

fn switch(ui: &mut Ui, w: Id, r: Rect, on: bool) -> bool {
    let track = Rect::new(r.right() - 42.0, r.center().y - 12.0, 42.0, 24.0);
    let resp = ui.interact(w, track);
    ui.fill(track, 12.0, if on { accent() } else if resp.hovered { TILE_HOVER } else { TILE });
    let knob_x = if on { track.right() - 21.0 } else { track.x + 3.0 };
    ui.fill(Rect::new(knob_x, track.y + 3.0, 18.0, 18.0), 9.0, if on { SURFACE } else { TEXT_DIM });
    resp.clicked
}

fn button(ui: &mut Ui, w: Id, r: Rect, label: &str) -> bool {
    let resp = ui.interact(w, r);
    ui.fill(r, R_SMALL, if resp.hovered { TILE_HOVER } else { TILE });
    ui.text(r, label, TextStyle::new(13.0, 500, TEXT).aligned(Align::Center));
    resp.clicked
}

impl Settings {
    fn persist(&mut self) {
        // The island owns the last tab; never overwrite it with a stale copy.
        let disk = Config::load();
        self.cfg.last_tab = disk.last_tab;
        // The ignore list is edited in its own window: keep what it saved.
        self.cfg.ignore_apps = disk.ignore_apps;
        // Lesson colours are chosen in the agenda window.
        self.cfg.course_styles = disk.course_styles;
        self.cfg.course_styles_at = disk.course_styles_at;
        // The island adds the media apps it meets.
        crate::views::media::merge_seen(&mut self.cfg.media_seen, &disk.media_seen);
        // Targets are typed in the editor process: its file is the newer one.
        for (mine, saved) in self.cfg.slots.iter_mut().zip(disk.slots) {
            mine.target = saved.target;
        }
        self.cfg.save();
        config::apply_appearance(&self.cfg);
        unsafe {
            if let Ok(island) = FindWindowW(w!("naelbar.window"), w!("naelbar control")) {
                let _ = PostMessageW(Some(island), WM_CONFIG_CHANGED, WPARAM(0), LPARAM(0));
            }
        }
        self.dirty = true;
    }

    fn geocode(&mut self) {
        let name = self.city.trim().to_string();
        if name.is_empty() {
            self.cfg.weather_city.clear();
            self.cfg.weather_location = None;
            self.city_status.clear();
            self.persist();
            return;
        }
        self.city_status = tr("Recherche…", "Searching…").to_string();
        let hwnd = self.hwnd.0 as isize;
        std::thread::spawn(move || {
            let result = nb_integrations::geocode_blocking(&name).map_err(|_| ());
            *GEOCODED.lock().unwrap() = Some(result);
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_GEOCODED, WPARAM(0), LPARAM(0));
            }
        });
    }

    fn on_geocoded(&mut self) {
        match GEOCODED.lock().unwrap().take() {
            Some(Ok((lat, lon, label))) => {
                self.cfg.weather_location = Some((lat, lon));
                self.cfg.weather_city = label.clone();
                self.city = label;
                self.city_status = tr("Ville enregistrée", "City saved").to_string();
                self.persist();
            }
            Some(Err(())) => self.city_status = tr("Ville introuvable", "City not found").to_string(),
            None => {}
        }
        self.dirty = true;
    }

    fn on_char(&mut self, c: u32) {
        let target = match self.focus {
            Focus::City => &mut self.city,
            Focus::Ics => &mut self.ics,
            Focus::DockSearch => &mut self.dock_query,
            Focus::Search => &mut self.search,
        };
        match c {
            8 => {
                target.pop();
            }
            13 => {
                if self.focus == Focus::City {
                    self.geocode();
                } else if self.focus == Focus::Ics {
                    self.cfg.ics_url = self.ics.trim().to_string();
                    self.persist();
                }
            }
            22 => {
                if let Some(text) = crate::widgets::clip::get_text(self.hwnd, 2048) {
                    target.extend(text.chars().filter(|c| !c.is_control()));
                }
            }
            27 => {
                target.clear();
            }
            c if c >= 32 => {
                if let Some(ch) = char::from_u32(c) {
                    target.push(ch);
                }
            }
            _ => {}
        }
        if matches!(self.focus, Focus::Search | Focus::DockSearch) {
            self.scroll = 0.0;
        }
        self.dirty = true;
    }

    /// Returns true if a frame went to the screen.
    fn frame(&mut self) -> bool {
        self.dirty = false;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let mut out = Vec::new();
        let mut save = false;
        let mut field_clicked = false;
        let mut ui = Ui::new(&self.input, &mut self.widgets, &mut out, self.scale, now);

        ui.fill(Rect::new(0.0, 0.0, W, H), 0.0, SURFACE);
        ui.fill(Rect::new(NAV_W, 0.0, 1.0, H), 0.0, HAIRLINE);

        // Navigation.
        let search_rect = Rect::new(14.0, 16.0, NAV_W - 28.0, 36.0);
        if text_field(&mut ui, id("search", 0), search_rect, &self.search, tr("Rechercher", "Search"), self.focus == Focus::Search) {
            self.focus = Focus::Search;
            field_clicked = true;
        }
        let searching = !self.search.trim().is_empty();
        for (i, name) in SECTIONS.iter().enumerate() {
            let r = Rect::new(10.0, 64.0 + i as f32 * 38.0, NAV_W - 20.0, 34.0);
            let resp = ui.interact(id("nav", i), r);
            let on = !searching && self.section == i;
            if on {
                ui.fill(r, R_SMALL, TILE_HOVER);
            } else if resp.hovered {
                ui.fill(r, R_SMALL, TILE);
            }
            ui.text(r.inset_xy(12.0, 0.0), tr(name.0, name.1), TextStyle::new(13.5, if on { 600 } else { 400 }, if on { TEXT } else { TEXT_DIM }));
            if resp.clicked {
                self.section = i;
                self.search.clear();
                self.scroll = 0.0;
            }
        }

        // Page.
        let query = fold(self.search.trim());
        let rows: Vec<(usize, &Row)> = ROWS.iter().enumerate().filter(|(_, r)| r.only.applies(&self.cfg) && if query.is_empty() { r.section == self.section } else { row_matches(r, &query) }).collect();
        let view = Rect::new(NAV_W + 1.0, 0.0, W - NAV_W - 1.0, H);
        let on_dock = !searching && self.section == dock_section();
        let on_launcher = !searching && self.section == launcher_section();
        if on_dock && !self.dock.started {
            self.dock.started = true;
            let hwnd = self.hwnd.0 as isize;
            std::thread::spawn(move || {
                let names: Vec<String> = crate::dock::with_com(crate::dock::scan_apps).into_iter().map(|a| a.name).collect();
                *DOCK_APPS.lock().unwrap_or_else(|e| e.into_inner()) = Some(names);
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_DOCK_SCANNED, WPARAM(0), LPARAM(0));
                }
            });
        }
        let dock_query = fold(self.dock_query.trim());
        let dock_rows: Vec<usize> = if on_dock { self.dock.apps.iter().enumerate().filter(|(_, n)| dock_query.is_empty() || fold(n).contains(&dock_query)).map(|(i, _)| i).collect() } else { Vec::new() };
        let content = if on_launcher { 24.0 + 52.0 + crate::launcher::SLOT_COUNT as f32 * LAUNCH_ROW + 24.0 } else if on_dock { 24.0 + 52.0 + ROW_H + 52.0 + 30.0 + dock_rows.len() as f32 * DOCK_ROW + 24.0 } else { 24.0 + 52.0 + rows.len() as f32 * ROW_H + 24.0 };
        let on_music = !searching && self.section == MUSIC_SECTION;
        let content = if on_music { content + crate::views::media::settings_list_height(&self.cfg) } else { content };
        let on_phone = !searching && self.section == PHONE_SECTION;
        let content = if on_phone { content + crate::phone::page::AGENDA_HEIGHT } else { content };
        let max_scroll = (content - H).max(0.0);
        if ui.hovered(view) && ui.input.wheel != 0.0 {
            self.scroll -= ui.input.wheel * 60.0;
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);

        let x = view.x + PAGE_PAD;
        let cw = view.w - 2.0 * PAGE_PAD;
        ui.push_clip(view, 0.0);
        let mut y = 24.0 - self.scroll;
        let page_title = if searching { tr("Résultats", "Results") } else { tr(SECTIONS[self.section].0, SECTIONS[self.section].1) };
        ui.text(Rect::new(x, y, cw, 40.0), page_title, TextStyle::new(26.0, 700, TEXT));
        y += 52.0;

        if rows.is_empty() && !on_dock && !on_launcher {
            ui.text(Rect::new(x, y, cw, 30.0), tr("Aucun réglage trouvé.", "No setting found."), TextStyle::new(13.5, 400, TEXT_DIM));
        }

        for (pos, (index, row)) in rows.iter().enumerate() {
            let rect = Rect::new(x, y, cw, ROW_H);
            y += ROW_H;
            if rect.bottom() < 0.0 || rect.y > H {
                continue;
            }
            if pos > 0 {
                ui.fill(Rect::new(x, rect.y, cw, 1.0), 0.0, HAIRLINE);
            }
            let text_w = cw - CTRL_W - 24.0;
            let title = if searching { format!("{} · {}", tr(SECTIONS[row.section].0, SECTIONS[row.section].1), tr(row.title.0, row.title.1)) } else { tr(row.title.0, row.title.1).to_string() };
            ui.text(Rect::new(x, rect.y + 11.0, text_w, 20.0), &title, TextStyle::new(14.0, 500, TEXT));
            let is_city = matches!(row.ctl, Ctl::City);
            let desc = if is_city && !self.city_status.is_empty() { self.city_status.as_str() } else { tr(row.desc.0, row.desc.1) };
            ui.text(Rect::new(x, rect.y + 33.0, text_w, 36.0), desc, TextStyle::new(12.5, 400, TEXT_DIM).top().wrapped());

            let ctrl = Rect::new(rect.right() - CTRL_W, rect.y + (ROW_H - 34.0) * 0.5, CTRL_W, 34.0);
            let w = id("row", *index);
            match &row.ctl {
                Ctl::Choice(get, set, options) => {
                    let current = get(&self.cfg).to_string();
                    let labels: Vec<&str> = options.iter().map(|o| tr(o.1, o.2)).collect();
                    let selected = options.iter().position(|o| o.0 == current).unwrap_or(0);
                    if let Some(i) = ui.segmented(w, ctrl, &labels, selected) {
                        if i != selected {
                            set(&mut self.cfg, options[i].0);
                            save = true;
                        }
                    }
                }
                Ctl::Slider { get, set, min, max, step, unit } => {
                    let value = get(&self.cfg);
                    let mut area = ctrl;
                    let label = area.take_right(58.0, 10.0);
                    ui.text(label, &unit_label(value, *unit), TextStyle::new(12.5, 500, TEXT_DIM).aligned(Align::End));
                    if let Some(v) = ui.slider_with_knob(w, area, (value - min) / (max - min)) {
                        let snapped = snap_value(min + v * (max - min), *min, *max, *step);
                        if (snapped - value).abs() > f32::EPSILON {
                            set(&mut self.cfg, snapped);
                            save = true;
                        }
                    }
                }
                Ctl::Autostart => {
                    if switch(&mut ui, w, ctrl, self.autostart) {
                        self.autostart = !self.autostart;
                        set_autostart(self.autostart);
                        self.autostart = autostart_enabled();
                    }
                }
                Ctl::StartMenu => {
                    if switch(&mut ui, w, ctrl, self.start_menu) {
                        set_start_menu(!self.start_menu);
                        self.start_menu = start_menu_enabled();
                    }
                }
                Ctl::Monitor => {
                    let mut labels = vec![tr("Principal", "Primary").to_string()];
                    let mut values = vec![String::new()];
                    for (i, m) in self.monitors.iter().enumerate().take(3) {
                        labels.push(format!("{} {}", tr("Écran", "Display"), i + 1));
                        values.push(m.name.clone());
                    }
                    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                    let selected = values.iter().position(|v| *v == self.cfg.monitor).unwrap_or(0);
                    if let Some(i) = ui.segmented(w, ctrl, &refs, selected) {
                        if i != selected {
                            self.cfg.monitor = values[i].clone();
                            save = true;
                        }
                    }
                }
                Ctl::Accent => {
                    let current = self.cfg.accent.to_ascii_uppercase();
                    for (i, hex) in ACCENTS.iter().enumerate() {
                        let swatch = Rect::new(ctrl.right() - 216.0 + i as f32 * 32.0, ctrl.center().y - 12.0, 24.0, 24.0);
                        let resp = ui.interact(crate::ui::id("accent", i), swatch);
                        let rgb = parse_hex(hex).unwrap_or(DEFAULT_ACCENT);
                        let color = crate::ui::Color::rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
                        ui.fill(swatch, 12.0, color);
                        if *hex == current {
                            ui.stroke(swatch.inset(-3.0), 15.0, 1.5, TEXT);
                        } else if resp.hovered {
                            ui.stroke(swatch.inset(-3.0), 15.0, 1.0, TEXT_FAINT);
                        }
                        if resp.clicked && *hex != current {
                            self.cfg.accent = hex.to_string();
                            save = true;
                        }
                    }
                }
                Ctl::City => {
                    if text_field(&mut ui, w, ctrl, &self.city, tr("Par exemple Blois", "For example Paris"), self.focus == Focus::City) {
                        self.focus = Focus::City;
                        field_clicked = true;
                    }
                }
                Ctl::IcsUrl => {
                    let shown = if self.ics.chars().count() > 30 { format!("…{}", self.ics.chars().skip(self.ics.chars().count() - 29).collect::<String>()) } else { self.ics.clone() };
                    if text_field(&mut ui, w, ctrl, &shown, "https://…", self.focus == Focus::Ics) {
                        self.focus = Focus::Ics;
                        field_clicked = true;
                    }
                }
                Ctl::OpenConfig => {
                    let r = Rect::new(ctrl.right() - 90.0, ctrl.y, 90.0, ctrl.h);
                    if button(&mut ui, w, r, tr("Ouvrir", "Open")) {
                        open_config_file();
                    }
                }
                Ctl::IgnoreApps => {
                    let r = Rect::new(ctrl.right() - 110.0, ctrl.y, 110.0, ctrl.h);
                    let count = self.cfg.ignore_apps.len();
                    ui.text(Rect::new(ctrl.x, ctrl.y, ctrl.w - 122.0, ctrl.h), &count.to_string(), TextStyle::new(13.0, 500, TEXT_DIM).aligned(Align::End));
                    if button(&mut ui, w, r, tr("Modifier", "Edit")) {
                        if let Ok(exe) = std::env::current_exe() {
                            let _ = std::process::Command::new(exe).arg("--edit=ignore").spawn();
                        }
                    }
                }
                Ctl::GoogleAccount => {
                    let r = Rect::new(ctrl.right() - 120.0, ctrl.y, 120.0, ctrl.h);
                    let busy = ACCOUNT_BUSY.load(std::sync::atomic::Ordering::Acquire);
                    let state = crate::agendasync::signed_in();
                    let label = if busy { tr("Patiente…", "Please wait…") } else if state == Some(true) { tr("Connecté", "Signed in") } else { tr("Non connecté", "Signed out") };
                    ui.text(Rect::new(ctrl.x, ctrl.y, ctrl.w - 132.0, ctrl.h), label, TextStyle::new(13.0, 500, TEXT_DIM).aligned(Align::End));
                    let action = if state == Some(true) { tr("Déconnecter", "Sign out") } else { tr("Se connecter", "Sign in") };
                    if button(&mut ui, w, r, action) && !busy {
                        account_action(self.hwnd, if state == Some(true) { "--agenda-logout" } else { "--agenda-login" });
                    }
                }
                Ctl::CoursesRoot => {
                    let r = Rect::new(ctrl.right() - 90.0, ctrl.y, 90.0, ctrl.h);
                    let root = self.cfg.courses_root.trim_end_matches(['\\', '/']);
                    let name = if root.is_empty() { tr("Téléchargements", "Downloads") } else { root.rsplit(['\\', '/']).next().unwrap_or(root) };
                    ui.text(Rect::new(ctrl.x, ctrl.y, ctrl.w - 102.0, ctrl.h), name, TextStyle::new(13.0, 500, TEXT_DIM).aligned(Align::End));
                    if button(&mut ui, w, r, tr("Choisir", "Choose")) {
                        if let Some(p) = pick_folder(self.hwnd) {
                            self.cfg.courses_root = p;
                            save = true;
                        }
                    }
                }
                Ctl::YtmExtension => {
                    let r = Rect::new(ctrl.right() - 110.0, ctrl.y, 110.0, ctrl.h);
                    let installed = crate::ytmext::installed_version();
                    let status = EXT_STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let label = if !status.is_empty() { status } else { installed.clone().unwrap_or_else(|| tr("Non installée", "Not installed").into()) };
                    ui.text(Rect::new(ctrl.x - 120.0, ctrl.y, ctrl.w + 120.0 - 122.0, ctrl.h), &label, TextStyle::new(13.0, 500, TEXT_DIM).aligned(Align::End));
                    if button(&mut ui, w, r, if installed.is_some() { tr("Dossier", "Folder") } else { tr("Installer", "Install") }) {
                        if let Some(d) = crate::ytmext::dir() {
                            crate::widgets::clip::set_text(self.hwnd, &d.to_string_lossy());
                        }
                        if installed.is_some() {
                            crate::ytmext::reveal();
                            *EXT_STATUS.lock().unwrap_or_else(|e| e.into_inner()) = tr("Chemin copié", "Path copied").into();
                        } else {
                            install_extension(self.hwnd);
                        }
                    }
                }
                Ctl::Version => {
                    let r = Rect::new(ctrl.right() - 110.0, ctrl.y, 110.0, ctrl.h);
                    let status = UPDATE_STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let label = if status.is_empty() { env!("CARGO_PKG_VERSION").to_string() } else { format!("{} · {status}", env!("CARGO_PKG_VERSION")) };
                    ui.text(Rect::new(ctrl.x - 120.0, ctrl.y, ctrl.w + 120.0 - 122.0, ctrl.h), &label, TextStyle::new(13.0, 500, TEXT_DIM).aligned(Align::End));
                    if button(&mut ui, w, r, tr("Rechercher", "Check")) {
                        check_update(self.hwnd);
                    }
                }
                Ctl::FpsRights => {
                    // Copied only; the user runs it in an admin terminal.
                    let r = Rect::new(ctrl.right() - 110.0, ctrl.y, 110.0, ctrl.h);
                    if button(&mut ui, w, r, tr("Copier", "Copy")) {
                        let cmd = tr(
                            "net localgroup \"Utilisateurs du journal de performances\" %USERNAME% /add",
                            "net localgroup \"Performance Log Users\" %USERNAME% /add",
                        );
                        crate::widgets::clip::set_text(self.hwnd, cmd);
                    }
                }
            }
        }
        if on_dock {
            let text_w = cw - CTRL_W - 24.0;
            ui.text(Rect::new(x, y + 11.0, text_w, 20.0), tr("Favoris du navigateur", "Browser bookmarks"), TextStyle::new(14.0, 500, TEXT));
            ui.text(Rect::new(x, y + 33.0, text_w, 36.0), tr("La barre de favoris de Brave, sous les applications.", "Brave's bookmark bar, shown under the apps."), TextStyle::new(12.5, 400, TEXT_DIM).top().wrapped());
            let ctrl = Rect::new(x + cw - CTRL_W, y + (ROW_H - 34.0) * 0.5, CTRL_W, 34.0);
            let selected = usize::from(self.dock.store.bookmarks == "none");
            if let Some(i) = ui.segmented(id("dock.source", 0), ctrl, &["Brave", tr("Aucun", "None")], selected) {
                if i != selected {
                    self.dock.store.bookmarks = if i == 1 { "none" } else { "brave" }.into();
                    self.dock.store.save();
                }
            }
            y += ROW_H;
            ui.fill(Rect::new(x, y, cw, 1.0), 0.0, HAIRLINE);
            let search = Rect::new(x, y + 12.0, cw, 36.0);
            if text_field(&mut ui, id("dock.search", 0), search, &self.dock_query, tr("Rechercher une application", "Search apps"), self.focus == Focus::DockSearch) {
                self.focus = Focus::DockSearch;
                field_clicked = true;
            }
            y += 52.0;
            let note = if !self.dock.started || self.dock.apps.is_empty() { tr("Recherche des applications…", "Looking for apps…") } else if dock_rows.is_empty() { tr("Aucune application trouvée.", "No app found.") } else { tr("Active pour épingler dans le dock.", "Switch on to pin to the dock.") };
            ui.text(Rect::new(x, y, cw, 24.0), note, TextStyle::new(12.5, 400, TEXT_DIM));
            y += 30.0;
            let names: Vec<&str> = self.dock.apps.iter().map(String::as_str).collect();
            let mut toggled = None;
            for &i in &dock_rows {
                let r = Rect::new(x, y, cw, DOCK_ROW);
                y += DOCK_ROW;
                if r.bottom() < 0.0 || r.y > H {
                    continue;
                }
                ui.text(Rect::new(x, r.y, cw - 60.0, DOCK_ROW), names[i], TextStyle::new(13.5, 400, TEXT));
                if switch(&mut ui, id("dock.pin", i), r, self.dock.store.is_pinned(&names, i)) {
                    toggled = Some(i);
                }
            }
            if let Some(i) = toggled {
                self.dock.store.normalize(&names);
                self.dock.store.toggle(&names, i);
                self.dock.store.save();
            }
        }
        if on_launcher {
            for i in 0..self.cfg.slots.len().min(crate::launcher::SLOT_COUNT) {
                let kind = self.cfg.slots[i].kind();
                let r = Rect::new(x, y, cw, LAUNCH_ROW);
                y += LAUNCH_ROW;
                if r.bottom() < 0.0 || r.y > H {
                    continue;
                }
                ui.text(Rect::new(r.x, r.y, 18.0, r.h), &(i + 1).to_string(), TextStyle::new(13.0, 500, TEXT_DIM));
                let kind_rect = Rect::new(r.x + 22.0, r.y + 6.0, 128.0, 34.0);
                if button(&mut ui, id("launch.kind", i), kind_rect, kind.label()) {
                    self.cfg.slots[i].kind = kind.next().id().into();
                    save = true;
                }
                let field = Rect::new(kind_rect.right() + 8.0, kind_rect.y, (r.right() - kind_rect.right() - 8.0).max(40.0), 34.0);
                let resp = ui.interact(id("launch.target", i), field);
                let editable = kind.uses_target();
                ui.fill(field, R_SMALL, if resp.hovered && editable { TILE_HOVER } else { TILE });
                let target = self.cfg.slots[i].target.clone();
                let (text, color) = if !editable {
                    (tr("Aucune cible", "No target").to_string(), TEXT_FAINT)
                } else if target.is_empty() {
                    (kind.hint().to_string(), TEXT_FAINT)
                } else {
                    (short_tail(&target, 30), TEXT)
                };
                ui.text(field.inset_xy(12.0, 0.0), &text, TextStyle::new(13.0, 400, color));
                if resp.clicked && editable {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe).arg(format!("--edit=slot:{}", i + 1)).spawn();
                    }
                }
            }
        }
        if on_music && crate::views::media::settings_list(&mut ui, &mut self.cfg, x, y, cw, H, switch) {
            save = true;
        }
        if on_phone {
            crate::phone::page::draw_agenda(&mut ui, x, y, cw, H);
        }
        ui.pop_clip();

        if max_scroll > 0.0 {
            let track_h = H - 16.0;
            let bar_h = (H / content * track_h).max(30.0);
            let bar_y = 8.0 + (self.scroll / max_scroll) * (track_h - bar_h);
            ui.fill(Rect::new(W - 7.0, bar_y, 3.0, bar_h), 1.5, TILE_HOVER);
        }

        let cmds = std::mem::take(&mut ui.cmds);
        drop(ui);
        if self.input.released && !field_clicked && self.input.mouse.is_some() && !matches!(self.focus, Focus::Search) {
            self.focus = Focus::Search;
            self.dirty = true;
        }
        self.input.pressed = false;
        self.input.released = false;
        self.input.wheel = 0.0;
        if !self.input.down {
            self.widgets.active = None;
        }

        let mut presented = false;
        if cmds != self.last {
            if self.renderer.render(&cmds).is_ok() {
                self.last = cmds;
                presented = true;
            } else {
                self.last.clear();
            }
        }
        if save {
            self.persist();
        }
        presented
    }

    fn on_message(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<isize> {
        let lo = |v: isize| (v & 0xFFFF) as i16 as f32;
        let hi = |v: isize| ((v >> 16) & 0xFFFF) as i16 as f32;
        match msg {
            WM_MOUSEMOVE => {
                self.input.mouse = Some(crate::ui::Pt { x: lo(lp.0) / self.scale, y: hi(lp.0) / self.scale });
                if !self.tracking {
                    let mut t = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut t);
                    }
                    self.tracking = true;
                }
                self.dirty = true;
                Some(0)
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                if !self.input.down {
                    self.input.mouse = None;
                }
                self.dirty = true;
                Some(0)
            }
            WM_LBUTTONDOWN => {
                self.input.mouse = Some(crate::ui::Pt { x: lo(lp.0) / self.scale, y: hi(lp.0) / self.scale });
                self.input.down = true;
                self.input.pressed = true;
                self.input.press_pos = self.input.mouse.unwrap_or_default();
                unsafe { SetCapture(self.hwnd) };
                self.dirty = true;
                Some(0)
            }
            WM_LBUTTONUP => {
                self.input.mouse = Some(crate::ui::Pt { x: lo(lp.0) / self.scale, y: hi(lp.0) / self.scale });
                self.input.down = false;
                self.input.released = true;
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.dirty = true;
                Some(0)
            }
            WM_CAPTURECHANGED if self.input.down => {
                self.input.down = false;
                self.dirty = true;
                Some(0)
            }
            WM_MOUSEWHEEL => {
                self.input.wheel += hi(wp.0 as isize) / 120.0;
                self.dirty = true;
                Some(0)
            }
            WM_CHAR => {
                self.on_char(wp.0 as u32);
                Some(0)
            }
            WM_ACTIVATE if wp.0 & 0xFFFF != 0 => {
                // Back from the editor window: show the target it stored.
                let disk = Config::load();
                for (mine, saved) in self.cfg.slots.iter_mut().zip(disk.slots) {
                    mine.target = saved.target;
                }
                self.dirty = true;
                None
            }
            WM_DOCK_SCANNED => {
                if let Some(names) = DOCK_APPS.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    self.dock.apps = names;
                }
                self.dirty = true;
                Some(0)
            }
            WM_GEOCODED => {
                self.on_geocoded();
                Some(0)
            }
            WM_DPICHANGED => {
                let dpi = (wp.0 & 0xFFFF) as f32;
                self.scale = dpi / 96.0;
                let r = unsafe { *(lp.0 as *const RECT) };
                unsafe {
                    let _ = SetWindowPos(self.hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                }
                self.renderer.set_size((W * self.scale).round() as u32, (H * self.scale).round() as u32, self.scale);
                self.last.clear();
                self.dirty = true;
                Some(0)
            }
            WM_ERASEBKGND => Some(1),
            WM_PAINT => {
                unsafe {
                    let _ = ValidateRect(Some(self.hwnd), None);
                }
                self.last.clear();
                self.dirty = true;
                Some(0)
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                Some(0)
            }
            _ => None,
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match with(|s| s.on_message(msg, wp, lp)).flatten() {
        Some(v) => LRESULT(v),
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Windows refuses focus to a process started from the no-activate island, so
/// the window is first lifted above the others, then asked for focus.
unsafe fn bring_forward(hwnd: HWND) {
    let flags = SWP_NOMOVE | SWP_NOSIZE;
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
    let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
    let _ = SetForegroundWindow(hwnd);
}

/// Opens the settings window, or brings the existing one forward.
pub fn run(section: usize) -> Result<()> {
    unsafe {
        let class = w!("naelbar.settings");
        let _mutex = CreateMutexW(None, true, w!("Local\\naelbar.settings"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(existing) = FindWindowW(class, PCWSTR::null()) {
                let _ = ShowWindow(existing, SW_RESTORE);
                bring_forward(existing);
            }
            return Ok(());
        }

        let instance = GetModuleHandleW(None)?;
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            hIcon: crate::app_icon(),
            hIconSm: crate::app_icon(),
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        let cfg = Config::load();
        config::apply_appearance(&cfg);
        let style = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let ex = WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW;
        let title = HSTRING::from(tr("Paramètres de naelbar", "naelbar settings"));
        let hwnd = CreateWindowExW(ex, class, &title, style, CW_USEDEFAULT, CW_USEDEFAULT, 100, 100, None, None, Some(instance.into()), None)?;

        let dark: BOOL = true.into();
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const BOOL as *const _, size_of::<BOOL>() as u32);

        let dpi = GetDpiForWindow(hwnd).max(96);
        let scale = dpi as f32 / 96.0;
        let (cw, ch) = ((W * scale).round() as i32, (H * scale).round() as i32);
        let mut frame = RECT { left: 0, top: 0, right: cw, bottom: ch };
        let _ = AdjustWindowRectExForDpi(&mut frame, style, false, ex, dpi);
        let (fw, fh) = (frame.right - frame.left, frame.bottom - frame.top);
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let _ = SetWindowPos(hwnd, None, (sw - fw) / 2, (sh - fh) / 2, fw, fh, SWP_NOZORDER | SWP_NOACTIVATE);

        let mut renderer = Renderer::new(hwnd);
        renderer.set_size(cw as u32, ch as u32, scale);
        let city = cfg.weather_city.clone();
        let ics = cfg.ics_url.clone();
        let state = Settings {
            hwnd,
            renderer,
            cfg,
            input: Input::default(),
            widgets: WidgetState::default(),
            last: Vec::new(),
            scale,
            section: section.min(SECTIONS.len() - 1),
            search: String::new(),
            city,
            city_status: String::new(),
            ics,
            focus: Focus::Search,
            scroll: 0.0,
            dirty: true,
            tracking: false,
            autostart: autostart_enabled(),
            start_menu: start_menu_enabled(),
            monitors: monitors::list(),
            dock: DockPage { store: crate::dock::Store::load(), apps: Vec::new(), started: false },
            dock_query: String::new(),
        };
        STATE.with(|c| *c.borrow_mut() = Some(state));
        with(|s| s.frame());
        let _ = ShowWindow(hwnd, SW_SHOW);
        bring_forward(hwnd);

        // A frame is only drawn once the swapchain can take it. Drawing on every
        // pointer move would queue images ahead of the display, and the slider
        // would trail behind the pointer.
        let mut msg = MSG::default();
        let mut slot = false;
        'run: loop {
            let dirty = with(|s| s.dirty).unwrap_or(false);
            if dirty && !slot {
                slot = match with(|s| s.renderer.waitable()).flatten() {
                    Some(h) => {
                        // Only pending input delays the frame. A failed wait must not.
                        let r = MsgWaitForMultipleObjectsEx(Some(&[h]), 32, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                        r.0 != WAIT_OBJECT_0.0 + 1
                    }
                    None => true,
                };
            } else if !dirty {
                if GetMessageW(&mut msg, None, 0, 0).0 <= 0 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'run;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if slot && with(|s| s.dirty).unwrap_or(false) && with(|s| s.frame()).unwrap_or(false) {
                slot = false;
            }
        }
        STATE.with(|c| *c.borrow_mut() = None);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_ignores_case_and_accents() {
        assert_eq!(fold("Écran À Propos"), "ecran a propos");
        let monitor = ROWS.iter().find(|r| matches!(r.ctl, Ctl::Monitor)).unwrap();
        assert!(row_matches(monitor, &fold("ECRAN")));
        assert!(!row_matches(monitor, &fold("pomodoro")));
    }

    #[test]
    fn sliders_snap_inside_their_range() {
        assert_eq!(snap_value(0.83, 0.8, 1.5, 0.05), 0.85);
        assert_eq!(snap_value(99.0, 5.0, 90.0, 5.0), 90.0);
        assert_eq!(snap_value(-3.0, 0.0, 40.0, 1.0), 0.0);
    }

    #[test]
    fn notch_rows_follow_the_shape() {
        let notch = Config { shape: "notch".into(), ..Config::default() };
        let island = Config::default();
        let height = ROWS.iter().find(|r| r.title.0 == "Hauteur de l'encoche").unwrap();
        let top = ROWS.iter().find(|r| r.title.0 == "Distance du haut").unwrap();
        assert!(height.only.applies(&notch) && !height.only.applies(&island));
        assert!(top.only.applies(&island) && !top.only.applies(&notch));
    }

    #[test]
    fn the_launcher_page_exists() {
        assert_eq!(SECTIONS[launcher_section()].1, "Launcher");
        assert_eq!(short_tail("abcdef", 4), "…def");
        assert_eq!(short_tail("abc", 4), "abc");
    }

    #[test]
    fn activity_section_index_matches() {
        assert_eq!(SECTIONS[ACTIVITY_SECTION].1, "Activities");
    }

    #[test]
    fn phone_section_index_matches() {
        assert_eq!(SECTIONS[PHONE_SECTION].1, "Phone");
        assert_eq!(SECTIONS[TABS_SECTION].1, "Tabs");
        // Every island tab has its row, and no row names a missing tab.
        let names: Vec<&str> = crate::views::all().iter().map(|v| v.name()).collect();
        assert_eq!(ROWS.iter().filter(|r| r.section == TABS_SECTION).count(), names.len());
    }

    #[test]
    fn music_section_index_matches() {
        assert_eq!(SECTIONS[MUSIC_SECTION].1, "Music");
    }

    #[test]
    fn every_row_is_in_a_known_section() {
        assert!(ROWS.iter().all(|r| r.section < SECTIONS.len()));
    }
}
