//! Rocky OS desktop: a graphical shell on the framebuffer.
//!
//! Entered from the text shell with `desktop`, left with Esc or
//! the Exit button in the top bar (which returns to the shell).
//!
//! * Aurora wallpaper: banded vertical gradient, glowing orbs,
//!   deterministic stars (no RNG crate).
//! * Glass top bar: Rocky logo, focused-app indicator, search
//!   pill (opens the Start menu), live clock, memory count, Exit.
//! * Floating bottom dock with 6 apps and active dots.
//! * Start menu listing all apps + an About line.
//! * Desktop icons on the left and a live clock widget.
//! * Windows with drop shadows, rounded corners, gradient title
//!   bars, traffic-light close/minimise/maximise buttons.
//! * Six apps: Terminal (runs the shell commands, output captured
//!   into a scrollback), Files (RockyFS list + file viewer),
//!   Calculator (clickable buttons), Notes (editable, saves to
//!   RockyFS as notes.txt), System Monitor (uptime, tasks, live
//!   activity bar), Settings (specs + 3 wallpaper themes).
//! * PS/2 mouse: cursor, click, drag. Keyboard/serial input
//!   goes to the focused Terminal or Notes window.
//!
//! Rendering is a full redraw on every event (mouse move, key,
//! clock second change) — simple and correct at this scale. The
//! wallpaper is drawn with banded `fill_rect` calls (a per-pixel
//! fill loop costs ~1.5 s per frame in a debug build; the row
//! copies keep a full redraw well under 500 ms).

use crate::framebuffer::{self, Color};
use crate::interrupts::TIMER_TICKS;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

const TOPBAR_H: i32 = 32;
const TITLE_H: i32 = 28;
const TITLE_R: i32 = 10;
const CHAR_W: i32 = 8;
const CHAR_H: i32 = 16;
const DOCK_W: i32 = 408;
const DOCK_H: i32 = 64;
const DOCK_Y: i32 = 644;
const MENU_X: i32 = 16;
const MENU_Y: i32 = 300;
const MENU_W: i32 = 300;
const MENU_H: i32 = 336;
const MENU_ROWS_Y: i32 = 340;
const MENU_ROW_H: i32 = 44;
const WIDGET_X: i32 = 120;
const WIDGET_Y: i32 = 512;
const WIDGET_W: i32 = 280;
const WIDGET_H: i32 = 120;

const WHITE: Color = (255, 255, 255);
const BLACK: Color = (0, 0, 0);
const TOPBAR: Color = (14, 16, 28);
const TOPBAR_TEXT: Color = (255, 255, 255);
const TOPBAR_MUTED: Color = (150, 170, 205);
const PILL: Color = (36, 40, 58);
const PILL_TEXT: Color = (140, 150, 170);
const WIN_BG: Color = (30, 32, 48);
const TERM_BG: Color = (14, 16, 26);
const TERM_TEXT: Color = (210, 220, 230);
const TERM_GREEN: Color = (90, 220, 140);
const TEXT: Color = (232, 236, 244);
const MUTED: Color = (150, 158, 175);
const HIGHLIGHT: Color = (64, 90, 160);
const EXIT_BTN: Color = (200, 60, 60);
const DOCK_BG: Color = (18, 20, 36);
const MENU_BG: Color = (24, 26, 46);
const MENU_ACCENT: Color = (124, 80, 220);
const WIDGET_BG: Color = (18, 20, 36);

/// Blend two colours: `t` (0..=255) is the weight of `b`.
fn blend(a: Color, b: Color, t: u32) -> Color {
    let t = t.min(255) as u16;
    (
        (a.0 as u16 * (255 - t) / 255 + b.0 as u16 * t / 255) as u8,
        (a.1 as u16 * (255 - t) / 255 + b.1 as u16 * t / 255) as u8,
        (a.2 as u16 * (255 - t) / 255 + b.2 as u16 * t / 255) as u8,
    )
}

struct Theme {
    name: &'static str,
    top: Color,
    mid: Color,
    bot: Color,
    orbs: [Color; 3],
}

const THEMES: [Theme; 3] = [
    Theme {
        name: "Aurora",
        top: (20, 26, 58),
        mid: (74, 52, 160),
        bot: (16, 110, 120),
        orbs: [(120, 90, 255), (40, 200, 190), (255, 120, 180)],
    },
    Theme {
        name: "Sunset",
        top: (46, 14, 52),
        mid: (196, 64, 96),
        bot: (248, 150, 64),
        orbs: [(255, 120, 80), (255, 200, 90), (200, 60, 140)],
    },
    Theme {
        name: "Midnight",
        top: (4, 6, 18),
        mid: (12, 20, 48),
        bot: (6, 10, 26),
        orbs: [(60, 80, 180), (30, 60, 120), (90, 60, 160)],
    },
];

/// Glowing orbs: (centre x, centre y, radius). Same positions
/// for every theme; only the colours change.
const ORBS: [(i32, i32, i32); 3] = [(980, 170, 170), (220, 540, 150), (640, 630, 110)];

/// Wallpaper gradient colour at `t` (0..=255, top to bottom).
fn gradient_at(theme: &Theme, t: u32) -> Color {
    if t < 128 {
        blend(theme.top, theme.mid, t * 2)
    } else {
        blend(theme.mid, theme.bot, (t - 128) * 2)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum App {
    Terminal,
    Files,
    Calculator,
    Notes,
    Monitor,
    Settings,
}

const APPS: [App; 6] = [
    App::Terminal,
    App::Files,
    App::Calculator,
    App::Notes,
    App::Monitor,
    App::Settings,
];

impl App {
    fn title(self) -> &'static str {
        match self {
            App::Terminal => "Terminal",
            App::Files => "Files",
            App::Calculator => "Calculator",
            App::Notes => "Notes",
            App::Monitor => "System Monitor",
            App::Settings => "Settings",
        }
    }
    /// Short label for desktop icons.
    fn short_label(self) -> &'static str {
        match self {
            App::Terminal => "Terminal",
            App::Files => "Files",
            App::Calculator => "Calculator",
            App::Notes => "Notes",
            App::Monitor => "Monitor",
            App::Settings => "Settings",
        }
    }
    fn glyph(self) -> &'static str {
        match self {
            App::Terminal => ">_",
            App::Files => "F",
            App::Calculator => "=",
            App::Notes => "N",
            App::Monitor => "M",
            App::Settings => "S",
        }
    }
    fn color(self) -> Color {
        match self {
            App::Terminal => (36, 40, 54),
            App::Files => (43, 108, 184),
            App::Calculator => (230, 120, 40),
            App::Notes => (220, 180, 40),
            App::Monitor => (20, 160, 140),
            App::Settings => (120, 110, 150),
        }
    }
    fn subtitle(self) -> &'static str {
        match self {
            App::Terminal => "shell & commands",
            App::Files => "RockyFS browser",
            App::Calculator => "quick maths",
            App::Notes => "write & save",
            App::Monitor => "tasks & uptime",
            App::Settings => "themes & about",
        }
    }
}

