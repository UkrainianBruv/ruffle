use crate::backends::{
    CpalAudioBackend, DesktopExternalInterfaceProvider, DesktopFSCommandProvider, DesktopUiBackend,
    RfdNavigatorInterface,
};
use crate::custom_event::RuffleEvent;
use crate::gui::MovieView;
use crate::preferences::GlobalPreferences;
use crate::{CALLSTACK, RENDER_INFO, SWF_INFO};
use anyhow::anyhow;
use ruffle_core::backend::navigator::{OpenURLMode, SocketMode};
use ruffle_core::config::Letterbox;
use ruffle_core::events::{GamepadButton, KeyCode};
use ruffle_core::{DefaultFont, LoadBehavior, Player, PlayerBuilder, PlayerEvent};
use ruffle_frontend_utils::backends::executor::{AsyncExecutor, PollRequester};
use ruffle_frontend_utils::backends::navigator::ExternalNavigatorBackend;
use ruffle_frontend_utils::bundle::source::BundleSourceError;
use ruffle_frontend_utils::bundle::{Bundle, BundleError};
use ruffle_frontend_utils::content::PlayingContent;
use ruffle_frontend_utils::player_options::PlayerOptions;
use ruffle_render::backend::RenderBackend;
use ruffle_render::quality::StageQuality;
use ruffle_render_wgpu::backend::WgpuRenderBackend;
use ruffle_render_wgpu::descriptors::Descriptors;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use url::Url;
use winit::event_loop::EventLoopProxy;
use winit::window::Window;

/// Options used when creating a Player (& passed through to a PlayerBuilder).
/// These may be primed by command line arguments.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub player: PlayerOptions,
    pub proxy: Option<Url>,
    pub socket_allowed: HashSet<String>,
    pub tcp_connections: Option<SocketMode>,
    pub fullscreen: bool,
    pub save_directory: PathBuf,
    pub open_url_mode: OpenURLMode,
    pub gamepad_button_mapping: HashMap<GamepadButton, KeyCode>,
    pub avm2_optimizer_enabled: bool,
}

impl From<&GlobalPreferences> for LaunchOptions {
    fn from(value: &GlobalPreferences) -> Self {
        Self {
            player: PlayerOptions {
                parameters: value.cli.parameters().collect(),
                max_execution_duration: value.cli.max_execution_duration,
                base: value.cli.base.clone(),
                quality: value.cli.quality,
                align: value.cli.align,
                force_align: if value.cli.force_align {
                    Some(true)
                } else {
                    None
                },
                scale: value.cli.scale,
                force_scale: if value.cli.force_scale {
                    Some(true)
                } else {
                    None
                },
                upgrade_to_https: if value.cli.upgrade_to_https {
                    Some(true)
                } else {
                    None
                },
                load_behavior: value.cli.load_behavior,
                letterbox: value.cli.letterbox,
                spoof_url: value.cli.spoof_url.clone(),
                player_version: value.cli.player_version,
                player_runtime: value.cli.player_runtime,
                frame_rate: value.cli.frame_rate,
                dummy_external_interface: if value.cli.dummy_external_interface {
                    Some(true)
                } else {
                    None
                },
            },
            proxy: value.cli.proxy.clone(),
            fullscreen: value.cli.fullscreen,
            save_directory: value.cli.save_directory.clone(),
            open_url_mode: value.cli.open_url_mode,
            socket_allowed: HashSet::from_iter(value.cli.socket_allow.iter().cloned()),
            tcp_connections: value.cli.tcp_connections,
            gamepad_button_mapping: HashMap::from_iter(value.cli.gamepad_button.iter().cloned()),
            avm2_optimizer_enabled: !value.cli.no_avm2_optimizer,
        }
    }
}

#[derive(Clone)]
struct WinitWaker(EventLoopProxy<RuffleEvent>);

impl PollRequester for WinitWaker {
    fn request_poll(&self) {
        if self.0.send_event(RuffleEvent::TaskPoll).is_err() {
            tracing::error!("Couldn't request poll - event loop is closed");
        }
    }
}

/// Represents a current Player and any associated state with that player,
/// which may be lost when this Player is closed (dropped)
struct ActivePlayer {
    player: Arc<Mutex<Player>>,
    executor: Arc<AsyncExecutor<WinitWaker>>,
}

