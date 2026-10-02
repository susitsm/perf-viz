//! The reader tuner window: measures the disk, tries reader settings, and offers the best ones.
//! The measuring itself is `perf_reader::tune`; this is the window around it.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use eframe::egui::{self, RichText, vec2};
use perf_reader::{
    synth,
    tune::{self, Event, Report, Settings},
};

use crate::theme::Pal;

#[derive(Default)]
struct Shared {
    lines: Vec<String>,
    running: bool,
    result: Option<Result<Report, String>>,
}

/// What the window asks of the app this frame.
#[derive(Default)]
pub struct Outcome {
    /// Settings to store (the user saved the tuning results).
    pub apply: Option<Settings>,
    /// The app was asked to quit and the question about unsaved results is settled.
    pub quit: bool,
    /// A file the user asked to open while results were unsaved, now free to open.
    pub open_file: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    Save,
    Discard,
    Cancel,
}

/// What the "save the tuning results?" question interrupted.
enum Ask {
    Close,
    Quit,
    Open(PathBuf),
}

#[derive(Default)]
pub struct Tuner {
    pub open: bool,
    /// The result was saved or deliberately discarded.
    settled: bool,
    /// Asking whether to save the results before going on.
    confirm: Option<Ask>,
    source: Option<PathBuf>,
    /// Tune on a generated recording instead of a real one.
    synthetic: bool,
    /// Size of the generated recording, in GB (0: the default, [`DEFAULT_SYNTH_GB`]).
    synth_gb: f64,
    /// Where the generated recording is written (default: next to the chosen recording, else home).
    synth_dir: Option<PathBuf>,
    dir_picker: Option<mpsc::Receiver<Option<PathBuf>>>,
    shared: Arc<Mutex<Shared>>,
    cancel: Arc<AtomicBool>,
    picker: Option<mpsc::Receiver<Option<PathBuf>>>,
}

const DEFAULT_SYNTH_GB: f64 = 4.0;

fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("perf-viz tuner")
}

fn describe(event: &Event) -> String {
    match event {
        Event::Info(message) => message.clone(),
        Event::Probe { secs, gbps } => format!("  {secs:.0} s: {gbps:.2} GB/s"),
        Event::Trial {
            what,
            settings,
            gbps,
            long,
        } => format!(
            "  {what}: {} readers, {} MiB regions, {} open: {gbps:.2} GB/s{}",
            settings.readers,
            settings.region_mb,
            settings.in_flight,
            if *long { " (full length)" } else { "" }
        ),
    }
}

impl Tuner {
    /// Show the window, tuning for `current` (the file that is open) unless another was chosen.
    pub fn open_for(&mut self, ctx: &egui::Context, current: Option<&Path>) {
        if self.open {
            // Already open: bring that window to the front instead.
            ctx.send_viewport_cmd_to(viewport_id(), egui::ViewportCommand::Focus);
            return;
        }
        self.open = true;
        if self.source.is_none() {
            self.source = current.map(Path::to_path_buf);
        }
    }

