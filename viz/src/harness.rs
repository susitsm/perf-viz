//! Support for checking the GUI without a person: scripted pointer input and screenshots.
//!
//! Compiled only with the `scripted` feature (which `cargo test` turns on); without it [`Harness`]
//! is an empty type whose methods do nothing, so the app code that calls it needs no `cfg`, and the
//! screenshot encoder (the `image` crate) is not built.
//!
//! The scripted flags (`--click`, `--hover`, `--drag`, `--open`, `--pause`, `--screenshot`,
//! `--load-screenshot`) inject pointer input in window coordinates (points: screenshot pixels
//! divided by the scale factor) and save a screenshot afterwards.

#[cfg(not(any(test, feature = "scripted")))]
pub use disabled::Harness;
#[cfg(any(test, feature = "scripted"))]
pub use scripted::Harness;

#[cfg(not(any(test, feature = "scripted")))]
mod disabled {
    use std::path::PathBuf;

    use eframe::egui;

    use crate::cli::Cli;

    #[derive(Clone, Default)]
    pub struct Harness;

    impl Harness {
        pub fn from_cli(_cli: &Cli) -> Self {
            Harness
        }
        pub fn configure(&self, _ctx: &egui::Context) {}
        pub fn raw_input_hook(
            &mut self,
            _raw: &mut egui::RawInput,
            _screen_ready: bool,
            _frames: u32,
        ) {
        }
        pub fn take_open(&mut self) -> Option<PathBuf> {
            None
        }
        pub fn take_tune_window(&mut self) -> bool {
            false
        }
        pub fn finished(&self) -> bool {
            false
        }
        pub fn after_open(&mut self) -> Harness {
            Harness
        }
        pub fn poll_screen(&mut self, _ctx: &egui::Context, _frames: u32) {}
        pub fn poll_loading(&mut self, _ctx: &egui::Context, _elapsed_secs: f32) {}
    }
}

