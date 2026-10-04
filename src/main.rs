use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gio, glib};

use catchback::audio::{self, is_voice_chat, AudioMode, MusicChoice};
use catchback::config::{default_config_path, Config};
use catchback::ffmpeg::FfmpegExporter;
use catchback::format::{Container, Quality};
use catchback::gst::{record_mic_sample, GstBackend};
use catchback::labels::{ask_music_hint, audio_overview, music_notice, save_button_label, status};
use catchback::mic::{self, MicVerdict};
use catchback::music::{self, MusicLog};
use catchback::portal::PortalCapture;
use catchback::recorder::Recorder;
use catchback::session::{Mode, State};

const APP_ID: &str = "dev.catchback.Catchback";
const SOURCE_SYSTEM: &str = "Tudo que toca no computador";
const SOURCE_MIC_DEFAULT: &str = "Padrão do sistema";
/// Quanto tempo guardar o histórico de música numa gravação manual (sem janela).
const MANUAL_LOG_KEEP: Duration = Duration::from_secs(24 * 3600);

type SharedRecorder = Arc<Mutex<Recorder<GstBackend, FfmpegExporter>>>;

/// Controles de áudio do painel principal: uma linha por fonte.
struct Mixer {
    group: adw::PreferencesGroup,
    expander: adw::ExpanderRow,
    game_switch: adw::SwitchRow,
    /// Aplicativo do jogo (o índice 0 da lista é "tudo que toca no computador").
    game_app: adw::ComboRow,
    game_row: adw::ActionRow,
    game_volume: gtk::Scale,
    mic_switch: adw::SwitchRow,
    /// Escolha do microfone (o índice 0 é o padrão do sistema).
    mic_device: adw::ComboRow,
    mic_row: adw::ActionRow,
    mic_volume: gtk::Scale,
    /// Teste de nível do microfone: ícone de resultado e botão.
    mic_test: adw::ActionRow,
    mic_test_icon: gtk::Image,
    mic_test_button: gtk::Button,
    call_switch: adw::SwitchRow,
    call_row: adw::ActionRow,
    call_volume: gtk::Scale,
    ask: adw::SwitchRow,
    /// "O que entra no clip": grava / você decide / fica de fora.
    overview: adw::ActionRow,
    /// Aviso compacto de música tocando (escondido quando não há o que avisar).
    notice: adw::ActionRow,
    notice_outcome: gtk::Label,
    /// Nomes dos apps na lista de jogo (o índice 0 da lista é "sistema").
    apps: RefCell<Vec<String>>,
    /// `node.name` dos microfones na lista (o índice 0 da lista é o padrão).
    mics: RefCell<Vec<String>>,
    can_list_mics: Cell<bool>,
    /// Há `pactl` ou `pw-dump` para listar os aplicativos.
    can_list: Cell<bool>,
    /// Evita que atualizar os widgets por código dispare `mixer_changed`.
    updating: Cell<bool>,
}

struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    mode_group: adw::ToggleGroup,
    status_page: adw::StatusPage,
    mixer: Mixer,
    primary: gtk::Button,
    save: gtk::Button,
    recorder: SharedRecorder,
    config_path: PathBuf,
    /// Cópia da configuração do lado da interface: ler isto nunca espera o
    /// gravador (que fica trancado enquanto um clip é exportado).
    config: RefCell<Config>,
    save_pending: Cell<bool>,
    state: Cell<State>,
    started_at: Cell<Option<Instant>>,
    capture: RefCell<Option<PortalCapture>>,
    saving: Cell<bool>,
    /// A captura atual guarda jogo/sistema/mic em faixas separadas.
    separate_tracks: Cell<bool>,
    music_log: RefCell<MusicLog>,
    detected: RefCell<Vec<String>>,
    /// Apps de call (Discord...) abertos agora.
    voice_apps: RefCell<Vec<String>>,
    detecting: Cell<bool>,
    mic_testing: Cell<bool>,
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

/// Itens da lista de um `ComboRow` sem o corte que o padrão aplica (~20 caracteres):
/// textos longos quebram em mais de uma linha, e o item escolhido leva um ✓.
fn wrapping_factory(row: &adw::ComboRow) -> gtk::SignalListItemFactory {
    use std::collections::HashMap;
    let factory = gtk::SignalListItemFactory::new();
    // Um observador do `selected` por item visível, para atualizar o ✓ ao vivo.
    let observers: Rc<RefCell<HashMap<gtk::ListItem, glib::SignalHandlerId>>> = Rc::default();

    factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            let label = gtk::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(40)
                .build();
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            let content = gtk::Box::builder().spacing(12).build();
            content.append(&label);
            content.append(&check);
            item.set_child(Some(&content));
        }
    });
    factory.connect_bind({
        let (row, observers) = (row.downgrade(), observers.clone());
        move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            let content = item.child().and_downcast::<gtk::Box>();
            let label = content.as_ref().and_then(|c| c.first_child()).and_downcast::<gtk::Label>();
            let check = content.as_ref().and_then(|c| c.last_child());
            let (Some(label), Some(check), Some(text), Some(row)) =
                (label, check, item.item().and_downcast::<gtk::StringObject>(), row.upgrade())
            else {
                return;
            };
            label.set_label(&text.string());
            let position = item.position();
            check.set_opacity(if row.selected() == position { 1.0 } else { 0.0 });
            let id = row.connect_selected_notify(move |r| check.set_opacity(if r.selected() == position { 1.0 } else { 0.0 }));
            observers.borrow_mut().insert(item.clone(), id);
        }
    });
    factory.connect_unbind({
        let (row, observers) = (row.downgrade(), observers);
        move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            if let (Some(id), Some(row)) = (observers.borrow_mut().remove(item), row.upgrade()) {
                row.disconnect(id);
            }
        }
    });
    factory
}

