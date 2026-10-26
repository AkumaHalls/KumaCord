//! Chat notification and tray autotest (a child module of `autotest`): `notify-a` / `notify-b` /
//! `notify-c` (v0.7). Three clients on one hub: names `ntA<n>`, `ntB<n>`, `ntC<n>`.
//!
//! B is the one under test; A writes when B says it is ready (a "ntready <n>" line in the first
//! text channel), C is a bystander. The phases:
//!
//! 1. B has the channel open and in front: A writes; nothing counts, nothing sounds.
//! 2. B is behind another window ("Só mensagens diretas e menções", the default): a channel line
//!    counts as unread and asks for the soft channel sound, but no Windows notification.
//! 3. B asks for "Todas as mensagens": the next line also raises the notification (title, preview),
//!    the taskbar flash and the badge.
//! 4. A writes a DM: B's DM counter, the DM sound, the notification and the badge (DMs only).
//! 5. B comes to the front: the open channel is read by itself, opening the DM reads it; the badge
//!    goes to zero.
//! 6. B blocks A: A's next channel line is silent (no counter, sound or notification) but still
//!    shown.
//! 7. (`VOX_TRAY_TEST=1`) the tray: the close button hides the window (really hidden, the tray
//!    icon kept), the app idles for 8 s measuring its CPU, A writes and the hidden app still
//!    receives it, notifies and counts; a tray click brings the window back; a clicked
//!    notification opens the channel; "Sair" closes the app with exit code 0.
//!
//! C must never see anything of the DM between A and B. Everything is recorded, not shown: the
//! Windows notification and the flash are only counted unless `VOX_NOTIFY_REAL=1`, and with
//! `VOX_NO_PLAYBACK=1` the sounds are only requested. Silent.

use std::time::{Duration, Instant};

use super::Auto;
use crate::app::notify::{Target, WinNotify};
use crate::app::App;
use crate::backend::Cmd;
use crate::tray::TrayEvent;

/// B idles hidden this long before A writes to it (also the CPU measuring window).
const REST_SECS: u64 = 8;
pub const REST_STEP: u32 = 16;

#[derive(Default)]
pub struct State {
    ch: i64,
    done: u32,
    dm_sent: bool,
    pending: Option<(Instant, String)>,
    sounds: usize,
    balloons: u32,
    flashes: u32,
    dm_alerts: u32,
    cpu0: Option<(u64, Instant)>,
    seen_at: Option<Instant>,
    tray: bool,
}

fn log(what: &str) {
    tracing::info!(target: "autotest", "AUTOTEST notify: {what}");
}

/// Kernel + user CPU time of this process, in 100 ns units.
#[cfg(not(windows))]
fn cpu_time() -> u64 {
    0
}

#[cfg(not(windows))]
fn window_state(_app: &App) -> (bool, bool) {
    (true, false)
}

/// Kernel + user CPU time of this process, in 100 ns units.
#[cfg(windows)]
fn cpu_time() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z, z, z, z);
    // SAFETY: plain query with live out-pointers.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    t(k) + t(u)
}

#[cfg(windows)]
fn window_state(app: &App) -> (bool, bool) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
    let Some(h) = app.video.parent else { return (false, false) };
    // SAFETY: queries on our own window.
    unsafe { (IsWindowVisible(h as _) != 0, IsIconic(h as _) != 0) }
}

impl Auto {
    fn nt_tag(&self, what: &str) -> String {
        format!("{what} {}", self.args.run_id)
    }

    /// The name of the same-numbered client with another letter: `ntA1` -> `ntB1`.
    fn nt_name(&self, letter: char) -> String {
        let n = &self.args.name;
        format!("nt{letter}{}", n.get(3..).unwrap_or(""))
    }

    fn nt_online(app: &App, name: &str) -> Option<vox_proto::User> {
        app.online.iter().find(|u| u.username == name).cloned()
    }

    fn nt_ready(&mut self, app: &mut App, n: u32) {
        let content = self.nt_tag(&format!("ntready {n}"));
        app.backend.send(Cmd::SendMessage { channel: self.nt.ch, content, extras: Default::default() });
        log(&format!("told A I am ready for phase {n}"));
    }

    fn nt_snap(&mut self, app: &App) {
        let s = &app.notify.stats;
        self.nt.sounds = s.sounds.len();
        self.nt.balloons = s.balloons;
        self.nt.flashes = s.flashes;
        self.nt.dm_alerts = app.dms.dm_alerts;
    }