fn app_bg(app: App) -> Color {
    match app {
        App::Terminal => TERM_BG,
        _ => WIN_BG,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TitleBtn {
    Close,
    Min,
    Max,
}

#[derive(Clone, Copy)]
struct Window {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    title: &'static str,
    app: App,
    open: bool,
    min: bool,
    max: bool,
    saved: Option<(i32, i32, i32, i32)>,
}

impl Window {
    fn contains(&self, x: i32, y: i32) -> bool {
        self.open && x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    fn title_bar(&self, x: i32, y: i32) -> bool {
        self.contains(x, y) && y < self.y + TITLE_H
    }
    fn content_y(&self) -> i32 {
        self.y + TITLE_H
    }
}

/// Hit-test the traffic-light buttons (red close, amber
/// minimise, green maximise) in a title bar.
fn title_button_hit(win: &Window, x: i32, y: i32) -> Option<TitleBtn> {
    for (i, btn) in [TitleBtn::Close, TitleBtn::Min, TitleBtn::Max]
        .iter()
        .enumerate()
    {
        let cx = win.x + 18 + (i as i32) * 20;
        let cy = win.y + 14;
        let dx = x - cx;
        let dy = y - cy;
        if dx * dx + dy * dy <= 64 {
            return Some(*btn);
        }
    }
    None
}

fn dock_x() -> i32 {
    (framebuffer::width() as i32 - DOCK_W) / 2
}

struct Desktop {
    windows: Vec<Window>,
    focused: Option<App>,
    dragging: Option<(App, i32, i32)>,
    last_left: bool,
    last_second: u64,
    memory_regions: usize,
    theme: usize,
    start_open: bool,
    term_lines: Vec<String>,
    term_partial: String,
    term_input: String,
    files_selected: Option<String>,
    /// Cached RockyFS directory: `fs::list()` costs 16 ATA PIO
    /// sector reads, far too much to pay on every frame. The cache
    /// refreshes at most once a second (see `refresh_files`) and
    /// when the Files app is opened.
    files_cache: Vec<crate::fs::FileEntry>,
    files_cache_tick: u64,
    /// Cached preview of the selected file (loaded on selection,
    /// not on every frame).
    files_preview: Option<(String, Vec<u8>)>,
    files_preview_err: bool,
    calc_expr: String,
    calc_result: String,
    notes_text: String,
    notes_loaded: bool,
    notes_saved: Option<u64>,
    notes_save_err: bool,
    exit: bool,
}

fn clock_string() -> String {
    let secs = TIMER_TICKS.load(Ordering::SeqCst) / 18;
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

fn push_line(lines: &mut Vec<String>, line: String) {
    lines.push(line);
    if lines.len() > 200 {
        lines.drain(..lines.len() - 200);
    }
}

impl Desktop {
    fn new(memory_regions: usize) -> Self {
        let mut d = Desktop {
            windows: alloc::vec![
                Window { x: 730, y: 48, w: 510, h: 360, title: "Files", app: App::Files, open: true, min: false, max: false, saved: None },
                Window { x: 920, y: 380, w: 300, h: 260, title: "Calculator", app: App::Calculator, open: true, min: false, max: false, saved: None },
                Window { x: 96, y: 48, w: 620, h: 400, title: "Terminal", app: App::Terminal, open: true, min: false, max: false, saved: None },
            ],
            focused: Some(App::Terminal),
            dragging: None,
            last_left: false,
            last_second: u64::MAX,
            memory_regions,
            theme: 0,
            start_open: false,
            term_lines: Vec::new(),
            term_partial: String::new(),
            term_input: String::new(),
            files_selected: None,
            files_cache: crate::fs::list(),
            files_cache_tick: TIMER_TICKS.load(Ordering::SeqCst),
            files_preview: None,
            files_preview_err: false,
            calc_expr: String::new(),
            calc_result: String::new(),
            notes_text: String::new(),
            notes_loaded: false,
            notes_saved: None,
            notes_save_err: false,
            exit: false,
        };
        push_line(&mut d.term_lines, "Rocky OS Desktop - Aurora".into());
        push_line(&mut d.term_lines, "Terminal - type 'help' for commands.".into());
        push_line(&mut d.term_lines, "Dock or Start menu opens apps; Esc or Exit".into());
        push_line(&mut d.term_lines, "returns to the text shell.".into());
        d
    }

    fn window(&self, app: App) -> Option<&Window> {
        self.windows.iter().find(|w| w.app == app)
    }

    fn window_mut(&mut self, app: App) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.app == app)
    }

    fn bring_to_front(&mut self, app: App) {
        if let Some(pos) = self.windows.iter().position(|w| w.app == app) {
            let w = self.windows.remove(pos);
            self.windows.push(w);
        }
        self.focused = Some(app);
    }

    /// Refresh the cached file list if it is over a second old.
    /// Called once a second from the main loop; the Files app also
    /// refreshes on open, so a `write`/`rm` in the terminal shows
    /// up within a second.
    fn refresh_files(&mut self) {
        let now = TIMER_TICKS.load(Ordering::SeqCst);
        if now.wrapping_sub(self.files_cache_tick) >= 18 {
            self.files_cache = crate::fs::list();
            self.files_cache_tick = now;
        }
    }

    fn open_app(&mut self, app: App) {
        let (x, y, w, h, title) = match app {
            App::Terminal => (96, 48, 620, 400, "Terminal"),
            App::Files => (730, 48, 510, 360, "Files"),
            App::Calculator => (920, 380, 300, 260, "Calculator"),
            App::Notes => (140, 80, 560, 440, "Notes"),
            App::Monitor => (700, 90, 540, 420, "System Monitor"),
            App::Settings => (360, 140, 560, 380, "Settings"),
        };
        if app == App::Files {
            self.files_cache = crate::fs::list();
            self.files_cache_tick = TIMER_TICKS.load(Ordering::SeqCst);
        }
        if app == App::Notes && !self.notes_loaded {
            self.notes_loaded = true;
            if let Ok(data) = crate::fs::read_file("notes.txt") {
                self.notes_text = String::from_utf8_lossy(&data).into_owned();
            }
        }
        match self.window_mut(app) {
            Some(win) => {
                win.open = true;
                win.min = false;
            }
            None => self.windows.push(Window {
                x,
                y,
                w,
                h,
                title,
                app,
                open: true,
                min: false,
                max: false,
                saved: None,
            }),
        }
        self.bring_to_front(app);
    }

    /// Dock click: minimise if already focused, restore if
    /// minimised, focus if open, launch if closed.
    fn dock_click(&mut self, app: App) {
        match self.window(app) {
            Some(win) if win.open && !win.min && self.focused == Some(app) => {
                if let Some(w) = self.window_mut(app) {
                    w.min = true;
                }
                self.focused = None;
            }
            Some(win) if win.open => {
                if let Some(w) = self.window_mut(app) {
                    w.min = false;
                }
                self.bring_to_front(app);
            }
            _ => self.open_app(app),
        }
    }

    fn toggle_max(&mut self, app: App) {
        let w = framebuffer::width() as i32;
        if let Some(win) = self.window_mut(app) {
            if win.max {
                if let Some((x, y, ww, hh)) = win.saved {
                    win.x = x;
                    win.y = y;
                    win.w = ww;
                    win.h = hh;
                }
                win.max = false;
            } else {
                win.saved = Some((win.x, win.y, win.w, win.h));
                win.x = 8;
                win.y = TOPBAR_H + 8;
                win.w = w - 16;
                win.h = DOCK_Y - TOPBAR_H - 16;
                win.max = true;
            }
        }
    }

    fn dock_hit(&self, x: i32, y: i32) -> Option<App> {
        if y < DOCK_Y || y >= DOCK_Y + DOCK_H {
            return None;
        }
        let dx = dock_x();
        if x < dx + 12 || x >= dx + 12 + 6 * 64 {
            return None;
        }
        Some(APPS[((x - dx - 12) / 64) as usize])
    }

    fn desktop_icon_hit(&self, x: i32, y: i32) -> Option<App> {
        if x < 24 || x >= 72 {
            return None;
        }
        if y < 64 {
            return None;
        }
        let i = (y - 64) / 88;
        if i < 0 || i >= 6 {
            return None;
        }
        Some(APPS[i as usize])
    }

    fn start_menu_hit(&self, x: i32, y: i32) -> Option<App> {
        if x < MENU_X || x >= MENU_X + MENU_W {
            return None;
        }
        if y < MENU_ROWS_Y || y >= MENU_ROWS_Y + 6 * MENU_ROW_H {
            return None;
        }
        Some(APPS[((y - MENU_ROWS_Y) / MENU_ROW_H) as usize])
    }

    // ----- input -----

    fn feed_char(&mut self, c: char) {
        match self.focused {
            Some(App::Notes) => match c {
                '\u{1b}' => self.exit = true,
                '\n' | '\r' => {
                    if self.notes_text.len() < 4000 {
                        self.notes_text.push('\n');
                    }
                }
                '\u{8}' | '\u{7f}' => {
                    self.notes_text.pop();
                }
                c if !c.is_control() => {
                    if self.notes_text.len() < 4000 {
                        self.notes_text.push(c);
                    }
                }
                _ => {}
            },
            Some(App::Terminal) => self.feed_terminal(c),
            _ => {}
        }
    }

    fn feed_terminal(&mut self, c: char) {
        match c {
            '\u{1b}' => self.exit = true,
            '\n' | '\r' => {
                let input = core::mem::take(&mut self.term_input);
                push_line(&mut self.term_lines, format!("rocky> {}", input));
                let cmd = input.trim().to_string();
                if cmd == "clear" {
                    self.term_lines.clear();
                } else if cmd == "exit" {
                    push_line(&mut self.term_lines, "use Esc or the Exit button to leave the desktop".into());
                } else {
                    let mem = self.memory_regions;
                    // Sink: split command output into scrollback lines.
                    let mut lines: Vec<String> = Vec::new();
                    let mut partial = core::mem::take(&mut self.term_partial);
                    {
                        let mut sink = |s: &str| {
                            for ch in s.chars() {
                                if ch == '\n' {
                                    lines.push(core::mem::take(&mut partial));
                                } else if ch != '\r' {
                                    partial.push(ch);
                                }
                            }
                        };
                        crate::shell::run_command(&cmd, mem, &mut sink, true);
                    }
                    if !partial.is_empty() {
                        lines.push(core::mem::take(&mut partial));
                    }
                    self.term_partial = partial;
                    for line in lines {
                        push_line(&mut self.term_lines, line);
                    }
                }
            }
            '\u{8}' | '\u{7f}' => {
                self.term_input.pop();
            }
            c if !c.is_control() => {
                if self.term_input.len() < 100 {
                    self.term_input.push(c);
                }
            }
            _ => {}
        }
    }

    fn handle_press(&mut self, x: i32, y: i32) {
        let w = framebuffer::width() as i32;
        // Top bar: logo toggles Start, pill opens Start, Exit exits.
        if y < TOPBAR_H {
            if (10..30).contains(&x) {
                self.start_open = !self.start_open;
                return;
            }
            if (460..820).contains(&x) {
                self.start_open = true;
                return;
            }
            if x >= w - 74 && x < w - 10 {
                self.exit = true;
                return;
            }
            return;
        }
        // Start menu open: a row launches its app; a click inside
        // the panel keeps it open; anywhere else closes it.
        if self.start_open {
            if let Some(app) = self.start_menu_hit(x, y) {
                self.start_open = false;
                self.open_app(app);
                return;
            }
            if x >= MENU_X && x < MENU_X + MENU_W && y >= MENU_Y && y < MENU_Y + MENU_H {
                return;
            }
            self.start_open = false;
            return;
        }
        // Dock.
        if let Some(app) = self.dock_hit(x, y) {
            self.dock_click(app);
            return;
        }
        // Topmost window containing the point.
        for idx in (0..self.windows.len()).rev() {
            let win = self.windows[idx];
            if !win.open || win.min || !win.contains(x, y) {
                continue;
            }
            let app = win.app;
            self.bring_to_front(app);
            if let Some(btn) = title_button_hit(&win, x, y) {
                match btn {
                    TitleBtn::Close => {
                        if let Some(w) = self.window_mut(app) {
                            w.open = false;
                        }
                        if self.focused == Some(app) {
                            self.focused = None;
                        }
                    }
                    TitleBtn::Min => {
                        if let Some(w) = self.window_mut(app) {
                            w.min = true;
                        }
                        if self.focused == Some(app) {
                            self.focused = None;
                        }
                    }
                    TitleBtn::Max => self.toggle_max(app),
                }
                return;
            }
            if win.title_bar(x, y) {
                if !win.max {
                    self.dragging = Some((app, x - win.x, y - win.y));
                }
                return;
            }
            match app {
                App::Files => self.files_click(x, y),
                App::Calculator => self.calc_click(x, y),
                App::Notes => self.notes_click(x, y),
                App::Settings => self.settings_click(x, y),
                _ => {}
            }
            return;
        }
        // Desktop icons (behind the windows).
        if let Some(app) = self.desktop_icon_hit(x, y) {
            self.open_app(app);
            return;
        }
        self.focused = None;
    }

    fn files_click(&mut self, x: i32, y: i32) {
        let Some(win) = self.window(App::Files).copied() else { return };
        let row = (y - win.content_y() - 28) / 18;
        if row >= 0 && (row as usize) < self.files_cache.len() {
            let name = self.files_cache[row as usize].name.clone();
            if self.files_selected.as_ref() != Some(&name) {
                self.files_selected = Some(name.clone());
                // Load the preview once, on selection.
                match crate::fs::read_file(&name) {
                    Ok(data) => {
                        self.files_preview = Some((name, data));
                        self.files_preview_err = false;
                    }
                    Err(_) => {
                        self.files_preview = None;
                        self.files_preview_err = true;
                    }
                }
            }
        }
        let _ = x;
    }

    fn calc_click(&mut self, x: i32, y: i32) {
        let Some(win) = self.window(App::Calculator).copied() else { return };
        let labels = calc_labels();
        for (i, label) in labels.iter().enumerate() {
            if label.is_empty() {
                continue;
            }
            let (bx, by, bw, bh) = calc_button_rect(win, i);
            if x >= bx && x < bx + bw && y >= by && y < by + bh {
                self.calc_press(label);
                return;
            }
        }
    }

    fn calc_press(&mut self, label: &str) {
        match label {
            "C" => {
                self.calc_expr.clear();
                self.calc_result.clear();
            }
            "=" => {
                self.calc_result = eval_expr(&self.calc_expr);
            }
            _ => {
                if self.calc_expr.len() < 32 {
                    self.calc_expr.push_str(label);
                }
            }
        }
    }

    fn notes_click(&mut self, x: i32, y: i32) {
        let Some(win) = self.window(App::Notes).copied() else { return };
        let (bx, by) = (win.x + win.w - 96, win.y + win.h - 38);
        if x >= bx && x < bx + 84 && y >= by && y < by + 28 {
            match crate::fs::write_file("notes.txt", self.notes_text.as_bytes()) {
                Ok(()) => {
                    self.notes_saved = Some(TIMER_TICKS.load(Ordering::SeqCst));
                    self.notes_save_err = false;
                }
                Err(_) => {
                    self.notes_save_err = true;
                }
            }
        }
    }

    fn settings_click(&mut self, x: i32, y: i32) {
        let Some(win) = self.window(App::Settings).copied() else { return };
        let sw_y = win.content_y() + 150;
        for i in 0..3 {
            let sx = win.x + 20 + (i as i32) * 170;
            if x >= sx && x < sx + 150 && y >= sw_y && y < sw_y + 64 {
                self.theme = i as usize;
                return;
            }
        }
    }

    fn handle_mouse(&mut self) {
        let state = crate::mouse::state();
        let mut changed = state.changed;
        crate::mouse::clear_changed();
        if state.left && !self.last_left {
            self.handle_press(state.x, state.y);
            changed = true;
        }
        if !state.left && self.last_left {
            self.dragging = None;
            changed = true;
        }
        if state.left {
            if let Some((app, off_x, off_y)) = self.dragging {
                let w = framebuffer::width() as i32;
                let h = framebuffer::height() as i32;
                if let Some(win) = self.window_mut(app) {
                    win.x = (state.x - off_x).clamp(0, w - 80);
                    win.y = (state.y - off_y).clamp(TOPBAR_H, h - 40);
                }
                changed = true;
            }
        }
        self.last_left = state.left;
        if changed {
            self.render();
        }
    }

    // ----- rendering -----

    fn render(&self) {
        let w = framebuffer::width();
        let h = framebuffer::height();
        if w == 0 || h == 0 {
            return;
        }
        self.render_wallpaper(w, h);
        self.render_desktop_icons();
        self.render_widget();
        for win in &self.windows {
            if win.open && !win.min {
                self.render_window(win);
            }
        }
        if self.start_open {
            self.render_start_menu();
        }
        self.render_dock();
        self.render_topbar(w);
        let mouse = crate::mouse::state();
        draw_cursor(mouse.x, mouse.y);
    }

    fn render_wallpaper(&self, w: usize, h: usize) {
        let theme = &THEMES[self.theme];
        // Vertical gradient in 56 horizontal bands.
        const BANDS: usize = 56;
        for i in 0..BANDS {
            let y0 = i * h / BANDS;
            let y1 = (i + 1) * h / BANDS;
            let t = (i * 255 / (BANDS - 1)) as u32;
            framebuffer::fill_rect(0, y0, w, y1 - y0, gradient_at(theme, t));
        }
        // Glowing orbs: 4px horizontal bands, each band's colour
        // blending from the orb colour (centre) to the gradient
        // colour underneath (edge), with a brighter core.
        for (idx, &(cx, cy, r)) in ORBS.iter().enumerate() {
            let orb = theme.orbs[idx];
            let r2 = (r * r) as u32;
            let mut y = cy - r;
            while y <= cy + r {
                let yc = y + 2;
                let dy = (yc - cy).abs();
                if dy <= r {
                    let dx = framebuffer::isqrt(r2 - (dy * dy) as u32) as i32;
                    let k = ((dy as u32 * dy as u32) * 255 / r2).min(255);
                    let bg = gradient_at(theme, ((yc.max(0) as usize * 255) / h.max(1)).min(255) as u32);
                    let mut c = blend(orb, bg, k * 230 / 255);
                    // Bright core, ramped smoothly to zero at
                    // dy = r/3 (a constant blend would leave a
                    // hard horizontal edge across the orb).
                    if dy * 3 < r {
                        let wamt = 70 * (r as u32 - 3 * dy as u32) / r as u32;
                        c = blend(c, (255, 255, 255), wamt);
                    }
                    let x0 = (cx - dx).max(0);
                    let x1 = (cx + dx).min(w as i32 - 1);
                    if x1 >= x0 {
                        framebuffer::fill_rect(
                            x0 as usize,
                            y.max(0) as usize,
                            (x1 - x0 + 1) as usize,
                            4,
                            c,
                        );
                    }
                }
                y += 4;
            }
        }
        // Deterministic stars: a hash of the cell position decides
        // whether a star is drawn (no RNG crate needed).
        for gy in 0..(h as i32 / 20) {
            for gx in 0..(w as i32 / 26) {
                let x = gx * 26 + 13;
                let y = gy * 20 + 10;
                let mut v = (x as u32)
                    .wrapping_mul(374761393)
                    ^ (y as u32).wrapping_mul(668265263);
                v ^= v >> 13;
                v = v.wrapping_mul(1274126177);
                v ^= v >> 16;
                if v % 9 == 0 {
                    framebuffer::set_pixel(x as usize, y as usize, (205, 215, 255));
                } else if v % 29 == 0 {
                    framebuffer::fill_rect(x as usize, y as usize, 2, 2, WHITE);
                }
            }
        }
    }

    fn render_topbar(&self, w: usize) {
        let w = w as i32;
        framebuffer::fill_rect(0, 0, w as usize, TOPBAR_H as usize, TOPBAR);
        framebuffer::fill_rect(0, TOPBAR_H as usize, w as usize, 1, (40, 44, 64));
        // Rocky logo mark + name.
        framebuffer::fill_rounded_rect(10, 6, 20, 20, 6, (90, 60, 220));
        framebuffer::draw_text(16, 9, "R", TOPBAR_TEXT, None);
        framebuffer::draw_text(38, 8, "Rocky OS", TOPBAR_TEXT, None);
        // Open-app indicator.
        if let Some(app) = self.focused {
            framebuffer::draw_text(130, 8, &format!("[{}]", app.title()), TOPBAR_MUTED, None);
        }
        // Centre search / launcher pill (click opens Start).
        framebuffer::fill_rounded_rect(460, 5, 360, 22, 11, PILL);
        framebuffer::draw_text(476, 9, "Search apps...", PILL_TEXT, None);
        // Right: memory count, live clock, Exit button.
        framebuffer::draw_text(
            1006,
            8,
            &format!("mem: {} regions", self.memory_regions),
            TOPBAR_MUTED,
            None,
        );
        framebuffer::draw_text(1134, 8, &clock_string(), TOPBAR_TEXT, None);
        framebuffer::fill_rounded_rect((w - 74) as usize, 5, 64, 22, 6, EXIT_BTN);
        framebuffer::draw_text((w - 58) as usize, 9, "Exit", TOPBAR_TEXT, None);
    }

    fn render_dock(&self) {
        let dx = dock_x();
        framebuffer::fill_rounded_rect(
            dx as usize,
            DOCK_Y as usize,
            DOCK_W as usize,
            DOCK_H as usize,
            28,
            DOCK_BG,
        );
        for (i, app) in APPS.iter().enumerate() {
            let ix = dx + 12 + (i as i32) * 64;
            let iy = DOCK_Y + 6;
            framebuffer::fill_rounded_rect(ix as usize, iy as usize, 44, 44, 10, app.color());
            let g = app.glyph();
            framebuffer::draw_text(
                (ix + 22 - (g.len() as i32) * 4) as usize,
                (iy + 14) as usize,
                g,
                WHITE,
                None,
            );
            if let Some(win) = self.window(*app) {
                if win.open {
                    let dot = if win.min { (120, 125, 140) } else { (225, 230, 245) };
                    framebuffer::fill_circle(ix + 22, DOCK_Y + 57, 3, dot);
                }
            }
        }
    }

    fn render_start_menu(&self) {
        framebuffer::fill_rounded_rect(
            MENU_X as usize,
            MENU_Y as usize,
            MENU_W as usize,
            MENU_H as usize,
            12,
            MENU_BG,
        );
        framebuffer::fill_rounded_rect(MENU_X as usize, MENU_Y as usize, MENU_W as usize, 4, 2, MENU_ACCENT);
        framebuffer::draw_text((MENU_X + 16) as usize, (MENU_Y + 14) as usize, "Rocky OS", WHITE, None);
        for (i, app) in APPS.iter().enumerate() {
            let y0 = MENU_ROWS_Y + (i as i32) * MENU_ROW_H;
            framebuffer::fill_rounded_rect(
                (MENU_X + 16) as usize,
                (y0 + 6) as usize,
                32,
                32,
                8,
                app.color(),
            );
            let g = app.glyph();
            framebuffer::draw_text(
                (MENU_X + 16 + 16 - (g.len() as i32) * 4) as usize,
                (y0 + 14) as usize,
                g,
                WHITE,
                None,
            );
            framebuffer::draw_text((MENU_X + 60) as usize, (y0 + 8) as usize, app.title(), WHITE, None);
            framebuffer::draw_text(
                (MENU_X + 60) as usize,
                (y0 + 24) as usize,
                app.subtitle(),
                MUTED,
                None,
            );
        }
        framebuffer::fill_rect(
            (MENU_X + 12) as usize,
            (MENU_Y + MENU_H - 28) as usize,
            (MENU_W - 24) as usize,
            1,
            (50, 54, 72),
        );
        framebuffer::draw_text(
            (MENU_X + 16) as usize,
            (MENU_Y + MENU_H - 22) as usize,
            "Rocky OS 0.1 - 64-bit Rust kernel",
            MUTED,
            None,
        );
    }

    fn render_desktop_icons(&self) {
        for (i, app) in APPS.iter().enumerate() {
            let y0 = 64 + (i as i32) * 88;
            framebuffer::fill_rounded_rect(24, y0 as usize, 48, 48, 12, app.color());
            let g = app.glyph();
            framebuffer::draw_text(
                (48 - (g.len() as i32) * 4) as usize,
                (y0 + 16) as usize,
                g,
                WHITE,
                None,
            );
            let name = app.short_label();
            framebuffer::draw_text(
                (48 - (name.len() as i32) * 4).max(0) as usize,
                (y0 + 54) as usize,
                name,
                WHITE,
                None,
            );
        }
    }

    fn render_widget(&self) {
        let theme = &THEMES[self.theme];
        framebuffer::fill_rounded_rect(
            WIDGET_X as usize,
            WIDGET_Y as usize,
            WIDGET_W as usize,
            WIDGET_H as usize,
            14,
            WIDGET_BG,
        );
        framebuffer::draw_text_scaled(
            (WIDGET_X + 16) as usize,
            (WIDGET_Y + 12) as usize,
            &clock_string(),
            (240, 244, 255),
            None,
            2,
        );
        framebuffer::draw_text(
            (WIDGET_X + 16) as usize,
            (WIDGET_Y + 52) as usize,
            &format!("Rocky OS - {}", theme.name),
            (180, 190, 220),
            None,
        );
        framebuffer::draw_text(
            (WIDGET_X + 16) as usize,
            (WIDGET_Y + 72) as usize,
            &format!("uptime {}", clock_string()),
            MUTED,
            None,
        );
        framebuffer::draw_text(
            (WIDGET_X + 16) as usize,
            (WIDGET_Y + 92) as usize,
            &format!("{} regions - heap 100 KiB", self.memory_regions),
            MUTED,
            None,
        );
    }

    fn render_window(&self, win: &Window) {
        let active = self.focused == Some(win.app);
        let x = win.x;
        let y = win.y;
        let w = win.w;
        let h = win.h;
        // Drop shadow, offset down-right.
        framebuffer::fill_rounded_rect(
            (x + 6) as usize,
            (y + 7) as usize,
            w as usize,
            h as usize,
            10,
            (8, 10, 20),
        );
        // Body with rounded corners.
        framebuffer::fill_rounded_rect(
            x as usize,
            y as usize,
            w as usize,
            h as usize,
            10,
            app_bg(win.app),
        );
        self.render_title_bar(win, active);
        match win.app {
            App::Terminal => self.render_terminal(win),
            App::Files => self.render_files(win),
            App::Calculator => self.render_calculator(win),
            App::Notes => self.render_notes(win),
            App::Monitor => self.render_monitor(win),
            App::Settings => self.render_settings(win),
        }
    }

    fn render_title_bar(&self, win: &Window, active: bool) {
        let (c1, c2) = if active {
            ((70, 120, 240), (120, 80, 220))
        } else {
            ((100, 108, 128), (76, 82, 102))
        };
        let x = win.x;
        let y = win.y;
        let w = win.w;
        for i in 0..TITLE_H {
            let color = blend(c1, c2, (i * 255 / (TITLE_H - 1)) as u32);
            if i < TITLE_R {
                let dy = (TITLE_R - i) as u32;
                let dx = framebuffer::isqrt(TITLE_R as u32 * TITLE_R as u32 - dy * dy) as i32;
                let inset = TITLE_R - dx;
                framebuffer::fill_rect(
                    (x + inset) as usize,
                    (y + i) as usize,
                    (w - 2 * inset).max(0) as usize,
                    1,
                    color,
                );
            } else {
                framebuffer::fill_rect(x as usize, (y + i) as usize, w as usize, 1, color);
            }
        }
        // Traffic lights: red close, amber minimise, green maximise.
        let cy = y + 14;
        framebuffer::fill_circle(x + 18, cy, 6, (255, 95, 86));
        framebuffer::fill_circle(x + 38, cy, 6, (255, 189, 46));
        framebuffer::fill_circle(x + 58, cy, 6, (39, 201, 63));
        framebuffer::draw_text((x + 80) as usize, (y + 6) as usize, win.title, WHITE, None);
    }

    fn render_terminal(&self, win: &Window) {
        let cx = win.x + 8;
        let mut cy = win.content_y() + 6;
        let max_lines = ((win.h - TITLE_H - 28) / CHAR_H).max(0) as usize;
        let max_chars = ((win.w - 16) / CHAR_W).max(0) as usize;
        let start = self.term_lines.len().saturating_sub(max_lines);
        for line in self.term_lines.iter().skip(start) {
            let shown: String = line.chars().take(max_chars).collect();
            framebuffer::draw_text(cx as usize, cy as usize, &shown, TERM_TEXT, None);
            cy += CHAR_H;
        }
        framebuffer::draw_text(cx as usize, cy as usize, "rocky> ", TERM_GREEN, None);
        framebuffer::draw_text(
            (cx + 7 * CHAR_W) as usize,
            cy as usize,
            &self.term_input,
            (240, 244, 255),
            None,
        );
        let cursor_x = cx + (7 + self.term_input.chars().count() as i32) * CHAR_W;
        framebuffer::fill_rect(cursor_x as usize, cy as usize, 8, 16, TERM_GREEN);
    }

    fn render_files(&self, win: &Window) {
        let files = &self.files_cache;
        let mut y = win.content_y() + 6;
        framebuffer::draw_text(
            (win.x + 8) as usize,
            y as usize,
            &format!("RockyFS - {} files", files.len()),
            MUTED,
            None,
        );
        y += 22;
        if files.is_empty() {
            framebuffer::draw_text((win.x + 8) as usize, y as usize, "(no files)", MUTED, None);
            return;
        }
        for f in files {
            if Some(&f.name) == self.files_selected.as_ref() {
                framebuffer::fill_rect(
                    (win.x + 2) as usize,
                    y as usize,
                    (win.w - 4) as usize,
                    18,
                    HIGHLIGHT,
                );
            }
            framebuffer::draw_text(
                (win.x + 8) as usize,
                y as usize,
                &format!("{}  {} bytes", f.name, f.size),
                TEXT,
                None,
            );
            y += 18;
        }
        if let Some(name) = &self.files_selected {
            let pane_y = y + 10;
            framebuffer::draw_text(
                (win.x + 8) as usize,
                pane_y as usize,
                &format!("-- {} --", name),
                MUTED,
                None,
            );
            if self.files_preview_err {
                framebuffer::draw_text(
                    (win.x + 8) as usize,
                    (pane_y + 18) as usize,
                    "read error",
                    TEXT,
                    None,
                );
            } else if let Some((_, data)) = &self.files_preview {
                let text = String::from_utf8_lossy(data);
                let mut ly = pane_y + 18;
                for line in text.lines().take(8) {
                    framebuffer::draw_text((win.x + 8) as usize, ly as usize, line, TERM_TEXT, None);
                    ly += 16;
                }
            }
        }
    }

    fn render_calculator(&self, win: &Window) {
        let cx = win.x + 10;
        let cy = win.content_y() + 10;
        framebuffer::fill_rounded_rect(
            cx as usize,
            cy as usize,
            (win.w - 20) as usize,
            26,
            6,
            TERM_BG,
        );
        framebuffer::draw_rect(cx as usize, cy as usize, (win.w - 20) as usize, 26, (70, 76, 100));
        let shown = if self.calc_result.is_empty() {
            &self.calc_expr
        } else {
            &self.calc_result
        };
        framebuffer::draw_text((cx + 8) as usize, (cy + 5) as usize, shown, (240, 244, 255), None);
        for (i, label) in calc_labels().iter().enumerate() {
            if label.is_empty() {
                continue;
            }
            let (bx, by, bw, bh) = calc_button_rect(*win, i);
            let color = match *label {
                "=" => (40, 160, 90),
                "C" => (170, 60, 60),
                "+" | "-" | "*" | "/" => (100, 70, 150),
                _ => (48, 52, 72),
            };
            framebuffer::fill_rounded_rect(bx as usize, by as usize, bw as usize, bh as usize, 6, color);
            framebuffer::draw_text(
                (bx + bw / 2 - 4) as usize,
                (by + 8) as usize,
                label,
                (240, 244, 255),
                None,
            );
        }
    }

    fn render_notes(&self, win: &Window) {
        let max_chars = ((win.w - 16) / CHAR_W).max(1) as usize;
        let max_lines = ((win.h - TITLE_H - 60) / CHAR_H).max(1) as usize;
        // Wrap the text into display lines (char wrap per paragraph).
        let mut lines: Vec<String> = Vec::new();
        for para in self.notes_text.split('\n') {
            if para.is_empty() {
                lines.push(String::new());
                continue;
            }
            let mut cur = String::new();
            for ch in para.chars() {
                cur.push(ch);
                if cur.chars().count() >= max_chars {
                    lines.push(core::mem::take(&mut cur));
                }
            }
            lines.push(cur);
        }
        let start = lines.len().saturating_sub(max_lines);
        let mut y = win.content_y() + 8;
        if self.notes_text.is_empty() {
            framebuffer::draw_text(
                (win.x + 8) as usize,
                y as usize,
                "Type here. Save writes notes.txt to RockyFS.",
                MUTED,
                None,
            );
        } else {
            for line in lines.iter().skip(start) {
                framebuffer::draw_text((win.x + 8) as usize, y as usize, line, TEXT, None);
                y += CHAR_H;
            }
            // Cursor block at the end of the text.
            let last_len = lines.last().map(|l| l.chars().count()).unwrap_or(0) as i32;
            let cursor_x = win.x + 8 + last_len * CHAR_W;
            let cursor_y =
                win.content_y() + 8 + (lines.len() - 1 - start) as i32 * CHAR_H;
            framebuffer::fill_rect(cursor_x as usize, cursor_y as usize, 8, 16, TERM_GREEN);
        }
        // Save button (bottom right) and status (bottom left).
        let (bx, by) = (win.x + win.w - 96, win.y + win.h - 38);
        framebuffer::fill_rounded_rect(bx as usize, by as usize, 84, 28, 6, (40, 160, 90));
        framebuffer::draw_text((bx + 26) as usize, (by + 6) as usize, "Save", WHITE, None);
        let status = if self.notes_save_err {
            "save failed".to_string()
        } else if let Some(tick) = self.notes_saved {
            let secs = tick / 18;
            format!("saved {:02}:{:02}:{:02} (notes.txt)", secs / 3600, (secs / 60) % 60, secs % 60)
        } else {
            format!("{} chars", self.notes_text.chars().count())
        };
        framebuffer::draw_text(
            (win.x + 12) as usize,
            (win.y + win.h - 31) as usize,
            &status,
            MUTED,
            None,
        );
    }

    fn render_monitor(&self, win: &Window) {
        let ticks = TIMER_TICKS.load(Ordering::SeqCst);
        let mut y = win.content_y() + 8;
        let tx = (win.x + 10) as usize;
        framebuffer::draw_text(tx, y as usize, "System Monitor - live", TEXT, None);
        y += 20;
        framebuffer::draw_text(
            tx,
            y as usize,
            &format!("Uptime: {}   ticks: {}", clock_string(), ticks),
            TERM_TEXT,
            None,
        );
        y += 18;
        framebuffer::draw_text(
            tx,
            y as usize,
            &format!("Memory: {} regions  -  heap 100 KiB", self.memory_regions),
            TERM_TEXT,
            None,
        );
        y += 18;
        framebuffer::draw_text(
            tx,
            y as usize,
            &format!(
                "Preemptions: {}   quantum 3 ticks (~165 ms)",
                crate::scheduler::PREEMPTIONS.load(Ordering::SeqCst)
            ),
            TERM_TEXT,
            None,
        );
        y += 18;
        framebuffer::draw_text(tx, y as usize, "Tasks (preemptive round-robin):", MUTED, None);
        y += 18;
        for (id, name, state, steps) in crate::scheduler::list_tasks() {
            framebuffer::draw_text(
                tx,
                y as usize,
                &format!("  #{} {} {:?}  steps {}", id, name, state, steps),
                TERM_TEXT,
                None,
            );
            y += 18;
        }
        y += 6;
        framebuffer::draw_text(tx, y as usize, "Activity:", MUTED, None);
        y += 18;
        let bar_w = (win.w - 20).min(420);
        framebuffer::draw_rect(tx, y as usize, bar_w as usize, 16, (70, 76, 100));
        let fill = ((ticks % 36) as i32 * (bar_w - 4) / 36).max(0);
        if fill > 0 {
            framebuffer::fill_rect((win.x + 12) as usize, (y + 2) as usize, fill as usize, 12, (90, 200, 140));
        }
        framebuffer::draw_text(
            (win.x + 14 + bar_w) as usize,
            y as usize,
            &format!("tick {}", ticks),
            MUTED,
            None,
        );
    }

    fn render_settings(&self, win: &Window) {
        let mut y = win.content_y() + 10;
        let tx = (win.x + 12) as usize;
        framebuffer::draw_text(tx, y as usize, "Rocky OS 0.1", TEXT, None);
        y += 20;
        framebuffer::draw_text(tx, y as usize, "64-bit Rust kernel - boots in QEMU (BIOS)", TERM_TEXT, None);
        y += 18;
        framebuffer::draw_text(
            tx,
            y as usize,
            "Screen 1280x720  -  heap 100 KiB  -  RockyFS 8 MiB",
            TERM_TEXT,
            None,
        );
        y += 18;
        framebuffer::draw_text(
            tx,
            y as usize,
            &format!(
                "Memory regions: {}   Files on disk: {}",
                self.memory_regions,
                self.files_cache.len()
            ),
            TERM_TEXT,
            None,
        );
        y += 28;
        framebuffer::draw_text(tx, y as usize, "Wallpaper theme - click a swatch to apply:", TEXT, None);
        let sw_y = win.content_y() + 150;
        for (i, theme) in THEMES.iter().enumerate() {
            let sx = win.x + 20 + (i as i32) * 170;
            framebuffer::fill_rect(sx as usize, sw_y as usize, 150, 22, theme.top);
            framebuffer::fill_rect(sx as usize, (sw_y + 22) as usize, 150, 20, theme.mid);
            framebuffer::fill_rect(sx as usize, (sw_y + 42) as usize, 150, 22, theme.bot);
            if i == self.theme {
                framebuffer::draw_rect((sx - 2) as usize, (sw_y - 2) as usize, 154, 68, WHITE);
            }
            framebuffer::draw_text((sx + 8) as usize, (sw_y + 70) as usize, theme.name, TEXT, None);
        }
    }
}

/// Calculator button labels, 4 columns x 5 rows.
fn calc_labels() -> [&'static str; 20] {
    ["7", "8", "9", "/", "4", "5", "6", "*", "1", "2", "3", "-", "0", ".", "=", "+", "C", "(", ")", ""]
}

fn calc_button_rect(win: Window, i: usize) -> (i32, i32, i32, i32) {
    let col = (i % 4) as i32;
    let row = (i / 4) as i32;
    let bw = 56;
    let bh = 32;
    let bx = win.x + 10 + col * (bw + 6);
    let by = win.content_y() + 44 + row * (bh + 4);
    (bx, by, bw, bh)
}

/// Evaluate a simple expression: numbers with + - * / and
/// parentheses, left-to-right with * / before + -. Returns the
/// result or "err".
fn eval_expr(expr: &str) -> String {
    let mut values: Vec<f64> = Vec::new();
    let mut ops: Vec<char> = Vec::new();
    let apply = |values: &mut Vec<f64>, ops: &mut Vec<char>| -> Result<(), ()> {
        let op = ops.pop().ok_or(())?;
        let b = values.pop().ok_or(())?;
        let a = values.pop().ok_or(())?;
        let r = match op {
            '+' => a + b,
            '-' => a - b,
            '*' => a * b,
            '/' => {
                if b == 0.0 {
                    return Err(());
                }
                a / b
            }
            _ => return Err(()),
        };
        values.push(r);
        Ok(())
    };
    let prec = |op: char| match op {
        '+' | '-' => 1,
        '*' | '/' => 2,
        _ => 0,
    };
    let mut num = String::new();
    for c in expr.chars() {
        match c {
            '0'..='9' | '.' => num.push(c),
            '+' | '-' | '*' | '/' => {
                if !num.is_empty() {
                    match num.parse() {
                        Ok(v) => values.push(v),
                        Err(_) => return "err".into(),
                    }
                    num.clear();
                }
                while let Some(&top) = ops.last() {
                    if top != '(' && prec(top) >= prec(c) {
                        if apply(&mut values, &mut ops).is_err() {
                            return "err".into();
                        }
                    } else {
                        break;
                    }
                }
                ops.push(c);
            }
            '(' => ops.push(c),
            ')' => {
                if !num.is_empty() {
                    match num.parse() {
                        Ok(v) => values.push(v),
                        Err(_) => return "err".into(),
                    }
                    num.clear();
                }
                while let Some(&top) = ops.last() {
                    if top == '(' {
                        break;
                    }
                    if apply(&mut values, &mut ops).is_err() {
                        return "err".into();
                    }
                }
                if ops.pop() != Some('(') {
                    return "err".into();
                }
            }
            _ => return "err".into(),
        }
    }
    if !num.is_empty() {
        match num.parse() {
            Ok(v) => values.push(v),
            Err(_) => return "err".into(),
        }
    }
    while !ops.is_empty() {
        if apply(&mut values, &mut ops).is_err() {
            return "err".into();
        }
    }
    match values.pop() {
        Some(v) if values.is_empty() => {
            // No float helpers in no_std: integrality via a
            // round-trip through i64, abs by hand.
            let av = if v < 0.0 { -v } else { v };
            if v == (v as i64) as f64 && av < 1e15 {
                format!("{}", v as i64)
            } else {
                format!("{:.4}", v)
            }
        }
        _ => "err".into(),
    }
}

/// Classic arrow cursor: black shadow offset by (1,1), white on top.
fn draw_cursor(x: i32, y: i32) {
    const CURSOR: [&str; 14] = [
        "X.........",
        "XX........",
        "XXX.......",
        "XXXX......",
        "XXXXX.....",
        "XXXXXX....",
        "XXXXXXX...",
        "XXXXXXXX..",
        "XXXXXXXXX.",
        "XXXXXX....",
        "XX.XX.....",
        "X...XX....",
        "....XX....",
        "....XX....",
    ];
    for (dy, row) in CURSOR.iter().enumerate() {
        for (dx, c) in row.chars().enumerate() {
            if c == 'X' {
                framebuffer::set_pixel((x + dx as i32 + 1) as usize, (y + dy as i32 + 1) as usize, BLACK);
            }
        }
    }
    for (dy, row) in CURSOR.iter().enumerate() {
        for (dx, c) in row.chars().enumerate() {
            if c == 'X' {
                framebuffer::set_pixel((x + dx as i32) as usize, (y + dy as i32) as usize, WHITE);
            }
        }
    }
}

/// Enter the desktop. Returns when the user exits (Esc / Exit).
pub fn run(memory_regions: usize) {
    crate::serial_print(format_args!("desktop: starting\n"));
    x86_64::instructions::interrupts::without_interrupts(|| {
        crate::mouse::init();
    });
    let mut desktop = Desktop::new(memory_regions);
    desktop.render();
    loop {
        if desktop.exit {
            break;
        }
        // Serial input: drain everything waiting, then render once.
        // A burst of typed characters must not cost a full redraw
        // per byte — at ~150 ms a frame, 45 chars would lag 7 s.
        let mut fed = false;
        for _ in 0..64 {
            match crate::shell::serial_read_byte() {
                Some(0x1b) => {
                    desktop.exit = true;
                    break;
                }
                Some(byte) => {
                    desktop.feed_char(byte as char);
                    fed = true;
                }
                None => break,
            }
        }
        if fed {
            desktop.render();
        }
        if desktop.exit {
            break;
        }
        // PS/2 keyboard input, batched the same way.
        let chars = crate::shell::keyboard_chars();
        let mut kb_fed = false;
        for c in chars {
            if c == '\u{1b}' {
                desktop.exit = true;
                break;
            }
            desktop.feed_char(c);
            kb_fed = true;
        }
        if kb_fed {
            desktop.render();
        }
        if desktop.exit {
            break;
        }
        desktop.handle_mouse();
        let sec = TIMER_TICKS.load(Ordering::SeqCst) / 18;
        if sec != desktop.last_second {
            desktop.last_second = sec;
            desktop.refresh_files();
            desktop.render();
        }
        x86_64::instructions::interrupts::enable_and_hlt();
    }
    crate::serial_print(format_args!("desktop: exiting\n"));
    framebuffer::clear();
}