    /// Stop a run and wait (briefly) for it to stop; called when the app exits.
    pub fn shutdown(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        for _ in 0..100 {
            if !self.running() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// A finished tuning whose results were neither saved nor discarded.
    pub fn unsaved(&self) -> bool {
        !self.settled && matches!(self.shared.lock().unwrap().result, Some(Ok(_)))
    }

    /// The app is closing: ask about unsaved results first.
    pub fn ask_before_quit(&mut self) {
        self.open = true;
        self.confirm = Some(Ask::Quit);
    }

    /// The user wants to open `path`: ask about unsaved results first. The file comes back through
    /// [`Outcome::open_file`] once the question is settled.
    pub fn ask_before_open(&mut self, path: PathBuf) {
        self.confirm = Some(Ask::Open(path));
    }

    fn running(&self) -> bool {
        self.shared.lock().unwrap().running
    }

    fn synth_gb(&self) -> f64 {
        if self.synth_gb > 0.0 {
            self.synth_gb
        } else {
            DEFAULT_SYNTH_GB
        }
    }

    /// The folder a generated recording goes to: the chosen one, else next to the recording, else home.
    fn synth_folder(&self) -> PathBuf {
        self.synth_dir
            .clone()
            .or_else(|| {
                self.source
                    .as_ref()
                    .and_then(|path| path.canonicalize().ok())
                    .and_then(|path| path.parent().map(Path::to_path_buf))
            })
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn can_start(&self) -> bool {
        self.synthetic || self.source.is_some()
    }

    fn start(&mut self, ctx: &egui::Context) {
        if !self.can_start() {
            return;
        }
        let (source, synthetic) = (self.source.clone(), self.synthetic);
        let (folder, bytes) = (self.synth_folder(), (self.synth_gb() * 1e9) as u64);
        self.cancel.store(false, Ordering::Relaxed);
        self.settled = false;
        *self.shared.lock().unwrap() = Shared {
            lines: Vec::new(),
            running: true,
            result: None,
        };
        let (shared, cancel, ctx) = (self.shared.clone(), self.cancel.clone(), ctx.clone());
        std::thread::spawn(move || {
            let say = |text: String| {
                shared.lock().unwrap().lines.push(text);
                ctx.request_repaint();
            };
            #[cfg_attr(not(any(test, feature = "scripted")), allow(unused_mut))]
            let mut load_options = tune::Options::default();
            // A testing aid: scale every duration (0.1 makes a run about ten times shorter).
            #[cfg(any(test, feature = "scripted"))]
            if let Some(factor) = std::env::var("PERF_VIZ_TUNE_SCALE")
                .ok()
                .and_then(|text| text.parse::<f64>().ok())
            {
                load_options = load_options.scaled(factor);
            }
            // Dropped at the end of the thread: a generated recording is deleted however the run ends.
            let mut generated: Option<synth::Temp> = None;
            let target: Result<PathBuf, String> = if synthetic {
                synth::remove_stale(&folder);
                let path = synth::temp_path(&folder);
                match synth::free_bytes(&folder) {
                    Some(free) if free < bytes + (256 << 20) => Err(format!(
                        "not enough free space in {}: {:.1} GB free, {:.1} GB needed",
                        folder.display(),
                        free as f64 / 1e9,
                        bytes as f64 / 1e9
                    )),
                    _ => {
                        say(format!(
                            "Writing a {:.1} GB generated recording to {}",
                            bytes as f64 / 1e9,
                            folder.display()
                        ));
                        generated = Some(synth::Temp(path.clone()));
                        synth::write(&path, bytes, &cancel, |done| {
                            say(format!("  {:.1} GB written", done as f64 / 1e9))
                        })
                        .map(|_| path)
                        .map_err(|error| error.to_string())
                    }
                }
            } else {
                source.ok_or_else(|| "no recording chosen".to_string())
            };
            let outcome = target.and_then(|path| tune::tune(&path, &load_options, &cancel, &mut |event| say(describe(&event))).map_err(|error| error.to_string())).map(|mut report| {
                if synthetic {
                    report.notes.push("tuned on a generated recording; a real one may behave a little differently".into());
                }
                report
            });
            drop(generated);
            let mut shared_state = shared.lock().unwrap();
            shared_state.running = false;
            shared_state.result = Some(outcome);
            ctx.request_repaint();
        });
    }

    fn choose_folder(&mut self, ctx: &egui::Context) {
        if self.dir_picker.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(
                rfd::FileDialog::new()
                    .set_title("Folder for the generated recording (on the disk to tune for)")
                    .pick_folder(),
            );
            ctx.request_repaint();
        });
        self.dir_picker = Some(receiver);
    }

    fn choose_file(&mut self, ctx: &egui::Context) {
        if self.picker.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(
                rfd::FileDialog::new()
                    .set_title("Choose a perf.data file to tune with")
                    .pick_file(),
            );
            ctx.request_repaint();
        });
        self.picker = Some(receiver);
    }

    fn poll_pickers(&mut self) {
        if let Some(receiver) = &self.picker {
            match receiver.try_recv() {
                Ok(picked) => {
                    self.picker = None;
                    if picked.is_some() {
                        self.source = picked;
                        self.shared.lock().unwrap().result = None;
                        self.settled = false;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(_) => self.picker = None,
            }
        }
        if let Some(receiver) = &self.dir_picker {
            match receiver.try_recv() {
                Ok(picked) => {
                    self.dir_picker = None;
                    if picked.is_some() {
                        self.synth_dir = picked;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(_) => self.dir_picker = None,
            }
        }
    }

    /// A testing aid: start tuning as soon as the window opens (clicks cannot reach this window).
    #[cfg(any(test, feature = "scripted"))]
    fn autostart_for_testing(&mut self, ctx: &egui::Context) {
        if self.open
            && std::env::var_os("PERF_VIZ_TUNE_AUTOSTART").is_some()
            && self.source.is_some()
            && !self.running()
            && self.shared.lock().unwrap().result.is_none()
            && self.shared.lock().unwrap().lines.is_empty()
        {
            self.start(ctx);
        }
    }

    fn draw_intro(&self, ui: &mut egui::Ui) {
        ui.label(format!(
            "Measures how fast your disk reads for several seconds, then tries reader settings on reads of about 10 seconds each and picks the fastest cheap ones. {} \
             The whole run takes about a minute and a half.",
            if self.synthetic {
                "It writes a generated recording to the folder below, tunes on it, and deletes it afterwards."
            } else {
                "It only reads the recording (nothing is written, no extra disk space is needed); a short recording is read repeatedly."
            }
        ));
    }

    fn draw_synthetic_choice(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, running: bool) {
        ui.add_enabled_ui(!running, |ui| {
            ui.horizontal(|ui| {
                ui.label("Size:");
                let mut gb = self.synth_gb();
                if ui
                    .add(
                        egui::DragValue::new(&mut gb)
                            .range(1.0..=100.0)
                            .speed(0.25)
                            .suffix(" GB"),
                    )
                    .changed()
                {
                    self.synth_gb = gb;
                }
                ui.label("Folder:");
                ui.label(RichText::new(self.synth_folder().display().to_string()).strong());
                if ui
                    .add_enabled(self.dir_picker.is_none(), egui::Button::new("Choose…"))
                    .clicked()
                {
                    self.choose_folder(ctx);
                }
            });
        });
        ui.weak(format!("Needs about {:.0} GB of free space in that folder while it runs; choose a folder on the disk you want to tune for (/tmp is often memory).", self.synth_gb() + 0.3));
    }

    fn draw_recording_choice(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, running: bool) {
        ui.horizontal(|ui| {
            ui.label("Recording:");
            let shown = self
                .source
                .as_ref()
                .map_or("(none chosen)".to_string(), |path| {
                    path.display().to_string()
                });
            ui.label(RichText::new(shown).strong());
            if ui
                .add_enabled(
                    !running && self.picker.is_none(),
                    egui::Button::new("Choose…"),
                )
                .clicked()
            {
                self.choose_file(ctx);
            }
        });
    }

    fn draw_start_or_cancel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, running: bool) {
        ui.horizontal(|ui| {
            if running {
                if ui.button("Cancel").clicked() {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                ui.spinner();
            } else if ui
                .add_enabled(self.can_start(), egui::Button::new("Start tuning"))
                .clicked()
            {
                self.start(ctx);
            }
        });
    }

    fn draw_log(&self, ui: &mut egui::Ui) {
        let shared_state = self.shared.lock().unwrap();
        if shared_state.lines.is_empty() {
            return;
        }
        egui::ScrollArea::vertical()
            .max_height(260.0)
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for line in &shared_state.lines {
                    ui.label(RichText::new(line).monospace().size(12.0));
                }
            });
    }

    fn draw_report(
        &mut self,
        ui: &mut egui::Ui,
        report: &tune::Report,
        pal: &Pal,
        saved_before: bool,
        out: &mut Outcome,
    ) -> bool {
        ui.separator();
        ui.label(format!("Disk: {:.2} GB/s sustained (first second {:.2} GB/s). With the best settings the reader does {:.2} GB/s on a {:.0} GB read{}.", report.sustained_gbps, report.burst_gbps, report.best_gbps, report.trial_bytes as f64 / 1e9, {
            // Only when it makes sense: a read that is partly a fast burst can beat the sustained figure.
            let share = report.best_gbps / report.sustained_gbps.max(1e-9);
            if share <= 1.05 { format!(" ({:.0}% of the disk)", 100.0 * share) } else { String::new() }
        }));
        if let Some(default_gbps) = report.default_gbps {
            ui.label(format!(
                "The built-in defaults do {default_gbps:.2} GB/s on the same read."
            ));
        }
        ui.label(
            RichText::new(format!(
                "Best: {} readers, {} MiB regions, {} regions open (about {} MiB of memory)",
                report.best.readers,
                report.best.region_mb,
                report.best.in_flight,
                report.best.memory_mb()
            ))
            .strong(),
        );
        for note in &report.notes {
            ui.colored_label(pal.warning(), note);
        }
        ui.weak("This tunes reading only. Parsing and unwinding threads and DWARF depend on the CPU work of a real load and are left alone.");
        if self.settled && saved_before {
            ui.weak("Saved.");
        } else if ui.button("Apply, save and close").on_hover_text("Saves these in the [reader] table of the config file (used the next time a file is opened) and closes this window").clicked() {
            out.apply = Some(report.best);
            self.settled = true;
            return true;
        }
        false
    }

    /// Shows the result of the run; returns whether "Apply, save and close" was clicked.
    fn draw_result(
        &mut self,
        ui: &mut egui::Ui,
        pal: &Pal,
        saved_before: bool,
        out: &mut Outcome,
    ) -> bool {
        let shared = self.shared.clone();
        let shared_state = shared.lock().unwrap();
        match &shared_state.result {
            Some(Ok(report)) => self.draw_report(ui, report, pal, saved_before, out),
            Some(Err(message)) => {
                ui.separator();
                ui.colored_label(
                    pal.suspect(),
                    if message == "cancelled" {
                        "Cancelled".to_string()
                    } else {
                        format!("Tuning failed: {message}")
                    },
                );
                false
            }
            None => false,
        }
    }

    /// The contents of the tuner window; returns whether the results were applied and saved.
    fn draw_window_contents(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        pal: &Pal,
        saved_before: bool,
        out: &mut Outcome,
    ) -> bool {
        self.draw_intro(ui);
        ui.add_space(6.0);
        let running = self.running();
        ui.add_enabled_ui(!running, |ui| {
            ui.checkbox(&mut self.synthetic, "Use a generated test file instead of a recording")
                .on_hover_text("Writes a file of realistic records to the folder below, tunes on it and deletes it afterwards. For when you have no big recording, or want to tune before recording one.");
        });
        if self.synthetic {
            self.draw_synthetic_choice(ui, ctx, running);
        } else {
            self.draw_recording_choice(ui, ctx, running);
        }
        self.draw_start_or_cancel(ui, ctx, running);
        ui.add_space(4.0);
        self.draw_log(ui);
        let applied = self.draw_result(ui, pal, saved_before, out);
        ui.allocate_space(vec2(ui.available_width(), 0.0));
        applied
    }

    /// The window's close button. Results not saved yet need an answer first.
    fn handle_close_request(&mut self, ctx: &egui::Context, viewport: egui::ViewportId) {
        if self.unsaved() {
            ctx.send_viewport_cmd_to(viewport, egui::ViewportCommand::CancelClose);
            self.confirm = Some(Ask::Close);
            ctx.request_repaint();
        } else {
            self.cancel.store(true, Ordering::Relaxed);
            self.open = false;
        }
    }

    /// Draw the window if it is open. Returns the settings when "Apply, save and close" is clicked.
    pub fn show(&mut self, ctx: &egui::Context) -> Outcome {
        let mut out = Outcome::default();
        self.poll_pickers();
        if !self.open && self.confirm.is_none() {
            return out;
        }
        let pal = Pal {
            dark: ctx.global_style().visuals.dark_mode,
        };
        let saved_before = self.settled;
        #[cfg(any(test, feature = "scripted"))]
        self.autostart_for_testing(ctx);
        let mut close_requested = false;
        let mut close_after_apply = false;
        let viewport = viewport_id();
        if self.open {
            // Its own window, so the loaded profile stays usable while tuning runs.
            let builder = egui::ViewportBuilder::default()
                .with_title("perf-viz: tune reader")
                .with_inner_size([620.0, 640.0]);
            ctx.show_viewport_immediate(viewport, builder, |viewport_ui, _class| {
                if viewport_ui.input(|input| input.viewport().close_requested()) {
                    close_requested = true;
                }
                egui::CentralPanel::default().show(viewport_ui, |ui| {
                    close_after_apply =
                        self.draw_window_contents(ui, ctx, &pal, saved_before, &mut out);
                });
                if matches!(self.confirm, Some(Ask::Close)) {
                    self.confirm_ui(viewport_ui.ctx(), &mut out);
                }
            });
        }
        if close_after_apply {
            self.open = false;
        }
        if close_requested {
            self.handle_close_request(ctx, viewport);
        }
        // Opening a file or quitting is asked in the main window.
        if matches!(self.confirm, Some(Ask::Open(_) | Ask::Quit)) {
            self.confirm_ui(ctx, &mut out);
        }
        out
    }

    /// The "save the tuning results?" question, as a modal in the window of `ctx`.
    fn confirm_ui(&mut self, ctx: &egui::Context, out: &mut Outcome) {
        if let Some(ask) = self.confirm.take() {
            let best = match &self.shared.lock().unwrap().result {
                Some(Ok(report)) => Some(report.best),
                _ => None,
            };
            let mut answer: Option<Answer> = None;
            egui::Modal::new(egui::Id::new("tuner_confirm")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading("Save the tuning results?");
                if let Some(best_settings) = best {
                    ui.label(format!(
                        "Best settings found: {} readers, {} MiB regions, {} regions open.",
                        best_settings.readers, best_settings.region_mb, best_settings.in_flight
                    ));
                }
                ui.label(match ask {
                    Ask::Open(_) => "Unsaved results are lost when another file is opened.",
                    _ => "Unsaved results are lost when you close this.",
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        answer = Some(Answer::Save);
                    }
                    if ui.button("Discard").clicked() {
                        answer = Some(Answer::Discard);
                    }
                    if ui
                        .button(if matches!(ask, Ask::Open(_)) {
                            "Don't open the file"
                        } else {
                            "Keep tuning window open"
                        })
                        .clicked()
                    {
                        answer = Some(Answer::Cancel);
                    }
                });
            });
            match answer {
                None => self.confirm = Some(ask),
                Some(Answer::Cancel) => {}
                Some(answer) => {
                    if matches!(answer, Answer::Save) {
                        out.apply = best;
                    }
                    self.settled = true;
                    match ask {
                        Ask::Close => self.open = false,
                        Ask::Quit => {
                            self.open = false;
                            out.quit = true;
                        }
                        Ask::Open(path) => out.open_file = Some(path),
                    }
                }
            }
        }
    }
}
