use anyhow::Result;
use std::sync::mpsc;
use cosmic::{
    app::{Application, Core, Settings, Task},
    iced::{
        self,
        event,
        keyboard::{self, key::Named},
        window::Id as WindowId,
        Event, Subscription,
    },
    iced::platform_specific::shell::wayland::commands::layer_surface::{
        Anchor, KeyboardInteractivity, Layer, get_layer_surface, destroy_layer_surface,
    },
    iced::platform_specific::runtime::wayland::layer_surface::SctkLayerSurfaceSettings,
    Element,
};
use crate::wayland::{ActivateCommand, ToplevelEntry};
use switcher_config::{Theme, ThemeValues};

pub const INACTIVITY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Clone)]
pub struct SharedListener(pub std::sync::Arc<std::sync::Mutex<Option<std::os::unix::net::UnixListener>>>);

impl std::hash::Hash for SharedListener {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::sync::Arc::as_ptr(&self.0).hash(state);
    }
}

impl PartialEq for SharedListener {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SharedListener {}

pub struct AppSwitcher {
    core:              Core,
    pub toplevels:     Vec<ToplevelEntry>,
    pub selected:      usize,
    cmd_tx:            mpsc::SyncSender<ActivateCommand>,
    super_held:        bool,
    alt_held:          bool,
    modifiers: keyboard::Modifiers,
    initial_shortcut: bool,
    pending_activation: Option<std::time::Instant>,
    surface_id:        WindowId,
    pub theme:         ThemeValues,
    pub visuals:       Vec<crate::icons::AppVisual>,
    listener:          SharedListener,
    pub last_activity: std::time::Instant,
}

#[derive(Debug, Clone)]
pub enum Message {
    SelectNext,
    SelectPrev,
    SelectIndex(usize),
    Activate,
    Cancel,
    KeyEvent(keyboard::Event),
    Tick(std::time::Instant),
}

impl Application for AppSwitcher {
    type Executor = cosmic::executor::Default;
    type Flags    = (
        Vec<ToplevelEntry>,
        bool,
        bool,
        mpsc::SyncSender<ActivateCommand>,
        Theme,
        Option<std::os::unix::net::UnixListener>,
    );
    type Message  = Message;

    const APP_ID: &'static str = "io.github.cosmic-ext-app-switcher";

    fn core(&self) -> &Core { &self.core }
    fn core_mut(&mut self) -> &mut Core { &mut self.core }

