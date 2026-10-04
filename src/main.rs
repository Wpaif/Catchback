use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};

use catchback::config::{default_config_path, Config};
use catchback::ffmpeg::FfmpegExporter;
use catchback::format::{Container, Quality};
use catchback::gst::GstBackend;
use catchback::labels::{save_button_label, status};
use catchback::portal::PortalCapture;
use catchback::recorder::Recorder;
use catchback::session::{Mode, State};

const APP_ID: &str = "dev.catchback.Catchback";

type SharedRecorder = Arc<Mutex<Recorder<GstBackend, FfmpegExporter>>>;

struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    mode_group: adw::ToggleGroup,
    status_page: adw::StatusPage,
    primary: gtk::Button,
    save: gtk::Button,
    recorder: SharedRecorder,
    config_path: PathBuf,
    state: Cell<State>,
    started_at: Cell<Option<Instant>>,
    capture: RefCell<Option<PortalCapture>>,
    saving: Cell<bool>,
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

fn segment_dir() -> PathBuf {
    // Em disco (não em /tmp, que costuma ser tmpfs na RAM).
    let base = match std::env::var("XDG_CACHE_HOME") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(home()).join(".cache"),
    };
    base.join("catchback").join("segments")
}

fn now() -> chrono::NaiveDateTime {
    chrono::Local::now().naive_local()
}

impl App {
    fn mode(&self) -> Mode {
        match self.mode_group.active_name().as_deref() {
            Some("manual") => Mode::Manual,
            _ => Mode::Replay,
        }
    }