fn combo(title: &str, labels: &[&str], selected: usize) -> adw::ComboRow {
    let row = adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(labels))
        // O valor escolhido aparece como subtítulo, com a largura toda da linha.
        .use_subtitle(true)
        .build();
    row.set_list_factory(Some(&wrapping_factory(&row)));
    row.set_selected(selected as u32);
    row
}

fn volume_row(title: &str, subtitle: &str, value: u32) -> (adw::ActionRow, gtk::Scale) {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 200.0, 5.0);
    scale.set_value(f64::from(value));
    scale.set_width_request(190);
    scale.set_valign(gtk::Align::Center);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_format_value_func(|_, v| format!("{v:.0}%"));
    scale.add_mark(100.0, gtk::PositionType::Bottom, None);
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    row.add_suffix(&scale);
    (row, scale)
}

impl Mixer {
    fn new(config: &Config) -> Self {
        let game_switch = adw::SwitchRow::builder()
            .title("Som do jogo")
            .subtitle("Escolha o aplicativo abaixo")
            .active(config.audio.has_game())
            .build();
        let game_app = combo("Aplicativo do jogo", &[SOURCE_SYSTEM], 0);
        let (game_row, game_volume) = volume_row("Volume do jogo", "Só na gravação", config.game_volume);
        let mic_switch = adw::SwitchRow::builder()
            .title("Seu microfone")
            .subtitle("A sua voz")
            .active(config.audio.has_mic())
            .build();
        let mic_device = combo("Dispositivo do microfone", &[SOURCE_MIC_DEFAULT], 0);
        let (mic_row, mic_volume) = volume_row("Volume do microfone", "Só na gravação", config.mic_volume);
        let mic_test_icon = gtk::Image::from_icon_name("audio-input-microphone-symbolic");
        let mic_test_button = gtk::Button::builder().label("Testar").valign(gtk::Align::Center).build();
        let mic_test = adw::ActionRow::builder()
            .title("Testar microfone")
            .subtitle("Grava 2 segundos e mede o nível")
            .build();
        mic_test.add_prefix(&mic_test_icon);
        mic_test.add_suffix(&mic_test_button);
        let call_switch = adw::SwitchRow::builder()
            .title("Call dos amigos")
            .subtitle("Discord e outros apps de voz")
            .active(config.record_call)
            .build();
        let (call_row, call_volume) = volume_row("Volume da call", "Só na gravação", config.call_volume);
        let ask = adw::SwitchRow::builder()
            .title("Perguntar sobre a música ao salvar")
            .subtitle(ask_music_hint(config))
            .active(config.ask_music)
            .build();
        let overview = adw::ActionRow::builder().title("O que entra no clip").activatable(false).build();
        overview.add_prefix(&gtk::Image::from_icon_name("dialog-information-symbolic"));
        overview.set_subtitle_lines(0);

        // O resumo logo acima já diz o que entra no clip; aqui só explica o que há dentro.
        let expander = adw::ExpanderRow::builder()
            .title("Ajustar o áudio")
            .subtitle("Fontes, volumes e microfone")
            .build();
        for row in [
            game_switch.upcast_ref::<gtk::Widget>(),
            game_app.upcast_ref(),
            game_row.upcast_ref(),
            mic_switch.upcast_ref(),
            mic_device.upcast_ref(),
            mic_row.upcast_ref(),
            mic_test.upcast_ref(),
            call_switch.upcast_ref(),
            call_row.upcast_ref(),
            ask.upcast_ref(),
        ] {
            expander.add_row(row);
        }
        let notice = adw::ActionRow::builder().subtitle_lines(1).visible(false).build();
        let notice_outcome = gtk::Label::builder().valign(gtk::Align::Center).build();
        notice_outcome.add_css_class("caption");
        notice.add_prefix(&gtk::Image::from_icon_name("audio-x-generic-symbolic"));
        notice.add_suffix(&notice_outcome);
        let group = adw::PreferencesGroup::new();
        // O resumo vem primeiro e fora do painel recolhível: está sempre visível, com os
        // controles abertos ou fechados.
        group.add(&overview);
        group.add(&expander);
        group.add(&notice);
        Self {
            group,
            expander,
            game_switch,
            game_app,
            game_row,
            game_volume,
            mic_switch,
            mic_device,
            mic_row,
            mic_volume,
            mic_test,
            mic_test_icon,
            mic_test_button,
            call_switch,
            call_row,
            call_volume,
            ask,
            overview,
            notice,
            notice_outcome,
            apps: RefCell::new(Vec::new()),
            mics: RefCell::new(Vec::new()),
            can_list_mics: Cell::new(true),
            can_list: Cell::new(true),
            updating: Cell::new(false),
        }
    }