#[cfg(any(test, feature = "scripted"))]
mod scripted {
    use std::{
        collections::VecDeque,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    use eframe::egui::{self, Pos2, pos2};

    use crate::cli::{Cli, LoadScreenshot, ScriptStep};

    #[derive(Clone, Copy)]
    enum Step {
        Click(Pos2),
        Hover(Pos2),
        /// Press at the first point, move to the second, release.
        Drag(Pos2, Pos2),
        /// Do nothing for this many milliseconds (to let a background job finish).
        Pause(u64),
        /// Behave as if this file was dropped on the window; the index is into `Script::files`.
        Open(usize),
        /// Open the reader tuner window.
        TuneWindow,
    }

    /// A step in progress and how far along it is.
    #[derive(Clone, Copy)]
    struct Doing {
        step: Step,
        /// 0: the press (or first event) is next, and so on.
        stage: u8,
    }

    /// Injected pointer events, one step at a time.
    #[derive(Clone)]
    struct Script {
        files: Vec<PathBuf>,
        steps: VecDeque<Step>,
        current: Option<Doing>,
        wait_until: Instant,
        /// When the last step finished; the screenshot is taken a moment later (tooltip delay).
        finished: Option<Instant>,
    }

    #[derive(Clone, Default)]
    pub struct Harness {
        screenshot: Option<PathBuf>,
        /// Save a screenshot this many seconds into loading (to check the loading screen).
        load_screenshot: Option<LoadScreenshot>,
        script: Option<Script>,
        injected_open: Option<PathBuf>,
        tune_window: bool,
        requested: bool,
        /// The screenshot is saved and the app is quitting: nothing may ask questions now.
        finished: bool,
    }

    impl Harness {
        pub fn from_cli(cli: &Cli) -> Self {
            let mut files = Vec::new();
            let steps: VecDeque<Step> = cli
                .script
                .steps()
                .iter()
                .map(|step| match step {
                    ScriptStep::Click(point) => Step::Click(pos2(point.x, point.y)),
                    ScriptStep::Hover(point) => Step::Hover(pos2(point.x, point.y)),
                    ScriptStep::Drag(drag) => {
                        Step::Drag(pos2(drag.from.x, drag.from.y), pos2(drag.to.x, drag.to.y))
                    }
                    ScriptStep::Pause(secs) => Step::Pause((secs * 1000.0) as u64),
                    ScriptStep::TuneWindow => Step::TuneWindow,
                    ScriptStep::Open(path) => {
                        files.push(path.clone());
                        Step::Open(files.len() - 1)
                    }
                })
                .collect();
            Harness {
                screenshot: cli.script.screenshot.clone(),
                load_screenshot: cli.script.load_screenshot(),
                script: (!steps.is_empty()).then(|| Script {
                    files,
                    steps,
                    current: None,
                    wait_until: Instant::now(),
                    finished: None,
                }),
                injected_open: None,
                tune_window: false,
                requested: false,
                finished: false,
            }
        }

        /// With `PERF_VIZ_EMBED_VIEWPORTS` set, show extra windows (the tuner) inside the main one, so a
        /// screenshot of the main window includes them and scripted clicks can reach them.
        pub fn configure(&self, ctx: &egui::Context) {
            if std::env::var_os("PERF_VIZ_EMBED_VIEWPORTS").is_some() {
                ctx.set_embed_viewports(true);
            }
        }

        /// Inject the next scripted pointer event, once the screen has been drawn a few times.
        pub fn raw_input_hook(
            &mut self,
            raw: &mut egui::RawInput,
            screen_ready: bool,
            frames: u32,
        ) {
            if !screen_ready || frames < 5 {
                return;
            }
            let Some(sc) = &mut self.script else { return };
            if sc.finished.is_some() || Instant::now() < sc.wait_until {
                return;
            }
            let mods = egui::Modifiers::default();
            let pause = |ms| Instant::now() + Duration::from_millis(ms);
            match sc.current {
                None => match sc.steps.pop_front() {
                    None => sc.finished = Some(Instant::now()),
                    Some(Step::Pause(ms)) => sc.wait_until = pause(ms),
                    Some(Step::TuneWindow) => {
                        self.tune_window = true;
                        sc.wait_until = pause(300);
                    }
                    Some(Step::Open(i)) => {
                        self.injected_open = sc.files.get(i).cloned();
                        sc.wait_until = pause(300);
                    }
                    Some(step) => {
                        let (Step::Click(pos) | Step::Hover(pos) | Step::Drag(pos, _)) = step
                        else {
                            unreachable!()
                        };
                        raw.events.push(egui::Event::PointerMoved(pos));
                        // Let the hover register before pressing; a hover step just pauses.
                        sc.current =
                            (!matches!(step, Step::Hover(_))).then_some(Doing { step, stage: 0 });
                        sc.wait_until = pause(if sc.current.is_some() { 150 } else { 350 });
                    }
                },
                Some(Doing {
                    step: step @ Step::Click(pos),
                    stage,
                }) => {
                    raw.events.push(egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: stage == 0,
                        modifiers: mods,
                    });
                    sc.current = if stage == 0 {
                        Some(Doing { step, stage: 1 })
                    } else {
                        None
                    };
                    sc.wait_until = pause(if stage == 0 { 80 } else { 350 });
                }
                Some(Doing {
                    step: step @ Step::Drag(from, to),
                    stage,
                }) => {
                    let mid = pos2((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
                    raw.events.push(match stage {
                        0 => egui::Event::PointerButton {
                            pos: from,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: mods,
                        },
                        1 => egui::Event::PointerMoved(mid),
                        2 => egui::Event::PointerMoved(to),
                        _ => egui::Event::PointerButton {
                            pos: to,
                            button: egui::PointerButton::Primary,
                            pressed: false,
                            modifiers: mods,
                        },
                    });
                    sc.current = if stage >= 3 {
                        None
                    } else {
                        Some(Doing {
                            step,
                            stage: stage + 1,
                        })
                    };
                    sc.wait_until = pause(if stage >= 3 { 350 } else { 80 });
                }
                Some(_) => sc.current = None,
            }
        }

        /// A file the script asked to open, as if it was dropped on the window.
        pub fn take_open(&mut self) -> Option<PathBuf> {
            self.injected_open.take()
        }

        /// The script asked for the tuner window.
        pub fn take_tune_window(&mut self) -> bool {
            std::mem::take(&mut self.tune_window)
        }

        /// The screenshot has been saved and the app is closing.
        pub fn finished(&self) -> bool {
            self.finished
        }

        /// What carries over when another file is opened: the pending screenshot, not the script.
        pub fn after_open(&mut self) -> Harness {
            Harness {
                screenshot: self.screenshot.take(),
                ..Harness::default()
            }
        }

        /// On a settled screen (loaded, welcome, or the tuning question): once the script has run (and
        /// the tooltip delay passed), or after a few frames without a script, save the screenshot and quit.
        pub fn poll_screen(&mut self, ctx: &egui::Context, frames: u32) {
            let Some(path) = self.screenshot.clone() else {
                return;
            };
            let ready = match &self.script {
                None => frames == 8,
                Some(sc) => sc
                    .finished
                    .is_some_and(|finished_at| finished_at.elapsed() > Duration::from_millis(900)),
            };
            self.poll(ctx, &path, ready);
        }

        /// On the loading screen: save `--load-screenshot` once that many seconds have passed.
        pub fn poll_loading(&mut self, ctx: &egui::Context, elapsed_secs: f32) {
            let Some(shot) = self.load_screenshot.clone() else {
                return;
            };
            self.poll(ctx, &shot.path, elapsed_secs > shot.after_secs);
        }

        /// Request a screenshot when `ready`, and save it (then quit) when it arrives.
        fn poll(&mut self, ctx: &egui::Context, path: &Path, ready: bool) {
            if ready && !self.requested {
                self.requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            }
            let shot = ctx.input(|i| {
                i.events.iter().find_map(|event| {
                    if let egui::Event::Screenshot { image, .. } = event {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
            });
            if let Some(img) = shot {
                let [width, height] = img.size;
                let mut buffer = Vec::with_capacity(width * height * 4);
                for px in &img.pixels {
                    buffer.extend_from_slice(&px.to_array());
                }
                match image::save_buffer(
                    path,
                    &buffer,
                    width as u32,
                    height as u32,
                    image::ColorType::Rgba8,
                ) {
                    Ok(()) => {
                        eprintln!("screenshot saved to {} ({width}x{height})", path.display())
                    }
                    Err(error) => eprintln!("screenshot failed: {error}"),
                }
                self.finished = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}