    fn config(&self) -> Config {
        self.recorder.lock().unwrap().config().clone()
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn toast_with_folder(self: &Rc<Self>, text: &str, file: PathBuf) {
        let toast = adw::Toast::builder().title(text).button_label("Abrir pasta").build();
        let app = self.clone();
        toast.connect_button_clicked(move |_| {
            gtk::FileLauncher::new(Some(&gio::File::for_path(&file)))
                .open_containing_folder(Some(&app.window), gio::Cancellable::NONE, |_| {});
        });
        self.toasts.add_toast(toast);
    }

    fn refresh(&self) {
        let state = self.state.get();
        let config = self.config();
        // Tempo real decorrido (não depende dos segmentos de 5 s concluídos).
        let progress = match state {
            State::Idle => Duration::ZERO,
            _ => self.started_at.get().map_or(Duration::ZERO, |t| t.elapsed()),
        };
        let st = status(state, progress, config.buffer_window());
        self.status_page.set_icon_name(Some(st.icon));
        self.status_page.set_title(&st.title);
        self.status_page.set_description(Some(&st.subtitle));

        let idle = state == State::Idle;
        self.mode_group.set_sensitive(idle);
        self.primary.set_sensitive(!self.saving.get());
        if idle {
            self.primary.set_label(match self.mode() {
                Mode::Replay => "Iniciar replay",
                Mode::Manual => "Iniciar gravação",
            });
            self.primary.remove_css_class("destructive-action");
            self.primary.add_css_class("suggested-action");
        } else {
            self.primary.set_label("Parar");
            self.primary.remove_css_class("suggested-action");
            self.primary.add_css_class("destructive-action");
        }
        self.save.set_visible(state == State::Buffering);
        self.save.set_sensitive(!self.saving.get());
        self.save.set_label(&if self.saving.get() {
            "Salvando…".to_string()
        } else {
            save_button_label(config.clip_span())
        });
    }

    /// Chamado a cada segundo: registra segmentos novos do buffer.
    fn tick(&self) {
        if let (State::Buffering, Ok(mut rec)) = (self.state.get(), self.recorder.try_lock())
            && let Err(e) = rec.poll()
        {
            eprintln!("erro ao atualizar o buffer: {e}");
        }
        self.refresh();
    }

    fn start(self: &Rc<Self>) {
        let mode = self.mode();
        self.primary.set_sensitive(false);
        let app = self.clone();
        glib::spawn_future_local(async move {
            match PortalCapture::request().await {
                Ok(capture) => {
                    let result = app.recorder.lock().unwrap().start(mode, capture.source, now());
                    match result {
                        Ok(()) => {
                            app.state.set(match mode {
                                Mode::Manual => State::Recording,
                                Mode::Replay => State::Buffering,
                            });
                            app.started_at.set(Some(Instant::now()));
                            *app.capture.borrow_mut() = Some(capture);
                        }
                        Err(e) => {
                            app.toast(&e.to_string());
                            capture.close().await;
                        }
                    }
                }
                Err(e) => app.toast(&e.to_string()),
            }
            app.primary.set_sensitive(true);
            app.refresh();
        });
    }

    fn stop(self: &Rc<Self>) {
        self.primary.set_sensitive(false);
        let recorder = self.recorder.clone();
        let app = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || recorder.lock().unwrap().stop()).await;
            let capture = app.capture.borrow_mut().take();
            if let Some(capture) = capture {
                capture.close().await;
            }
            app.state.set(State::Idle);
            match result {
                Ok(Ok(Some(file))) => app.toast_with_folder("Gravação salva", file),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => app.toast(&e.to_string()),
                Err(_) => app.toast("falha interna ao parar a captura"),
            }
            app.primary.set_sensitive(true);
            app.refresh();
        });
    }

    fn save_clip(self: &Rc<Self>) {
        if self.saving.replace(true) {
            return;
        }
        self.refresh();
        let recorder = self.recorder.clone();
        let app = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || {
                let mut rec = recorder.lock().unwrap();
                let span = rec.config().clip_span();
                rec.save_clip(span, now())
            })
            .await;
            app.saving.set(false);
            match result {
                Ok(Ok(file)) => app.toast_with_folder("Clip salvo", file),
                Ok(Err(e)) => app.toast(&e.to_string()),
                Err(_) => app.toast("falha interna ao salvar o clip"),
            }
            app.refresh();
        });
    }

    fn preferences(self: &Rc<Self>) {
        let config = self.config();
        let spin = |title: &str, value: u32, min: f64, max: f64, step: f64| {
            let row = adw::SpinRow::with_range(min, max, step);
            row.set_title(title);
            row.set_value(f64::from(value));
            row
        };
        let minutes = spin("Duração do buffer (minutos)", config.buffer_minutes, 1.0, 60.0, 1.0);
        let clip = spin("Duração do clip salvo (segundos)", config.clip_seconds, 5.0, 3600.0, 5.0);
        let fps = spin("Quadros por segundo", config.fps, 15.0, 120.0, 5.0);
        let combo = |title: &str, labels: Vec<&str>, selected: usize| {
            let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(&labels)).build();
            row.set_selected(selected as u32);
            row
        };
        let container = combo(
            "Formato do arquivo",
            Container::ALL.iter().map(|c| c.label()).collect(),
            Container::ALL.iter().position(|c| *c == config.container).unwrap_or(0),
        );
        let quality = combo(
            "Qualidade",
            Quality::ALL.iter().map(|q| q.label()).collect(),
            Quality::ALL.iter().position(|q| *q == config.quality).unwrap_or(0),
        );
        let folder = Rc::new(RefCell::new(config.output_dir.clone()));
        let folder_row = adw::ActionRow::builder()
            .title("Pasta dos clips")
            .subtitle(folder.borrow().to_string_lossy())
            .subtitle_selectable(true)
            .build();
        let choose = gtk::Button::builder().icon_name("folder-open-symbolic").valign(gtk::Align::Center).build();
        choose.add_css_class("flat");
        folder_row.add_suffix(&choose);
        {
            let (app, folder, row) = (self.clone(), folder.clone(), folder_row.clone());
            choose.connect_clicked(move |_| {
                let (folder, row) = (folder.clone(), row.clone());
                gtk::FileDialog::new().select_folder(Some(&app.window), gio::Cancellable::NONE, move |res| {
                    if let Some(path) = res.ok().and_then(|f| f.path()) {
                        row.set_subtitle(&path.to_string_lossy());
                        *folder.borrow_mut() = path;
                    }
                });
            });
        }

        let replay = adw::PreferencesGroup::builder()
            .title("Replay")
            .description("Vale a partir da próxima captura")
            .build();
        replay.add(&minutes);
        replay.add(&clip);
        let video = adw::PreferencesGroup::builder().title("Vídeo").build();
        video.add(&fps);
        video.add(&quality);
        video.add(&container);
        let output = adw::PreferencesGroup::builder().title("Saída").build();
        output.add(&folder_row);
        let page = adw::PreferencesPage::new();
        page.add(&replay);
        page.add(&video);
        page.add(&output);
        let dialog = adw::PreferencesDialog::builder().title("Preferências").build();
        dialog.add(&page);

        let app = self.clone();
        dialog.connect_closed(move |_| {
            let new = Config {
                buffer_minutes: minutes.value() as u32,
                clip_seconds: clip.value() as u32,
                fps: fps.value() as u32,
                container: Container::ALL.get(container.selected() as usize).copied().unwrap_or_default(),
                quality: Quality::ALL.get(quality.selected() as usize).copied().unwrap_or_default(),
                output_dir: folder.borrow().clone(),
            };
            if let Err(e) = new.validate().and_then(|()| new.save(&app.config_path)) {
                app.toast(&e.to_string());
                return;
            }
            app.recorder.lock().unwrap().set_config(new);
            app.refresh();
        });
        dialog.present(Some(&self.window));
    }
}