    fn init(core: Core, flags: Self::Flags) -> (Self, Task<Message>) {
        let (toplevels, reverse, shortcut, cmd_tx, theme_preset, listener_opt) = flags;
        let n = toplevels.len();
        let selected = if reverse {
            n.saturating_sub(1)
        } else {
            1.min(n.saturating_sub(1))
        };

        let theme = theme_preset.values();
        let (surface_w, surface_h) = crate::ui::surface_size(toplevels.len(), &theme);
        let visuals: Vec<crate::icons::AppVisual> = toplevels
            .iter()
            .map(|t| crate::icons::visual_for(&t.app_id, &t.title, theme.icon_size))
            .collect();

        let surface_id = WindowId::unique();
        let layer_task = get_layer_surface::<cosmic::Action<Message>>(SctkLayerSurfaceSettings {
            id: surface_id,
            layer: Layer::Overlay,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            anchor: Anchor::empty(),
            size: Some((Some(surface_w), Some(surface_h))),
            ..Default::default()
        });

        (
            AppSwitcher {
                core,
                toplevels,
                selected,
                cmd_tx,
                super_held: false,
                alt_held: false,
                modifiers: keyboard::Modifiers::empty(),
                initial_shortcut: shortcut,
                pending_activation: None,
                surface_id,
                theme,
                visuals,
                listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(listener_opt))),
                last_activity: std::time::Instant::now(),
            },
            layer_task,
        )
    }

    fn update(&mut self, msg: Message) -> Task<Message> {
        self.update_session(msg, false)
    }

    fn subscription(&self) -> Subscription<Message> {
        let key_sub = event::listen_with(|event, _status, _window| {
            match event {
                Event::Keyboard(ke) => Some(Message::KeyEvent(ke)),
                Event::Window(iced::window::Event::Unfocused) => Some(Message::Cancel),
                _ => None,
            }
        });

        // Listen on the Unix socket for cycle commands from subsequent binary invocations.
        let socket_sub = {
            use cosmic::iced::futures::SinkExt;
            Subscription::run_with(self.listener.clone(), |shared| {
                let listener_arc = shared.0.clone();
                cosmic::iced::stream::channel(16, move |mut tx: cosmic::iced::futures::channel::mpsc::Sender<Message>| async move {
                    use tokio::io::AsyncReadExt;
                    let tokio_listener = if let Some(std_listener) = listener_arc.lock().ok().and_then(|mut guard| guard.take()) {
                        tokio::net::UnixListener::from_std(std_listener).ok()
                    } else {
                        None
                    };

                    let listener = match tokio_listener {
                        Some(l) => l,
                        None => match tokio::net::UnixListener::bind(crate::socket_path()) {
                            Ok(l) => l,
                            Err(e) => {
                                log::error!("failed to bind switcher socket at {:?}: {e}", crate::socket_path());
                                std::future::pending::<()>().await;
                                unreachable!()
                            }
                        }
                    };
                    loop {
                        if let Ok((mut stream, _)) = listener.accept().await {
                            let mut buf = Vec::new();
                            let _ = stream.read_to_end(&mut buf).await;
                            let msg = match buf.as_slice() {
                                b"next" => Message::SelectNext,
                                b"prev" => Message::SelectPrev,
                                _ => continue,
                            };
                            let _ = tx.send(msg).await;
                        }
                    }
                })
            })
        };

        let timer_sub = cosmic::iced::time::every(std::time::Duration::from_millis(20)).map(Message::Tick);

        Subscription::batch([key_sub, socket_sub, timer_sub])
    }

    fn view(&self) -> Element<'_, Message> {
        crate::ui::view(self)
    }

    fn view_window(&self, _id: WindowId) -> Element<'_, Message> {
        crate::ui::view(self)
    }
}

impl AppSwitcher {
    pub(crate) fn update_session(&mut self, msg: Message, persistent: bool) -> Task<Message> {
        let n = self.toplevels.len();
        match msg {
            Message::SelectNext => {
                self.last_activity = std::time::Instant::now();
                if self.pending_activation.is_some() { self.pending_activation = Some(self.last_activity + std::time::Duration::from_millis(100)); }
                self.selected = (self.selected + 1) % n;
            }
            Message::SelectPrev => {
                self.last_activity = std::time::Instant::now();
                if self.pending_activation.is_some() { self.pending_activation = Some(self.last_activity + std::time::Duration::from_millis(100)); }
                self.selected = (self.selected + n - 1) % n;
            }
            Message::SelectIndex(i) => {
                self.last_activity = std::time::Instant::now();
                self.selected = i;
            }
            Message::Tick(now) => {
                if self.pending_activation.is_some_and(|deadline| now >= deadline) {
                    self.pending_activation = None;
                    return self.update_session(Message::Activate, persistent);
                }
                if !self.initial_shortcut && self.modifiers.is_empty() && now.duration_since(self.last_activity) >= INACTIVITY_TIMEOUT {
                    log::info!("Inactivity timeout expired; activating selected window");
                    return self.update_session(Message::Activate, persistent);
                }
            }
            Message::Activate => {
                self.cmd_tx
                    .send(ActivateCommand::Activate(self.toplevels[self.selected].handle_key))
                    .ok();
                if persistent { self.toplevels.clear(); }
                return Task::batch([
                    destroy_layer_surface(self.surface_id),
                    if persistent { Task::none() } else { iced::exit() },
                ]);
            }
            Message::Cancel => {
                self.cmd_tx.send(ActivateCommand::Cancel).ok();
                if persistent { self.toplevels.clear(); }
                return Task::batch([
                    destroy_layer_surface(self.surface_id),
                    if persistent { Task::none() } else { iced::exit() },
                ]);
            }
            Message::KeyEvent(ke) => {
                self.last_activity = std::time::Instant::now();
                match ke {
                keyboard::Event::KeyPressed { key, modifiers, .. } => {
                    self.modifiers = modifiers;
                    if modifiers.logo() {
                        self.super_held = true;
                    }
                    if modifiers.alt() {
                        self.alt_held = true;
                    }
                    match key {
                        iced::keyboard::Key::Named(Named::Tab) => {
                            return self.update_session(if modifiers.shift() {
                                Message::SelectPrev
                            } else {
                                Message::SelectNext
                            }, persistent);
                        }
                        iced::keyboard::Key::Named(Named::Escape) => {
                            return self.update_session(Message::Cancel, persistent);
                        }
                        iced::keyboard::Key::Named(Named::Enter) => {
                            return self.update_session(Message::Activate, persistent);
                        }
                        _ => {}
                    }
                }
                keyboard::Event::ModifiersChanged(mods) => {
                    self.modifiers = mods;
                    if mods.alt() || mods.logo() { self.pending_activation = None; }
                    let first = std::mem::take(&mut self.initial_shortcut);
                    if first && !mods.alt() && !mods.logo() {
                        self.pending_activation = Some(std::time::Instant::now() + std::time::Duration::from_millis(100));
                    }
                    let activate = (self.super_held && !mods.logo())
                        || (self.alt_held && !mods.alt());
                    self.super_held = mods.logo();
                    self.alt_held = mods.alt();
                    if activate {
                        return self.update_session(Message::Activate, persistent);
                    }
                }
                _ => {}
            }
        }
    }
    Task::none()
    }