impl ActivePlayer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        opt: &LaunchOptions,
        event_loop: EventLoopProxy<RuffleEvent>,
        movie_url: &Url,
        window: Rc<Window>,
        descriptors: Arc<Descriptors>,
        movie_view: MovieView,
        font_database: Rc<fontdb::Database>,
        preferences: GlobalPreferences,
    ) -> Self {
        let mut builder = PlayerBuilder::new();

        match CpalAudioBackend::new(&preferences) {
            Ok(audio) => {
                builder = builder.with_audio(audio);
            }
            Err(e) => {
                tracing::error!("Unable to create audio device: {}", e);
            }
        };

        let mut content = PlayingContent::DirectFile(movie_url.clone());
        if movie_url.scheme() == "file" {
            if let Ok(path) = movie_url.to_file_path() {
                match Bundle::from_path(&path) {
                    Ok(bundle) => {
                        if bundle.warnings().is_empty() {
                            tracing::info!("Opening bundle at {path:?}");
                        } else {
                            // TODO: Show warnings to user (toast?)
                            tracing::warn!("Opening bundle at {path:?} with warnings");
                            for warning in bundle.warnings() {
                                tracing::warn!("{warning}");
                            }
                        }
                        content = PlayingContent::Bundle(movie_url.clone(), bundle);
                    }
                    Err(BundleError::BundleDoesntExist)
                    | Err(BundleError::InvalidSource(BundleSourceError::UnknownSource)) => {
                        // Do nothing and carry on opening it as a swf - this likely isn't a bundle at all
                    }
                    Err(e) => {
                        // TODO: Visible popup when a bundle (or regular file) fails to open
                        tracing::error!("Couldn't open bundle at {path:?}: {e}");
                    }
                }
            }
        }

        let (executor, future_spawner) = AsyncExecutor::new(WinitWaker(event_loop.clone()));
        let movie_url = content.initial_swf_url().clone();
        let readable_name = content.name();
        let navigator = ExternalNavigatorBackend::new(
            opt.player
                .base
                .to_owned()
                .unwrap_or_else(|| movie_url.clone()),
            future_spawner,
            opt.proxy.clone(),
            opt.player.upgrade_to_https.unwrap_or_default(),
            opt.open_url_mode,
            opt.socket_allowed.clone(),
            opt.tcp_connections.unwrap_or(SocketMode::Ask),
            Rc::new(content),
            RfdNavigatorInterface,
        );

        if cfg!(feature = "software_video") {
            builder =
                builder.with_video(ruffle_video_software::backend::SoftwareVideoBackend::new());
        }

        let renderer = WgpuRenderBackend::new(descriptors, movie_view)
            .map_err(|e| anyhow!(e.to_string()))
            .expect("Couldn't create wgpu rendering backend");
        RENDER_INFO.with(|i| *i.borrow_mut() = Some(renderer.debug_info().to_string()));

        if opt.player.dummy_external_interface.unwrap_or_default() {
            builder = builder.with_external_interface(Box::new(DesktopExternalInterfaceProvider {
                spoof_url: opt.player.spoof_url.clone(),
            }));
        }

        if !opt.gamepad_button_mapping.is_empty() {
            builder = builder.with_gamepad_button_mapping(opt.gamepad_button_mapping.clone());
        }

        builder = builder
            .with_navigator(navigator)
            .with_renderer(renderer)
            .with_storage(preferences.storage_backend().create_backend(opt))
            .with_fs_commands(Box::new(DesktopFSCommandProvider {
                event_loop: event_loop.clone(),
                window: window.clone(),
            }))
            .with_ui(
                DesktopUiBackend::new(
                    window.clone(),
                    opt.open_url_mode,
                    font_database,
                    preferences,
                )
                .expect("Couldn't create ui backend"),
            )
            .with_autoplay(true)
            .with_letterbox(opt.player.letterbox.unwrap_or(Letterbox::On))
            .with_max_execution_duration(opt.player.max_execution_duration.unwrap_or(Duration::MAX))
            .with_quality(opt.player.quality.unwrap_or(StageQuality::High))
            .with_align(
                opt.player.align.unwrap_or_default(),
                opt.player.force_align.unwrap_or_default(),
            )
            .with_scale_mode(
                opt.player.scale.unwrap_or_default(),
                opt.player.force_scale.unwrap_or_default(),
            )
            .with_fullscreen(opt.fullscreen)
            .with_load_behavior(opt.player.load_behavior.unwrap_or(LoadBehavior::Streaming))
            .with_spoofed_url(opt.player.spoof_url.clone().map(|url| url.to_string()))
            .with_page_url(opt.player.spoof_url.clone().map(|url| url.to_string()))
            .with_player_version(opt.player.player_version)
            .with_player_runtime(opt.player.player_runtime.unwrap_or_default())
            .with_frame_rate(opt.player.frame_rate)
            .with_avm2_optimizer_enabled(opt.avm2_optimizer_enabled);
        let player = builder.build();

        window.set_title(&format!("Ruffle - {readable_name}"));

        SWF_INFO.with(|i| *i.borrow_mut() = Some(readable_name));

        let on_metadata = move |swf_header: &ruffle_core::swf::HeaderExt| {
            let _ = event_loop.send_event(RuffleEvent::OnMetadata(swf_header.clone()));
        };

        {
            let mut player_lock = player.lock().expect("Player lock must be available");
            CALLSTACK.with(|callstack| {
                *callstack.borrow_mut() = Some(player_lock.callstack());
            });
            player_lock.fetch_root_movie(
                movie_url.to_string(),
                opt.player.parameters.to_owned(),
                Box::new(on_metadata),
            );

            player_lock.set_default_font(
                DefaultFont::Serif,
                vec![
                    "Times New Roman".into(),
                    "Tinos".into(),
                    "Liberation Serif".into(),
                    "DejaVu Serif".into(),
                ],
            );
            player_lock.set_default_font(
                DefaultFont::Sans,
                vec![
                    "Arial".into(),
                    "Arimo".into(),
                    "Liberation Sans".into(),
                    "DejaVu Sans".into(),
                ],
            );
            player_lock.set_default_font(
                DefaultFont::Typewriter,
                vec![
                    "Courier New".into(),
                    "Cousine".into(),
                    "Liberation Mono".into(),
                    "DejaVu Sans Mono".into(),
                ],
            );
            player_lock.set_default_font(
                DefaultFont::JapaneseGothic,
                vec![
                    "ヒラギノ角ゴ Pro W3".into(), // Mac with Japanese environment
                    "MS UI Gothic".into(),        // Windows
                    "Noto Sans CJK JP".into(),    // Linux
                    "Arial Unicode MS".into(),    // Mac fallback
                ],
            );
            player_lock.set_default_font(
                DefaultFont::JapaneseGothicMono,
                vec![
                    "Osaka－等幅".into(),      // Mac with Japanese environment
                    "MS Gothic".into(),        // Windows
                    "Noto Sans CJK JP".into(), // Linux
                    "Arial Unicode MS".into(), // Mac fallback
                ],
            );
            player_lock.set_default_font(
                DefaultFont::JapaneseMincho,
                vec![
                    "ヒラギノ明朝 Pro W3".into(), // Mac with Japanese environment
                    "MS PMincho".into(),          // Windows
                    "Noto Sans CJK JP".into(),    // Linux
                    "Arial Unicode MS".into(),    // Mac fallback
                ],
            );
        }

        Self { player, executor }
    }
}