fn build_ui(application: &adw::Application) {
    let config_path = default_config_path(std::env::var("XDG_CONFIG_HOME").ok().as_deref(), &home());
    let (config, config_error) = match Config::load(&config_path) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e.to_string())),
    };
    let recorder = Recorder::new(GstBackend::default(), FfmpegExporter, config, segment_dir());

    let mode_group = adw::ToggleGroup::builder().halign(gtk::Align::Center).build();
    mode_group.add(adw::Toggle::builder().name("replay").label("Replay").build());
    mode_group.add(adw::Toggle::builder().name("manual").label("Gravação").build());
    mode_group.set_active_name(Some("replay"));

    let status_page = adw::StatusPage::builder().vexpand(true).build();
    let primary = gtk::Button::builder().halign(gtk::Align::Center).build();
    primary.add_css_class("pill");
    primary.add_css_class("suggested-action");
    let save = gtk::Button::builder().halign(gtk::Align::Center).visible(false).build();
    save.add_css_class("pill");

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(24)
        .build();
    content.append(&mode_group);
    content.append(&status_page);
    content.append(&primary);
    content.append(&save);

    let menu = gio::Menu::new();
    menu.append(Some("Preferências"), Some("win.preferences"));
    let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build();
    let header = adw::HeaderBar::new();
    header.pack_end(&menu_button);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&adw::Clamp::builder().maximum_size(480).child(&content).build()));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Catchback")
        .default_width(420)
        .default_height(520)
        .content(&toasts)
        .build();

    let app = Rc::new(App {
        window: window.clone(),
        toasts,
        mode_group: mode_group.clone(),
        status_page,
        primary: primary.clone(),
        save: save.clone(),
        recorder: Arc::new(Mutex::new(recorder)),
        config_path,
        state: Cell::new(State::Idle),
        started_at: Cell::new(None),
        capture: RefCell::new(None),
        saving: Cell::new(false),
    });

    {
        let app = app.clone();
        primary.connect_clicked(move |_| match app.state.get() {
            State::Idle => app.start(),
            _ => app.stop(),
        });
    }
    {
        let app = app.clone();
        save.connect_clicked(move |_| app.save_clip());
    }
    {
        let app = app.clone();
        mode_group.connect_active_name_notify(move |_| app.refresh());
    }
    {
        let preferences = gio::SimpleAction::new("preferences", None);
        let app = app.clone();
        preferences.connect_activate(move |_, _| app.preferences());
        window.add_action(&preferences);
        application.set_accels_for_action("win.preferences", &["<primary>comma"]);
    }
    {
        let app = app.clone();
        glib::timeout_add_seconds_local(1, move || {
            app.tick();
            glib::ControlFlow::Continue
        });
    }
    {
        // Ao fechar com captura ativa, finaliza os arquivos antes de sair.
        let app = app.clone();
        window.connect_close_request(move |_| {
            if app.state.get() != State::Idle {
                let _ = app.recorder.lock().unwrap().stop();
            }
            glib::Propagation::Proceed
        });
    }

    app.refresh();
    if let Some(e) = config_error {
        app.toast(&format!("Configuração inválida, usando o padrão ({e})"));
    }
    window.present();
}

fn main() -> glib::ExitCode {
    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_activate(build_ui);
    application.run()
}