    /// Recarrega a lista de apps tocando (mantém o escolhido, mesmo fechado).
    fn reload_apps(&self, config: &Config) {
        let listing = audio::list_apps();
        let can_list = listing.is_ok();
        // Apps de call não são o jogo: ficam fora da lista (têm a sua própria chave).
        let mut names: Vec<String> =
            listing.unwrap_or_default().into_iter().map(|a| a.name).filter(|n| !is_voice_chat(n)).collect();
        let mut labels = names.clone();
        if let Some(saved) = config.audio_app.as_ref().filter(|a| !names.contains(a)) {
            names.push(saved.clone());
            labels.push(format!("{saved} (fechado)"));
        }
        let mut all = vec![SOURCE_SYSTEM];
        all.extend(labels.iter().map(String::as_str));

        self.updating.set(true);
        self.game_app.set_model(Some(&gtk::StringList::new(&all)));
        let selected = config.audio_app.as_ref().and_then(|a| names.iter().position(|n| n == a));
        self.game_app.set_selected(selected.map_or(0, |i| i as u32 + 1));
        self.can_list.set(can_list);
        // Com `use_subtitle` o valor escolhido ocupa o subtítulo; sem como listar
        // apps, o subtítulo passa a explicar o que instalar.
        self.game_app.set_use_subtitle(can_list);
        if !can_list {
            self.game_app.set_subtitle("Instale o pactl (libpulse) ou o pw-dump (pipewire) para escolher um aplicativo");
        }
        *self.apps.borrow_mut() = names;
        self.updating.set(false);
    }

    /// Recarrega a lista de microfones (mantém o escolhido, mesmo desconectado).
    fn reload_mics(&self, config: &Config) {
        let listing = mic::list_mics();
        let can_list = listing.is_ok();
        let devices = listing.unwrap_or_default();
        let mut names: Vec<String> = devices.iter().map(|d| d.name.clone()).collect();
        let mut labels: Vec<String> = devices.iter().map(|d| d.description.clone()).collect();
        if let Some(saved) = config.mic_source.as_ref().filter(|m| !names.contains(m)) {
            names.push(saved.clone());
            labels.push(format!("{saved} (indisponível)"));
        }
        let mut all = vec![SOURCE_MIC_DEFAULT];
        all.extend(labels.iter().map(String::as_str));

        self.updating.set(true);
        self.mic_device.set_model(Some(&gtk::StringList::new(&all)));
        let selected = config.mic_source.as_ref().and_then(|m| names.iter().position(|n| n == m));
        self.mic_device.set_selected(selected.map_or(0, |i| i as u32 + 1));
        self.can_list_mics.set(can_list);
        self.mic_device.set_use_subtitle(can_list);
        if !can_list {
            self.mic_device.set_subtitle("Instale o pactl (libpulse) ou o pw-dump (pipewire) para escolher o microfone");
        }
        *self.mics.borrow_mut() = names;
        self.updating.set(false);
    }

    /// Lê os widgets e devolve a configuração correspondente.
    fn read_into(&self, mut config: Config) -> Config {
        config.audio = AudioMode::from_flags(self.game_switch.is_active(), self.mic_switch.is_active());
        config.audio_app = (self.game_app.selected() as usize)
            .checked_sub(1)
            .and_then(|i| self.apps.borrow().get(i).cloned());
        config.mic_source = (self.mic_device.selected() as usize)
            .checked_sub(1)
            .and_then(|i| self.mics.borrow().get(i).cloned());
        config.record_call = self.call_switch.is_active();
        config.game_volume = self.game_volume.value() as u32;
        config.mic_volume = self.mic_volume.value() as u32;
        config.call_volume = self.call_volume.value() as u32;
        config.ask_music = self.ask.is_active();
        config
    }
}

impl App {
    fn mode(&self) -> Mode {
        match self.mode_group.active_name().as_deref() {
            Some("manual") => Mode::Manual,
            _ => Mode::Replay,
        }
    }