    fn nt_unread(app: &App, ch: i64) -> u32 {
        app.unread.get(&ch).copied().unwrap_or(0)
    }

    fn nt_wait_ready(&mut self, app: &App, timeout: u64) -> Result<bool, Option<i32>> {
        if self.logged_in(app) && Self::nt_online(app, &self.nt_name('A')).is_some() && Self::nt_online(app, &self.nt_name('C')).is_some() {
            return Ok(true);
        }
        if self.waited(timeout) {
            return Err(self.fail("the other two clients never came online"));
        }
        Ok(false)
    }

    // ------------------------------------------------------------ A

    pub(super) fn run_notify_a(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => self.connect(app),
            1 => {
                match self.nt_wait_ready(app, 60) {
                    Err(code) => return code,
                    Ok(false) => return None,
                    Ok(true) => {}
                }
                self.nt.ch = Self::first_text(app)?;
                self.goto(2, "everybody is online; waiting for B");
            }
            2 => {
                if let Some((at, text)) = self.nt.pending.clone() {
                    if Instant::now() >= at {
                        self.nt.pending = None;
                        Self::say(app, &text);
                        log(&format!("wrote {text:?} after the rest"));
                    }
                    return None;
                }
                let n = self.nt.done + 1;
                if !Self::saw_message(app, &self.nt_tag(&format!("ntready {n}"))) {
                    if self.waited(240) {
                        return self.fail(&format!("B never got ready for phase {n}"));
                    }
                    return None;
                }
                self.goto(2, &format!("B is ready for phase {n}"));
                match n {
                    1 | 2 | 3 | 6 => Self::say(app, &self.nt_tag(&format!("nt{n}"))),
                    4 => {
                        let b = Self::nt_online(app, &self.nt_name('B'))?;
                        app.open_dm(b.id);
                        self.goto(3, "opening the DM with B");
                        self.nt.done = n;
                        return None;
                    }
                    7 => self.nt.pending = Some((Instant::now() + Duration::from_secs(REST_SECS + 1), self.nt_tag("nt7"))),
                    8 => {
                        Self::say(app, &self.nt_tag("ntdone"));
                        self.nt.pending = None;
                        self.goto(4, "B is done; telling C and finishing");
                    }
                    _ => {}
                }
                self.nt.done = n;
            }
            3 => {
                // The DM with B is open: write.
                if let Some(dm) = app.dms.selected.filter(|d| app.dms.conv(*d).is_some()) {
                    if !self.nt.dm_sent {
                        self.nt.dm_sent = true;
                        app.dms.input = self.nt_tag("nt4");
                        app.send_dm();
                        let _ = dm;
                    }
                    // Back to the channel so the next phases are about the channel.
                    app.select_text(self.nt.ch);
                    self.goto(2, "wrote the DM");
                } else if self.waited(20) {
                    return self.fail("the DM with B did not open");
                }
            }
            4 if self.waited(3) => return self.pass(),
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ C

    pub(super) fn run_notify_c(&mut self, app: &mut App) -> Option<i32> {
        match self.step {
            0 => {
                self.connect(app);
                app.notify.focus_override = Some(false);
                app.settings.notify.windows = WinNotify::All;
            }
            1 if self.logged_in(app) => {
                self.nt.ch = Self::first_text(app)?;
                self.goto(2, "connected; behind another window, watching");
            }
            1 if self.waited(30) => return self.fail("could not log in"),
            2 => {
                if Self::saw_message(app, &self.nt_tag("ntdone")) {
                    let unread = Self::nt_unread(app, self.nt.ch);
                    let s = &app.notify.stats;
                    log(&format!("channel unread={unread} balloons={} sounds={:?}; DM list {} entries, DM unread {}, DM logs {}", s.balloons, s.sounds, app.dms.convs.len(), app.dms.total_unread(), app.dms.logs.len()));
                    // Four of A's lines (nt1, nt2, nt3, nt6) plus B's markers reach a bystander.
                    if unread < 4 {
                        return self.fail(&format!("I saw only {unread} unread channel lines"));
                    }
                    if s.sounds.is_empty() || s.balloons == 0 {
                        return self.fail("the channel lines raised no sound or notification here");
                    }
                    if !app.dms.convs.is_empty() || app.dms.total_unread() != 0 || !app.dms.logs.is_empty() || app.dms.dm_alerts != 0 {
                        return self.fail("the DM between A and B leaked to a bystander");
                    }
                    if app.logs.values().flat_map(|l| &l.msgs).any(|m| m.content.contains("nt4")) {
                        return self.fail("the DM text shows up in a channel log");
                    }
                    return self.pass();
                } else if self.waited(300) {
                    return self.fail("A never finished");
                }
            }
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ B

    pub(super) fn run_notify_b(&mut self, app: &mut App) -> Option<i32> {
        let a_name = self.nt_name('A');
        match self.step {
            0 => {
                self.connect(app);
                self.nt.tray = app.notify.allow_real_tray();
                log(&format!("real tray icon: {}", self.nt.tray));
            }
            1 => {
                match self.nt_wait_ready(app, 60) {
                    Err(code) => return code,
                    Ok(false) => return None,
                    Ok(true) => {}
                }
                self.nt.ch = Self::first_text(app)?;
                app.select_text(self.nt.ch);
                app.notify.focus_override = Some(true);
                self.nt_snap(app);
                self.nt_ready(app, 1);
                self.goto(2, "channel open and in front; phase 1");
            }
            2 => {
                if Self::saw_message(app, &self.nt_tag("nt1")) {
                    let s = &app.notify.stats;
                    if Self::nt_unread(app, self.nt.ch) != 0 || s.sounds.len() != self.nt.sounds || s.balloons != self.nt.balloons || s.flashes != self.nt.flashes {
                        return self.fail(&format!("the open channel in front was not silent: unread {}, {s:?}", Self::nt_unread(app, self.nt.ch)));
                    }
                    log("phase 1 ok: the conversation I am reading is silent");
                    self.goto(3, "phase 1 done");
                } else if self.waited(30) {
                    return self.fail("A's phase-1 line never came");
                }
            }
            3 if self.waited(4) => {
                app.notify.focus_override = Some(false);
                self.nt_snap(app);
                self.nt_ready(app, 2);
                self.goto(4, "behind another window; phase 2");
            }
            4 => {
                if Self::saw_message(app, &self.nt_tag("nt2")) {
                    let s = app.notify.stats.clone();
                    let unread = Self::nt_unread(app, self.nt.ch);
                    if unread != 1 || s.sounds.len() != self.nt.sounds + 1 || s.sounds.last() != Some(&"channel_message") {
                        return self.fail(&format!("phase 2: unread {unread}, sounds {:?}", s.sounds));
                    }
                    if s.balloons != self.nt.balloons || s.flashes != self.nt.flashes || s.badge != 0 {
                        return self.fail(&format!("phase 2: the default setting must not notify Windows about a channel line: {s:?}"));
                    }
                    log("phase 2 ok: unread 1, channel sound, no Windows notification, no badge");
                    self.goto(5, "phase 2 done");
                } else if self.waited(30) {
                    return self.fail("A's phase-2 line never came");
                }
            }
            5 if self.waited(4) => {
                app.settings.notify.windows = WinNotify::All;
                self.nt_snap(app);
                self.nt_ready(app, 3);
                self.goto(6, "\"Todas as mensagens\"; phase 3");
            }
            6 => {
                if Self::saw_message(app, &self.nt_tag("nt3")) {
                    let s = app.notify.stats.clone();
                    let unread = Self::nt_unread(app, self.nt.ch);
                    let want_title = format!("{a_name} em #{}", app.channels.iter().find(|c| c.id == self.nt.ch).map(|c| c.name.as_str()).unwrap_or("?"));
                    if unread != 2 || s.sounds.len() != self.nt.sounds + 1 {
                        return self.fail(&format!("phase 3: unread {unread}, sounds {:?}", s.sounds));
                    }
                    if s.balloons != self.nt.balloons + 1 || s.flashes <= self.nt.flashes || s.last_balloon.as_ref() != Some(&(want_title.clone(), self.nt_tag("nt3"))) {
                        return self.fail(&format!("phase 3: notification wrong (wanted {want_title:?}): {s:?}"));
                    }
                    if s.badge != 2 {
                        return self.fail(&format!("phase 3: badge {} instead of 2", s.badge));
                    }
                    log(&format!("phase 3 ok: unread 2, notification {:?}, flashes {}, badge {}", s.last_balloon, s.flashes, s.badge));
                    self.goto(7, "phase 3 done");
                } else if self.waited(30) {
                    return self.fail("A's phase-3 line never came");
                }
            }
            7 if self.waited(4) => {
                app.settings.notify.windows = WinNotify::DmsAndMentions;
                self.nt_snap(app);
                self.nt_ready(app, 4);
                self.goto(8, "back to the default; phase 4 (a DM)");
            }
            8 => {
                let a = Self::nt_online(app, &a_name)?;
                let conv = app.dms.conv_with(a.id).cloned();
                if conv.as_ref().and_then(|c| c.last_message.as_ref()).is_some_and(|m| m.content == self.nt_tag("nt4")) {
                    let c = conv?;
                    let s = app.notify.stats.clone();
                    if c.unread != 1 || app.dms.dm_alerts != self.nt.dm_alerts + 1 {
                        return self.fail(&format!("phase 4: DM unread {}, DM sounds {} (was {})", c.unread, app.dms.dm_alerts, self.nt.dm_alerts));
                    }
                    if s.balloons != self.nt.balloons + 1 || s.last_balloon.as_ref() != Some(&(a_name.clone(), self.nt_tag("nt4"))) {
                        return self.fail(&format!("phase 4: notification wrong: {s:?}"));
                    }
                    if s.badge != 1 {
                        return self.fail(&format!("phase 4: the badge counts only DMs here: {} instead of 1", s.badge));
                    }
                    log(&format!("phase 4 ok: DM unread 1, DM sound, notification {:?}, badge 1", s.last_balloon));
                    self.goto(9, "phase 4 done");
                } else if self.waited(30) {
                    return self.fail("A's DM never came");
                }
            }
            9 if self.waited(1) => {
                app.notify.focus_override = Some(true);
                self.nt_ready(app, 5);
                self.goto(10, "came to the front; phase 5");
            }
            10 => {
                let a = Self::nt_online(app, &a_name)?;
                if Self::nt_unread(app, self.nt.ch) == 0 {
                    if app.dms.conv_with(a.id).map(|c| c.unread) != Some(1) || app.notify.stats.badge != 1 {
                        return self.fail("phase 5: the DM must still be unread until opened");
                    }
                    let dm = app.dms.conv_with(a.id)?.id;
                    app.select_dm(dm);
                    self.goto(11, "the open channel was read by coming to the front; opened the DM");
                } else if self.waited(5) {
                    return self.fail("phase 5: the open channel stayed unread after coming to the front");
                }
            }
            11 => {
                let a = Self::nt_online(app, &a_name)?;
                if app.dms.conv_with(a.id).map(|c| c.unread) == Some(0) && app.notify.stats.badge == 0 {
                    log("phase 5 ok: opening the DM read it, the badge is gone");
                    // Phase 6: back to the channel, behind another window, A blocked.
                    app.select_text(self.nt.ch);
                    app.notify.focus_override = Some(false);
                    app.settings.notify.windows = WinNotify::All;
                    app.block_user(&a, true);
                    self.nt_snap(app);
                    self.nt.sounds = app.notify.stats.sounds.len();
                    self.nt_ready(app, 6);
                    self.goto(12, "A is blocked; phase 6");
                } else if self.waited(5) {
                    return self.fail(&format!("phase 5: DM unread {:?}, badge {}", app.dms.conv_with(a.id).map(|c| c.unread), app.notify.stats.badge));
                }
            }
            12 => {
                if Self::saw_message(app, &self.nt_tag("nt6")) {
                    let seen = *self.nt.seen_at.get_or_insert_with(Instant::now);
                    if seen.elapsed() < Duration::from_secs(2) {
                        return None;
                    }
                    let s = app.notify.stats.clone();
                    let unread = Self::nt_unread(app, self.nt.ch);
                    if unread != 0 || s.sounds.len() != self.nt.sounds || s.balloons != self.nt.balloons || s.flashes != self.nt.flashes || s.badge != 0 {
                        return self.fail(&format!("phase 6: a blocked person's line must be silent: unread {unread}, {s:?}"));
                    }
                    let a = Self::nt_online(app, &a_name)?;
                    app.block_user(&a, false);
                    log("phase 6 ok: shown in the channel but silent");
                    self.nt.seen_at = None;
                    self.goto(13, "phase 6 done");
                } else if self.waited(30) {
                    return self.fail("A's phase-6 line never came");
                }
            }
            13 => {
                if !self.nt.tray {
                    log("phase 7 skipped: no real tray (set VOX_TRAY_TEST=1)");
                    self.nt_ready(app, 8);
                    self.goto(22, "told A; finishing");
                    return None;
                }
                if !app.notify.has_tray() {
                    if self.waited(15) {
                        return self.fail("the tray icon was never created");
                    }
                    return None;
                }
                app.notify.focus_override = None;
                app.settings.notify.windows = WinNotify::All;
                app.settings.notify.tray_on_close = true;
                app.select_text(self.nt.ch);
                let dm = app.dms.convs.first().map(|c| c.id);
                if let Some(dm) = dm {
                    app.select_dm(dm); // so the clicked notification has somewhere to move us from
                }
                app.repaint_ctx().send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                self.goto(14, "pressed the window's close button; phase 7");
            }
            14 => {
                let (visible, _) = window_state(app);
                if app.notify.hidden && !visible {
                    log("phase 7: the close button hid the window (tray icon kept); the app is running");
                    self.nt_snap(app);
                    self.nt_ready(app, 7);
                    self.nt.cpu0 = Some((cpu_time(), Instant::now()));
                    self.goto(REST_STEP, "hidden; resting");
                } else if self.waited(5) {
                    return self.fail(&format!("the close button did not hide the window (hidden {}, visible {visible})", app.notify.hidden));
                }
            }
            REST_STEP => {
                if Self::saw_message(app, &self.nt_tag("nt7")) {
                    let (c0, t0) = self.nt.cpu0?;
                    let secs = t0.elapsed().as_secs_f64();
                    let cpu = (cpu_time() - c0) as f64 / 1e7;
                    log(&format!("hidden for {secs:.1} s: {cpu:.3} s of CPU = {:.2} % of one core (the message that ended the rest included)", 100.0 * cpu / secs));
                    let s = app.notify.stats.clone();
                    let unread = Self::nt_unread(app, self.nt.ch);
                    if s.balloons != self.nt.balloons + 1 || s.sounds.len() != self.nt.sounds + 1 || unread == 0 {
                        return self.fail(&format!("phase 7: the hidden app did not notify: unread {unread}, {s:?}"));
                    }
                    if 100.0 * cpu / secs > 3.0 {
                        return self.fail(&format!("a hidden window used {:.1} % of a core", 100.0 * cpu / secs));
                    }
                    log("phase 7: the hidden app received the message, counted it, played the sound and raised the notification");
                    app.notify.simulate(TrayEvent::Open);
                    self.goto(17, "clicked the tray icon");
                } else if self.waited(REST_SECS + 40) {
                    return self.fail("the hidden app never received A's line");
                }
            }
            17 => {
                let (visible, iconic) = window_state(app);
                if !app.notify.hidden && visible && !iconic && app.notify.stats.restores >= 1 {
                    log(&format!("phase 7: the tray click restored the window (visible, not minimized); badge {}", app.notify.stats.badge));
                    // Hide again, with the DM open, then click the notification.
                    let dm = app.dms.convs.first().map(|c| c.id)?;
                    app.select_dm(dm);
                    app.repaint_ctx().send_viewport_cmd(eframe::egui::ViewportCommand::Close);
                    self.goto(18, "closed again");
                } else if self.waited(8) {
                    return self.fail(&format!("the tray click did not restore the window (hidden {}, visible {visible}, minimized {iconic})", app.notify.hidden));
                }
            }
            18 => {
                if app.notify.hidden && !window_state(app).0 {
                    app.notify.simulate(TrayEvent::BalloonClicked);
                    self.goto(19, "clicked the notification");
                } else if self.waited(5) {
                    return self.fail("the second close did not hide the window");
                }
            }
            19 => {
                let (visible, _) = window_state(app);
                let opened = app.selected == Some(self.nt.ch) && app.dms.selected.is_none();
                if visible && !app.notify.hidden && opened {
                    log(&format!("phase 7: the clicked notification restored the window on {:?}", Target::Channel(self.nt.ch)));
                    self.nt_ready(app, 8);
                    self.goto(21, "told A; then \"Sair\"");
                } else if self.waited(8) {
                    return self.fail(&format!("the clicked notification did not open the channel (visible {visible}, selected {:?}, dm {:?})", app.selected, app.dms.selected));
                }
            }
            21 if self.waited(2) => {
                // "Sair" ends the process, so the verdict is written first (exit code 0).
                crate::EXIT_CODE.store(0, std::sync::atomic::Ordering::Relaxed);
                tracing::info!(target: "autotest", "AUTOTEST {:?} RESULT: PASS if \"Sair\" closes the app now (t+{:.1}s)", self.role, self.started.elapsed().as_secs_f32());
                app.notify.simulate(TrayEvent::Quit);
                self.goto(20, "used \"Sair\" in the tray menu");
            }
            20 if self.waited(10) => return self.fail("\"Sair\" did not close the app"),
            22 if self.waited(2) => return self.pass(),
            _ => {}
        }
        None
    }
}