    pub(crate) fn surface_id(&self) -> WindowId { self.surface_id }
}

pub fn run(
    toplevels: Vec<ToplevelEntry>,
    reverse: bool,
    shortcut: bool,
    cmd_tx: mpsc::SyncSender<ActivateCommand>,
    theme: Theme,
    listener: Option<std::os::unix::net::UnixListener>,
) -> Result<()> {
    let settings = Settings::default().no_main_window(true);
    cosmic::app::run::<AppSwitcher>(settings, (toplevels, reverse, shortcut, cmd_tx, theme, listener))
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::keyboard::{self, key::Named, Modifiers};

    fn switcher_for_test(shortcut: bool, selected: usize) -> (AppSwitcher, mpsc::Receiver<ActivateCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(4);
        let toplevels = (0..3).map(|i| ToplevelEntry {
            app_id: i.to_string(), title: i.to_string(), is_active: i == 0, handle_key: i,
        }).collect();
        (AppSwitcher {
            core: Core::default(), toplevels, selected, cmd_tx,
            super_held: false, alt_held: false, modifiers: Modifiers::empty(),
            initial_shortcut: shortcut, pending_activation: None, surface_id: WindowId::unique(),
            theme: Theme::Dark.values(), visuals: vec![],
            listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(None))),
            last_activity: std::time::Instant::now(),
        }, cmd_rx)
    }

    #[test]
    fn persistent_sessions_reset_modifiers_and_settling_on_every_open() {
        for _ in 0..2 {
            let (mut app, rx) = switcher_for_test(true, 1);
            assert!(!app.alt_held && !app.super_held);
            assert!(app.modifiers.is_empty() && app.pending_activation.is_none());
            let _ = app.update_session(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::empty())), true);
            let deadline = app.pending_activation.unwrap();
            let _ = app.update_session(Message::Tick(deadline - std::time::Duration::from_millis(1)), true);
            assert!(rx.try_recv().is_err());
            let _ = app.update_session(Message::Tick(deadline), true);
            assert!(matches!(rx.try_recv(), Ok(ActivateCommand::Activate(1))));
            assert!(rx.try_recv().is_err());
            assert!(app.toplevels.is_empty());
        }
    }

    #[test]
    fn persistent_cancel_closes_session() {
        let (mut app, rx) = switcher_for_test(true, 1);
        let _ = app.update_session(Message::Cancel, true);
        assert!(matches!(rx.try_recv(), Ok(ActivateCommand::Cancel)));
        assert!(app.toplevels.is_empty());
    }

    #[test]
    fn quick_shortcut_activates_initial_next_or_previous_selection() {
        for selected in [1, 2] {
            let (mut app, rx) = switcher_for_test(true, selected);
            let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::empty())));
            assert!(rx.try_recv().is_err());
            let _ = app.update(Message::Tick(std::time::Instant::now() + std::time::Duration::from_millis(100)));
            assert!(matches!(rx.try_recv(), Ok(ActivateCommand::Activate(i)) if i == selected));
        }
    }

    #[test]
    fn cycling_restarts_pending_activation_and_uses_latest_selection() {
        let (mut app, rx) = switcher_for_test(true, 1);
        let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::empty())));
        let old_deadline = app.pending_activation.unwrap();
        let _ = app.update(Message::SelectNext);
        let new_deadline = app.pending_activation.unwrap();
        assert!(new_deadline >= old_deadline);
        let _ = app.update(Message::Tick(new_deadline));
        assert!(matches!(rx.try_recv(), Ok(ActivateCommand::Activate(2))));
    }

    #[test]
    fn modifier_press_cancels_pending_activation_until_release() {
        let (mut app, rx) = switcher_for_test(true, 1);
        let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::empty())));
        let deadline = app.pending_activation.unwrap();
        let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::ALT)));
        let _ = app.update(Message::Tick(deadline));
        assert!(rx.try_recv().is_err());
        let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::empty())));
        assert!(matches!(rx.try_recv(), Ok(ActivateCommand::Activate(1))));
    }

    #[test]
    fn initial_held_modifier_waits_for_release_and_prevents_timeout() {
        for modifier in [Modifiers::ALT, Modifiers::LOGO] {
            let (mut app, rx) = switcher_for_test(true, 1);
            let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(modifier)));
            let _ = app.update(Message::Tick(app.last_activity + INACTIVITY_TIMEOUT + std::time::Duration::from_secs(1)));
            assert!(rx.try_recv().is_err());
            let _ = app.update(Message::SelectNext);
            let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::empty())));
            assert!(matches!(rx.try_recv(), Ok(ActivateCommand::Activate(2))));
        }
    }

    #[test]
    fn direct_launch_waits_and_all_modifiers_block_timeout() {
        let (mut app, rx) = switcher_for_test(false, 1);
        let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::empty())));
        assert!(rx.try_recv().is_err());
        let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::SHIFT)));
        let _ = app.update(Message::Tick(app.last_activity + INACTIVITY_TIMEOUT + std::time::Duration::from_secs(1)));
        assert!(rx.try_recv().is_err());
        let _ = app.update(Message::KeyEvent(keyboard::Event::ModifiersChanged(Modifiers::empty())));
        let _ = app.update(Message::Tick(app.last_activity + INACTIVITY_TIMEOUT));
        assert!(matches!(rx.try_recv(), Ok(ActivateCommand::Activate(1))));
    }

    #[test]
    fn shortcut_does_not_timeout_before_initial_modifier_state() {
        let (mut app, rx) = switcher_for_test(true, 1);
        let _ = app.update(Message::Tick(app.last_activity + INACTIVITY_TIMEOUT + std::time::Duration::from_secs(1)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn inactivity_timeout_triggers_activation() {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(1);
        let toplevels = vec![ToplevelEntry {
            app_id: "test".into(),
            title: "Test App".into(),
            is_active: false,
            handle_key: 7,
        }];
        let mut app = AppSwitcher {
            core: Core::default(),
            toplevels,
            selected: 0,
            cmd_tx,
            super_held: false,
            alt_held: false,
            modifiers: keyboard::Modifiers::empty(),
            initial_shortcut: false,
            pending_activation: None,
            surface_id: WindowId::unique(),
            theme: Theme::Dark.values(),
            visuals: vec![],
            listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(None))),
            last_activity: std::time::Instant::now() - INACTIVITY_TIMEOUT - std::time::Duration::from_secs(1),
        };

        let _ = app.update(Message::Tick(std::time::Instant::now()));

        match cmd_rx.try_recv() {
            Ok(ActivateCommand::Activate(key)) => assert_eq!(key, 7),
            other => panic!("expected Activate(7) on timeout, got {other:?}"),
        }
    }

    #[test]
    fn user_activity_resets_inactivity_timeout() {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(1);
        let toplevels = vec![
            ToplevelEntry { app_id: "a".into(), title: "A".into(), is_active: false, handle_key: 1 },
            ToplevelEntry { app_id: "b".into(), title: "B".into(), is_active: false, handle_key: 2 },
        ];
        let mut app = AppSwitcher {
            core: Core::default(),
            toplevels,
            selected: 0,
            cmd_tx,
            super_held: false,
            alt_held: false,
            modifiers: keyboard::Modifiers::empty(),
            initial_shortcut: false,
            pending_activation: None,
            surface_id: WindowId::unique(),
            theme: Theme::Dark.values(),
            visuals: vec![],
            listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(None))),
            last_activity: std::time::Instant::now() - INACTIVITY_TIMEOUT,
        };

        // User interacts (cycles to next window) -> last_activity is reset
        let _ = app.update(Message::SelectNext);
        assert_eq!(app.selected, 1);
        assert!(std::time::Instant::now().duration_since(app.last_activity) < std::time::Duration::from_millis(100));

        // Tick right after: should NOT activate because timeout was reset
        let _ = app.update(Message::Tick(std::time::Instant::now()));
        assert!(cmd_rx.try_recv().is_err(), "should not activate immediately after user activity");
    }

    #[test]
    fn alt_held_recorded_on_key_press_and_activates_on_release() {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(1);
        let toplevels = vec![ToplevelEntry {
            app_id: "test".into(),
            title: "Test App".into(),
            is_active: false,
            handle_key: 42,
        }];
        let mut app = AppSwitcher {
            core: Core::default(),
            toplevels,
            selected: 0,
            cmd_tx,
            super_held: false,
            alt_held: false,
            modifiers: keyboard::Modifiers::empty(),
            initial_shortcut: false,
            pending_activation: None,
            surface_id: WindowId::unique(),
            theme: Theme::Dark.values(),
            visuals: vec![],
            listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(None))),
            last_activity: std::time::Instant::now(),
        };

        // User presses Alt+Tab: KeyPressed with ALT modifier arrives
        let key_event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(Named::Tab),
            modified_key: keyboard::Key::Named(Named::Tab),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: Modifiers::ALT,
            repeat: false,
            text: None,
        };
        let _ = app.update(Message::KeyEvent(key_event));
        assert!(app.alt_held, "app.alt_held should be set by KeyPressed with Alt modifier");

        // User releases Alt: ModifiersChanged with empty modifiers arrives
        let mod_event = keyboard::Event::ModifiersChanged(Modifiers::empty());
        let _ = app.update(Message::KeyEvent(mod_event));

        // cmd_rx should have received Activate(42)
        match cmd_rx.try_recv() {
            Ok(ActivateCommand::Activate(key)) => assert_eq!(key, 42),
            other => panic!("expected Activate(42), got {other:?}"),
        }
    }

    #[test]
    fn super_held_recorded_on_key_press_and_activates_on_release() {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(1);
        let toplevels = vec![ToplevelEntry {
            app_id: "test".into(),
            title: "Test App".into(),
            is_active: false,
            handle_key: 10,
        }];
        let mut app = AppSwitcher {
            core: Core::default(),
            toplevels,
            selected: 0,
            cmd_tx,
            super_held: false,
            alt_held: false,
            modifiers: keyboard::Modifiers::empty(),
            initial_shortcut: false,
            pending_activation: None,
            surface_id: WindowId::unique(),
            theme: Theme::Dark.values(),
            visuals: vec![],
            listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(None))),
            last_activity: std::time::Instant::now(),
        };

        // User presses Super+Tab: KeyPressed with LOGO modifier arrives
        let key_event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(Named::Tab),
            modified_key: keyboard::Key::Named(Named::Tab),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: Modifiers::LOGO,
            repeat: false,
            text: None,
        };
        let _ = app.update(Message::KeyEvent(key_event));
        assert!(app.super_held, "app.super_held should be set by KeyPressed with Logo modifier");

        // User releases Super: ModifiersChanged with empty modifiers arrives
        let mod_event = keyboard::Event::ModifiersChanged(Modifiers::empty());
        let _ = app.update(Message::KeyEvent(mod_event));

        match cmd_rx.try_recv() {
            Ok(ActivateCommand::Activate(key)) => assert_eq!(key, 10),
            other => panic!("expected Activate(10), got {other:?}"),
        }
    }

    #[test]
    fn cancel_message_sends_cancel_command() {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(1);
        let mut app = AppSwitcher {
            core: Core::default(),
            toplevels: vec![],
            selected: 0,
            cmd_tx,
            super_held: false,
            alt_held: false,
            modifiers: keyboard::Modifiers::empty(),
            initial_shortcut: false,
            pending_activation: None,
            surface_id: WindowId::unique(),
            theme: Theme::Dark.values(),
            visuals: vec![],
            listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(None))),
            last_activity: std::time::Instant::now(),
        };

        let _ = app.update(Message::Cancel);
        match cmd_rx.try_recv() {
            Ok(ActivateCommand::Cancel) => {}
            other => panic!("expected Cancel, got {other:?}"),
        }
    }

    #[test]
    fn escape_key_sends_cancel_command() {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(1);
        let mut app = AppSwitcher {
            core: Core::default(),
            toplevels: vec![],
            selected: 0,
            cmd_tx,
            super_held: false,
            alt_held: false,
            modifiers: keyboard::Modifiers::empty(),
            initial_shortcut: false,
            pending_activation: None,
            surface_id: WindowId::unique(),
            theme: Theme::Dark.values(),
            visuals: vec![],
            listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(None))),
            last_activity: std::time::Instant::now(),
        };

        let key_event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(Named::Escape),
            modified_key: keyboard::Key::Named(Named::Escape),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: Modifiers::empty(),
            repeat: false,
            text: None,
        };
        let _ = app.update(Message::KeyEvent(key_event));
        match cmd_rx.try_recv() {
            Ok(ActivateCommand::Cancel) => {}
            other => panic!("expected Cancel on Escape, got {other:?}"),
        }
    }

    #[test]
    fn enter_key_sends_activate_command() {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(1);
        let toplevels = vec![ToplevelEntry {
            app_id: "test".into(),
            title: "Test App".into(),
            is_active: false,
            handle_key: 7,
        }];
        let mut app = AppSwitcher {
            core: Core::default(),
            toplevels,
            selected: 0,
            cmd_tx,
            super_held: false,
            alt_held: false,
            modifiers: keyboard::Modifiers::empty(),
            initial_shortcut: false,
            pending_activation: None,
            surface_id: WindowId::unique(),
            theme: Theme::Dark.values(),
            visuals: vec![],
            listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(None))),
            last_activity: std::time::Instant::now(),
        };

        let key_event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(Named::Enter),
            modified_key: keyboard::Key::Named(Named::Enter),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: Modifiers::empty(),
            repeat: false,
            text: None,
        };
        let _ = app.update(Message::KeyEvent(key_event));
        match cmd_rx.try_recv() {
            Ok(ActivateCommand::Activate(key)) => assert_eq!(key, 7),
            other => panic!("expected Activate(7) on Enter, got {other:?}"),
        }
    }

    #[test]
    fn shift_tab_cycles_backwards() {
        let (cmd_tx, _cmd_rx) = mpsc::sync_channel(1);
        let toplevels = vec![
            ToplevelEntry { app_id: "a".into(), title: "A".into(), is_active: false, handle_key: 1 },
            ToplevelEntry { app_id: "b".into(), title: "B".into(), is_active: false, handle_key: 2 },
            ToplevelEntry { app_id: "c".into(), title: "C".into(), is_active: false, handle_key: 3 },
        ];
        let mut app = AppSwitcher {
            core: Core::default(),
            toplevels,
            selected: 0,
            cmd_tx,
            super_held: false,
            alt_held: false,
            modifiers: keyboard::Modifiers::empty(),
            initial_shortcut: false,
            pending_activation: None,
            surface_id: WindowId::unique(),
            theme: Theme::Dark.values(),
            visuals: vec![],
            listener: SharedListener(std::sync::Arc::new(std::sync::Mutex::new(None))),
            last_activity: std::time::Instant::now(),
        };

        // Shift+Tab from index 0 wraps to index 2
        let key_event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(Named::Tab),
            modified_key: keyboard::Key::Named(Named::Tab),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: Modifiers::SHIFT,
            repeat: false,
            text: None,
        };
        let _ = app.update(Message::KeyEvent(key_event));
        assert_eq!(app.selected, 2, "Shift+Tab from 0 should wrap to last element (2)");

        // Plain Tab from index 2 wraps to index 0
        let key_event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(Named::Tab),
            modified_key: keyboard::Key::Named(Named::Tab),
            physical_key: keyboard::key::Physical::Unidentified(keyboard::key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers: Modifiers::empty(),
            repeat: false,
            text: None,
        };
        let _ = app.update(Message::KeyEvent(key_event));
        assert_eq!(app.selected, 0, "Tab from 2 should wrap to first element (0)");
    }
}