    fn config(&self) -> Config {
        self.config.borrow().clone()
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

    /// Guarda a nova configuração; grava o arquivo logo depois (sem escrever a
    /// cada passo de um controle deslizante).
    fn update_config(self: &Rc<Self>, config: Config) {
        if let Err(e) = config.validate() {
            self.toast(&e.to_string());
            return;
        }
        *self.config.borrow_mut() = config;
        if !self.save_pending.replace(true) {
            let app = self.clone();
            glib::timeout_add_local_once(Duration::from_millis(500), move || {
                app.save_pending.set(false);
                if let Err(e) = app.config.borrow().save(&app.config_path) {
                    app.toast(&e.to_string());
                }
            });
        }
        self.refresh();
    }

    fn mixer_changed(self: &Rc<Self>) {
        if self.mixer.updating.get() {
            return;
        }
        self.update_config(self.mixer.read_into(self.config()));
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

        self.refresh_mixer(&config, idle);
    }

    fn refresh_mixer(&self, config: &Config, idle: bool) {
        let mixer = &self.mixer;
        let system = config.audio.has_game() && config.audio_app.is_none();
        let voice = self.voice_apps.borrow();

        // O resumo diz, em palavras, o que entra no clip e o que fica de fora.
        let overview = audio_overview(config, !voice.is_empty());
        let mut lines = vec![format!("Grava: {}", overview.records)];
        if let Some(decides) = overview.decides {
            lines.push(format!("Você decide ao salvar: {decides}"));
        }
        if !overview.leaves_out.is_empty() {
            lines.push(format!("Fica de fora: {}", overview.leaves_out));
        }
        if let Some(warning) = overview.warning {
            lines.push(format!("Atenção: {warning}"));
        }
        mixer.overview.set_subtitle(&lines.join("\n"));

        // Escolhas só entre capturas. O volume também, a não ser que as faixas
        // estejam separadas (aí o ganho só é aplicado ao salvar).
        let volume_live = idle || self.separate_tracks.get();
        for switch in [&mixer.game_switch, &mixer.mic_switch, &mixer.call_switch, &mixer.ask] {
            switch.set_sensitive(idle);
        }
        mixer.game_app.set_sensitive(idle && mixer.can_list.get());
        mixer.mic_device.set_sensitive(idle && mixer.can_list_mics.get());
        mixer.mic_test.set_sensitive(idle && !self.mic_testing.get());
        let apply = if volume_live { "Só na gravação" } else { "Vale na próxima captura" };
        for row in [&mixer.game_row, &mixer.mic_row, &mixer.call_row] {
            row.set_subtitle(apply);
            row.set_sensitive(volume_live);
        }

        // Cada fonte só mostra os ajustes quando está ligada.
        for row in [mixer.game_app.upcast_ref::<gtk::Widget>(), mixer.game_row.upcast_ref()] {
            row.set_visible(config.audio.has_game());
        }
        for row in [mixer.mic_device.upcast_ref::<gtk::Widget>(), mixer.mic_row.upcast_ref(), mixer.mic_test.upcast_ref()] {
            row.set_visible(config.audio.has_mic());
        }
        // No áudio do sistema inteiro a call já vem junto: não há o que escolher.
        mixer.call_switch.set_visible(!system);
        mixer.call_row.set_visible(!system && config.record_call);
        mixer.call_switch.set_subtitle(&if voice.is_empty() {
            "Nenhum app de call aberto agora (Discord, etc.)".to_string()
        } else {
            format!("Detectado: {}", voice.join(", "))
        });
        mixer.ask.set_visible(config.audio.has_game() && config.audio_app.is_some());
        mixer.ask.set_subtitle(&ask_music_hint(config));

        let detected = self.detected.borrow();
        match music_notice(config.audio, config.audio_app.is_some(), config.ask_music, &detected) {
            Some(n) => {
                mixer.notice.set_title(n.title);
                mixer.notice.set_subtitle(&n.detail);
                // O detalhe pode ser enorme (título de uma live): corta em uma linha
                // e deixa o texto inteiro na dica.
                mixer.notice.set_tooltip_text(Some(&detected.join("\n")));
                mixer.notice_outcome.set_label(n.outcome);
                if n.warn {
                    mixer.notice_outcome.remove_css_class("dim-label");
                    mixer.notice_outcome.add_css_class("warning");
                } else {
                    mixer.notice_outcome.remove_css_class("warning");
                    mixer.notice_outcome.add_css_class("dim-label");
                }
                mixer.notice.set_visible(true);
            }
            None => mixer.notice.set_visible(false),
        }
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

    /// Chamado a cada 2 s: vê o que está tocando fora do jogo (sem travar a janela).
    fn detect_music(self: &Rc<Self>) {
        if self.detecting.replace(true) {
            return;
        }
        let game = self.config().audio_app;
        let app = self.clone();
        glib::spawn_future_local(async move {
            let (found, voice) = gio::spawn_blocking(move || {
                let players = music::read_players();
                let apps = audio::list_apps().unwrap_or_default();
                let voice: Vec<String> = apps.iter().filter(|a| is_voice_chat(&a.name)).map(|a| a.name.clone()).collect();
                (music::detect(&players, &apps, game.as_deref()), voice)
            })
            .await
            .unwrap_or_default();
            app.detecting.set(false);
            if app.state.get() != State::Idle
                && let Some(started) = app.started_at.get()
            {
                app.music_log.borrow_mut().record(started.elapsed(), found.clone());
            }
            *app.detected.borrow_mut() = found;
            *app.voice_apps.borrow_mut() = voice;
            app.refresh();
        });
    }

    /// Grava 2 s do microfone escolhido e mostra se o nível está bom.
    fn test_mic(self: &Rc<Self>) {
        if self.mic_testing.replace(true) {
            return;
        }
        self.mixer.mic_test_button.set_label("Testando…");
        self.refresh();
        let source = self.config().mic_source;
        let app = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || record_mic_sample(source.as_deref(), 2.0)).await;
            app.mic_testing.set(false);
            let (icon, text) = match result {
                Ok(Ok(samples)) => {
                    let levels = mic::levels(&samples);
                    let verdict = mic::verdict(&levels);
                    let icon = match verdict {
                        MicVerdict::Ok => "emblem-ok-symbolic",
                        MicVerdict::Silent => "dialog-question-symbolic",
                        MicVerdict::Saturated => "dialog-warning-symbolic",
                    };
                    let detail = format!("pico {:.0}%, {:.0}% no limite", levels.peak * 100.0, levels.clipped * 100.0);
                    (icon, format!("{} ({detail})", mic::verdict_message(verdict)))
                }
                Ok(Err(e)) => ("dialog-error-symbolic", format!("Não foi possível testar: {e}")),
                Err(_) => ("dialog-error-symbolic", "Falha interna ao testar o microfone".to_string()),
            };
            app.mixer.mic_test_icon.set_icon_name(Some(icon));
            app.mixer.mic_test.set_subtitle(&text);
            app.mixer.mic_test_button.set_label("Testar");
            app.refresh();
        });
    }

    /// Ao iniciar uma captura com microfone, avisa (sem atrasar) se ele está saturado.
    fn warn_if_mic_saturated(self: &Rc<Self>) {
        let source = self.config().mic_source;
        let app = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || record_mic_sample(source.as_deref(), 1.0)).await;
            if let Ok(Ok(samples)) = result
                && mic::verdict(&mic::levels(&samples)) == MicVerdict::Saturated
                && app.state.get() != State::Idle
            {
                app.toast(mic::verdict_message(MicVerdict::Saturated));
            }
        });
    }

    fn start(self: &Rc<Self>) {
        let mode = self.mode();
        let config = self.config();
        let audio_plan = match audio::plan(&config, &audio::list_apps()) {
            Ok(plan) => plan,
            Err(e) => {
                self.toast(&e.to_string());
                return;
            }
        };
        self.primary.set_sensitive(false);
        let app = self.clone();
        glib::spawn_future_local(async move {
            match PortalCapture::request().await {
                Ok(capture) => {
                    let result = {
                        let mut rec = app.recorder.lock().unwrap();
                        rec.set_config(config.clone());
                        rec.start(mode, capture.source, &audio_plan, now())
                    };
                    match result {
                        Ok(()) => {
                            app.state.set(match mode {
                                Mode::Manual => State::Recording,
                                Mode::Replay => State::Buffering,
                            });
                            app.started_at.set(Some(Instant::now()));
                            app.separate_tracks.set(audio_plan.separate_tracks);
                            let keep = if mode == Mode::Replay { config.buffer_window() } else { MANUAL_LOG_KEEP };
                            let mut log = MusicLog::new(keep);
                            log.record(Duration::ZERO, app.detected.borrow().clone());
                            *app.music_log.borrow_mut() = log;
                            *app.capture.borrow_mut() = Some(capture);
                            if audio_plan.mic {
                                app.warn_if_mic_saturated();
                            }
                            // Pediu a call, mas não há app de voz aberto: ela não será gravada.
                            if let Some(warning) = audio_overview(&config, audio_plan.call.is_some()).warning {
                                app.toast(warning);
                            }
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

    /// Pergunta o que fazer com a música. `None` = o usuário cancelou.
    async fn ask_music(&self, heard: &[String], allow_cancel: bool) -> Option<MusicChoice> {
        let list: String = heard.iter().map(|s| format!("• {s}\n")).collect();
        let dialog = adw::AlertDialog::new(
            Some("Música detectada neste trecho"),
            Some(&format!(
                "Estava tocando:\n{list}\n«Manter» inclui todo o áudio do sistema (jogo, música e a call). \
                 «Sem a música» grava só o jogo e o microfone."
            )),
        );
        dialog.add_response("drop", "Sem a música");
        dialog.add_response("keep", "Manter a música");
        dialog.set_response_appearance("drop", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("drop"));
        if allow_cancel {
            dialog.add_response("cancel", "Cancelar");
            dialog.set_close_response("cancel");
        } else {
            dialog.set_close_response("drop");
        }
        match dialog.choose_future(Some(&self.window)).await.as_str() {
            "keep" => Some(MusicChoice::Keep),
            "drop" => Some(MusicChoice::Drop),
            _ => None,
        }
    }

    fn stop(self: &Rc<Self>) {
        self.primary.set_sensitive(false);
        let separate = self.separate_tracks.get();
        let was_recording = self.state.get() == State::Recording;
        let heard = match self.started_at.get() {
            Some(started) if was_recording && separate => {
                self.music_log.borrow().heard_between(Duration::ZERO, started.elapsed())
            }
            _ => Vec::new(),
        };
        let recorder = self.recorder.clone();
        let app = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || recorder.lock().unwrap().stop()).await;
            let capture = app.capture.borrow_mut().take();
            if let Some(capture) = capture {
                capture.close().await;
            }
            app.state.set(State::Idle);
            app.separate_tracks.set(false);
            match result {
                Ok(Ok(Some(file))) => app.finish_recording(file, separate, heard).await,
                Ok(Ok(None)) => {}
                Ok(Err(e)) => app.toast(&e.to_string()),
                Err(_) => app.toast("falha interna ao parar a captura"),
            }
            app.primary.set_sensitive(true);
            app.refresh();
        });
    }

    /// Gravação manual pronta: se as faixas estão separadas, aplica a escolha da música.
    async fn finish_recording(self: &Rc<Self>, file: PathBuf, separate: bool, heard: Vec<String>) {
        if separate {
            let choice = if heard.is_empty() {
                MusicChoice::Drop
            } else {
                self.ask_music(&heard, false).await.unwrap_or(MusicChoice::Drop)
            };
            let (recorder, path) = (self.recorder.clone(), file.clone());
            let mixed = gio::spawn_blocking(move || recorder.lock().unwrap().mixdown_recording(&path, choice)).await;
            match mixed {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    self.toast(&format!("Gravação salva, mas não foi possível mixar o áudio: {e}"));
                    return;
                }
                Err(_) => {
                    self.toast("falha interna ao mixar o áudio");
                    return;
                }
            }
        }
        self.toast_with_folder("Gravação salva", file);
    }

    fn save_clip(self: &Rc<Self>) {
        if self.saving.get() {
            return;
        }
        let span = self.config().clip_span();
        let heard = match self.started_at.get() {
            Some(started) if self.separate_tracks.get() => {
                let to = started.elapsed();
                self.music_log.borrow().heard_between(to.saturating_sub(span), to)
            }
            _ => Vec::new(),
        };
        let app = self.clone();
        glib::spawn_future_local(async move {
            let choice = if heard.is_empty() {
                MusicChoice::Drop
            } else {
                match app.ask_music(&heard, true).await {
                    Some(choice) => choice,
                    None => return,
                }
            };
            app.save_with(choice).await;
        });
    }

    async fn save_with(self: &Rc<Self>, music: MusicChoice) {
        self.saving.set(true);
        self.refresh();
        let (recorder, config) = (self.recorder.clone(), self.config());
        let result = gio::spawn_blocking(move || {
            let mut rec = recorder.lock().unwrap();
            rec.set_config(config);
            let span = rec.config().clip_span();
            rec.save_clip(span, now(), music)
        })
        .await;
        self.saving.set(false);
        match result {
            Ok(Ok(file)) => self.toast_with_folder("Clip salvo", file),
            Ok(Err(e)) => self.toast(&e.to_string()),
            Err(_) => self.toast("falha interna ao salvar o clip"),
        }
        self.refresh();
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
        let container = combo(
            "Formato do arquivo",
            &Container::ALL.map(|c| c.label()),
            Container::ALL.iter().position(|c| *c == config.container).unwrap_or(0),
        );
        let quality = combo(
            "Qualidade",
            &Quality::ALL.map(|q| q.label()),
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
            // Parte do mixer (áudio) é editada na janela principal: preserva.
            let new = Config {
                buffer_minutes: minutes.value() as u32,
                clip_seconds: clip.value() as u32,
                fps: fps.value() as u32,
                container: Container::ALL.get(container.selected() as usize).copied().unwrap_or_default(),
                quality: Quality::ALL.get(quality.selected() as usize).copied().unwrap_or_default(),
                output_dir: folder.borrow().clone(),
                ..app.config()
            };
            app.update_config(new);
        });
        dialog.present(Some(&self.window));
    }
}


/// Modo de depuração: `CATCHBACK_SNAPSHOT_DIR=/tmp/x cargo run` tira capturas da
/// própria janela (inclusive do menu de áudio aberto) e fecha o app. Serve para
/// conferir a interface sem depender de captura de tela do sistema.
mod debug_snapshot {
    use super::*;

    type Step = Box<dyn Fn(&Rc<App>)>;

    fn save(widget: &impl IsA<gtk::Widget>, path: &std::path::Path) -> bool {
        let (w, h) = (widget.width(), widget.height());
        let Some(renderer) = widget.native().and_then(|n| n.renderer()) else {
            eprintln!("{}: sem renderer", path.display());
            return false;
        };
        if w == 0 || h == 0 {
            eprintln!("{}: tamanho {w}x{h}", path.display());
            return false;
        }
        let snapshot = gtk::Snapshot::new();
        gtk::WidgetPaintable::new(Some(widget)).snapshot(&snapshot, f64::from(w), f64::from(h));
        let Some(node) = snapshot.to_node() else {
            eprintln!("{}: nó vazio", path.display());
            return false;
        };
        let ok = renderer.render_texture(node, None).save_to_png(path).is_ok();
        eprintln!("{}: {}", path.display(), if ok { "ok" } else { "falhou ao gravar" });
        ok
    }

    fn find_popover(widget: &gtk::Widget) -> Option<gtk::Popover> {
        if let Some(p) = widget.downcast_ref::<gtk::Popover>()
            && p.is_visible()
        {
            return Some(p.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            if let Some(p) = find_popover(&c) {
                return Some(p);
            }
            child = c.next_sibling();
        }
        None
    }

    pub fn run(app: &Rc<App>, dir: PathBuf) {
        let _ = std::fs::create_dir_all(&dir);
        if let Some(display) = gtk::gdk::Display::default() {
            let found = gtk::IconTheme::for_display(&display).has_icon(APP_ID);
            eprintln!("ícone {APP_ID} encontrado pelo tema: {found}");
        }
        let step = |ms: u64, f: Step| {
            let app = app.clone();
            glib::timeout_add_local_once(Duration::from_millis(ms), move || f(&app));
        };
        let shot = |ms: u64, name: &'static str| {
            let dir = dir.clone();
            step(ms, Box::new(move |a| {
                save(&a.window, &dir.join(name));
            }));
        };
        let popover_shot = |ms: u64, name: &'static str| {
            let dir = dir.clone();
            step(ms, Box::new(move |a| match find_popover(a.window.upcast_ref()) {
                Some(p) => {
                    save(&p, &dir.join(name));
                }
                None => eprintln!("{name}: popover não encontrado"),
            }));
        };

        shot(1200, "1-fechado.png");
        step(1500, Box::new(|a| a.mixer.expander.set_expanded(true)));
        // Cenário real: jogo + mic + call, lives enormes tocando no navegador.
        step(2000, Box::new(|a| {
            a.mixer.game_switch.set_active(true);
            a.mixer.mic_switch.set_active(true);
            a.mixer.call_switch.set_active(true);
            *a.voice_apps.borrow_mut() = vec!["Discord".into()];
            *a.detected.borrow_mut() = vec![
                "Mozilla firefox — artilh3iro – STREAM DROPS ON (PXG)(STEEL) SORTEIO DIA 06/10 DE 40 VIPS !TICKET MAX".into(),
                "Spotify — Banda Qualquer – Uma música com um nome bem comprido".into(),
            ];
            a.refresh();
        }));
        shot(2600, "2-sistema-com-aviso.png");
        // Com um aplicativo do jogo escolhido: aviso discreto e resumo claro.
        step(3000, Box::new(|a| {
            a.mixer.game_app.set_selected(a.mixer.apps.borrow().len().min(1) as u32);
            a.refresh();
        }));
        shot(3600, "3-app-escolhido.png");
        step(4000, Box::new(|a| ActionRowExt::activate(&a.mixer.game_app)));
        popover_shot(4600, "4-menu-jogo.png");
        step(5000, Box::new(|a| {
            if let Some(p) = find_popover(a.window.upcast_ref()) {
                p.popdown();
            }
        }));
        step(5400, Box::new(|a| ActionRowExt::activate(&a.mixer.mic_device)));
        popover_shot(6000, "5-menu-microfone.png");
        step(6600, Box::new(|a| {
            if let Some(p) = find_popover(a.window.upcast_ref()) {
                p.popdown();
            }
        }));
        // Teste de microfone de verdade (2 s) e captura do resultado.
        step(7000, Box::new(|a| a.test_mic()));
        shot(10000, "6-teste-mic.png");
        // Sem microfone no modo: as linhas do microfone não devem aparecer.
        step(10300, Box::new(|a| {
            a.mixer.mic_switch.set_active(false);
            a.mixer.call_switch.set_active(false);
        }));
        shot(10900, "7-so-jogo.png");
        step(11200, Box::new(|a| {
            a.mixer.game_switch.set_active(false);
            a.mixer.mic_switch.set_active(true);
        }));
        shot(11800, "8-so-microfone.png");
        // Call ligada sem nenhum app de voz aberto: deve avisar.
        step(12100, Box::new(|a| {
            a.mixer.game_switch.set_active(true);
            a.mixer.game_app.set_selected(a.mixer.apps.borrow().len().min(1) as u32);
            a.mixer.call_switch.set_active(true);
            a.voice_apps.borrow_mut().clear();
            a.refresh();
        }));
        shot(12800, "9-call-sem-discord.png");
        step(13200, Box::new(|a| a.window.close()));
    }
}