/// Owner of a Ruffle Player (via ActivePlayer),
/// responsible for either creating, destroying or communicating with that player.
pub struct PlayerController {
    player: Option<ActivePlayer>,
    event_loop: EventLoopProxy<RuffleEvent>,
    window: Rc<Window>,
    descriptors: Arc<Descriptors>,
    font_database: Rc<fontdb::Database>,
    preferences: GlobalPreferences,
}

impl PlayerController {
    pub fn new(
        event_loop: EventLoopProxy<RuffleEvent>,
        window: Rc<Window>,
        descriptors: Arc<Descriptors>,
        font_database: fontdb::Database,
        preferences: GlobalPreferences,
    ) -> Self {
        Self {
            player: None,
            event_loop,
            window,
            descriptors,
            font_database: Rc::new(font_database),
            preferences,
        }
    }

    pub fn create(&mut self, opt: &LaunchOptions, movie_url: &Url, movie_view: MovieView) {
        self.player = Some(ActivePlayer::new(
            opt,
            self.event_loop.clone(),
            movie_url,
            self.window.clone(),
            self.descriptors.clone(),
            movie_view,
            self.font_database.clone(),
            self.preferences.clone(),
        ));
    }

    pub fn destroy(&mut self) {
        self.player = None;
    }

    pub fn get(&self) -> Option<MutexGuard<Player>> {
        match &self.player {
            None => None,
            // We don't want to return None when the lock fails to grab as that's a fatal error, not a lack of player
            Some(player) => Some(
                player
                    .player
                    .try_lock()
                    .expect("Player lock must be available"),
            ),
        }
    }

    pub fn handle_event(&self, event: PlayerEvent) {
        if let Some(mut player) = self.get() {
            if player.is_playing() {
                player.handle_event(event);
            }
        }
    }

    pub fn poll(&self) {
        if let Some(player) = &self.player {
            player.executor.poll_all()
        }
    }
}