/// Ícone do aplicativo: padrão de todas as janelas. Em desenvolvimento (`cargo run`)
/// ele ainda não está instalado, então o tema também procura em `data/icons`.
fn setup_icon() {
    if let Some(display) = gtk::gdk::Display::default() {
        let dev_icons = concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons");
        if std::path::Path::new(dev_icons).exists() {
            gtk::IconTheme::for_display(&display).add_search_path(dev_icons);
        }
    }
    gtk::Window::set_default_icon_name(APP_ID);
}

fn build_ui(application: &adw::Application) {
    setup_icon();
    // No modo de depuração a configuração fica na pasta das capturas: os cenários
    // mexem nos controles e não podem sobrescrever a configuração real do usuário.
    let config_path = match std::env::var("CATCHBACK_SNAPSHOT_DIR") {
        Ok(dir) => PathBuf::from(dir).join("config.toml"),
        Err(_) => default_config_path(std::env::var("XDG_CONFIG_HOME").ok().as_deref(), &home()),
    };
    let (config, config_error) = match Config::load(&config_path) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e.to_string())),
    };
    let recorder = Recorder::new(GstBackend::default(), FfmpegExporter, config.clone(), segment_dir());

    let mode_group = adw::ToggleGroup::builder().halign(gtk::Align::Center).build();
    mode_group.add(adw::Toggle::builder().name("replay").label("Replay").build());
    mode_group.add(adw::Toggle::builder().name("manual").label("Gravação").build());
    mode_group.set_active_name(Some("replay"));

    let status_page = adw::StatusPage::builder().vexpand(true).build();
    // Ícone menor: com o painel de áudio aberto a página não pode ser cortada.
    status_page.add_css_class("compact");
    let primary = gtk::Button::builder().halign(gtk::Align::Center).build();
    primary.add_css_class("pill");
    primary.add_css_class("suggested-action");
    let save = gtk::Button::builder().halign(gtk::Align::Center).visible(false).build();
    save.add_css_class("pill");

    let mixer = Mixer::new(&config);
    mixer.reload_apps(&config);
    mixer.reload_mics(&config);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&mode_group);
    content.append(&status_page);
    content.append(&mixer.group);
    content.append(&primary);
    content.append(&save);

    let menu = gio::Menu::new();
    menu.append(Some("Preferências"), Some("win.preferences"));
    let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build();
    let header = adw::HeaderBar::new();
    header.pack_end(&menu_button);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&adw::Clamp::builder().maximum_size(520).child(&content).build())
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroller));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));

    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Catchback")
        .default_width(460)
        .default_height(700)
        .content(&toasts)
        .build();

    let app = Rc::new(App {
        window: window.clone(),
        toasts,
        mode_group: mode_group.clone(),
        status_page,
        mixer,
        primary: primary.clone(),
        save: save.clone(),
        recorder: Arc::new(Mutex::new(recorder)),
        config_path,
        config: RefCell::new(config),
        save_pending: Cell::new(false),
        state: Cell::new(State::Idle),
        started_at: Cell::new(None),
        capture: RefCell::new(None),
        saving: Cell::new(false),
        separate_tracks: Cell::new(false),
        music_log: RefCell::new(MusicLog::new(MANUAL_LOG_KEEP)),
        detected: RefCell::new(Vec::new()),
        voice_apps: RefCell::new(Vec::new()),
        detecting: Cell::new(false),
        mic_testing: Cell::new(false),
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
        // Qualquer mexida no mixer vira configuração (e é gravada em disco).
        let m = &app.mixer;
        let changed = {
            let app = app.clone();
            move || app.mixer_changed()
        };
        for switch in [&m.game_switch, &m.mic_switch, &m.call_switch] {
            let c = changed.clone();
            switch.connect_active_notify(move |_| c());
        }
        let c = changed.clone();
        m.game_app.connect_selected_notify(move |_| c());
        for scale in [&m.game_volume, &m.mic_volume, &m.call_volume] {
            let c = changed.clone();
            scale.connect_value_changed(move |_| c());
        }
        let c = changed.clone();
        m.mic_device.connect_selected_notify(move |_| c());
        m.ask.connect_active_notify(move |_| changed());
        let app2 = app.clone();
        m.mic_test_button.connect_clicked(move |_| app2.test_mic());
        // A lista de apps é refeita ao abrir o painel.
        let app2 = app.clone();
        m.expander.connect_expanded_notify(move |row| {
            if row.is_expanded() && app2.state.get() == State::Idle {
                app2.mixer.reload_apps(&app2.config());
                app2.mixer.reload_mics(&app2.config());
                app2.refresh();
            }
        });
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
        let app = app.clone();
        glib::timeout_add_seconds_local(2, move || {
            app.detect_music();
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
            if let Err(e) = app.config.borrow().save(&app.config_path) {
                eprintln!("não foi possível gravar a configuração: {e}");
            }
            glib::Propagation::Proceed
        });
    }

    app.refresh();
    if let Some(e) = config_error {
        app.toast(&format!("Configuração inválida, usando o padrão ({e})"));
    }
    app.detect_music();
    window.present();
    if let Ok(dir) = std::env::var("CATCHBACK_SNAPSHOT_DIR") {
        debug_snapshot::run(&app, PathBuf::from(dir));
    }
}

fn main() -> glib::ExitCode {
    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_activate(build_ui);
    application.run()
}
