use std::{
    collections::HashMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use pumpkin_plugin_api::{
    Context, Plugin, PluginMetadata, Server,
    command::{Command, CommandError, CommandSender, ConsumedArgs},
    commands::CommandHandler,
    events::{EventData, EventHandler, EventPriority, PlayerJoinEvent, PlayerLeaveEvent},
    permission::{Permission, PermissionDefault},
    text::{NamedColor, TextComponent},
};
use tracing::{error, info, warn};

const DB_FILE_NAME: &str = "playtime_pumpkin.db";
const PERMISSION_PLAYTIME: &str = "playtime-pumpkin:command.playtime";

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn format_duration(mut secs: u64) -> String {
    let days = secs / 86_400;
    secs %= 86_400;
    let hours = secs / 3_600;
    secs %= 3_600;
    let minutes = secs / 60;
    let seconds = secs % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{}d", days));
    }
    if hours > 0 {
        parts.push(format!("{}h", hours));
    }
    if minutes > 0 {
        parts.push(format!("{}m", minutes));
    }
    parts.push(format!("{}s", seconds));
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PlaytimeStore {
    /// Active sessions: player UUID → Unix join timestamp (seconds).
    sessions: HashMap<String, u64>,
    /// Accumulated totals: player UUID → total seconds played.
    totals: HashMap<String, u64>,
}

impl PlaytimeStore {
    fn load_from_disk(path: &Path) -> Self {
        let contents = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                info!(path = %path.display(), "no playtime DB found, starting empty");
                return Self::default();
            }
            Err(e) => {
                warn!(path = %path.display(), reason = %e, "failed to read playtime DB, starting empty");
                return Self::default();
            }
        };

        let mut totals = HashMap::new();
        for (index, line) in contents.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split('\t');
            let Some(uuid) = parts.next().filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some(raw_secs) = parts.next() else {
                warn!("Ignoring malformed playtime line {}", index + 1);
                continue;
            };
            if parts.next().is_some() {
                warn!("Ignoring malformed playtime line {}", index + 1);
                continue;
            }
            match raw_secs.parse::<u64>() {
                Ok(secs) => {
                    totals.insert(uuid.to_string(), secs);
                }
                Err(_) => {
                    warn!("Ignoring playtime line {} with invalid seconds", index + 1);
                }
            }
        }

        info!(player_count = totals.len(), "playtime DB loaded");
        Self {
            sessions: HashMap::new(),
            totals,
        }
    }

    fn save_to_disk(&self, path: &Path) {
        let mut entries: Vec<(&String, &u64)> = self.totals.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));

        let mut content = String::new();
        for (uuid, &secs) in entries {
            content.push_str(uuid);
            content.push('\t');
            content.push_str(&secs.to_string());
            content.push('\n');
        }

        let temp_path = path.with_extension("tmp");
        if let Err(e) = fs::write(&temp_path, &content) {
            error!(path = %temp_path.display(), reason = %e, "failed to write temp playtime DB");
            return;
        }
        if let Err(e) = fs::rename(&temp_path, path) {
            error!(
                source = %temp_path.display(),
                destination = %path.display(),
                reason = %e,
                "failed to finalize playtime DB"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Service (shared handle passed into handlers and command executor)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct PlaytimeService {
    store: Arc<Mutex<PlaytimeStore>>,
    db_path: PathBuf,
}

impl PlaytimeService {
    fn new(db_path: PathBuf) -> Self {
        Self {
            store: Arc::new(Mutex::new(PlaytimeStore::default())),
            db_path,
        }
    }

    fn load(&self) {
        let loaded = PlaytimeStore::load_from_disk(&self.db_path);
        *self.store.lock().unwrap() = loaded;
    }

    fn record_join(&self, uuid: String) {
        self.store.lock().unwrap().sessions.insert(uuid, unix_now());
    }

    fn record_leave(&self, uuid: String) {
        let mut store = self.store.lock().unwrap();
        if let Some(joined_at) = store.sessions.remove(&uuid) {
            let session_secs = unix_now().saturating_sub(joined_at);
            *store.totals.entry(uuid).or_insert(0) += session_secs;
            store.save_to_disk(&self.db_path);
        } else {
            warn!(uuid, "no join time recorded (plugin loaded mid-session?)");
        }
    }

    fn total_playtime(&self, uuid: &str) -> u64 {
        let store = self.store.lock().unwrap();
        let saved = store.totals.get(uuid).copied().unwrap_or(0);
        let live = store
            .sessions
            .get(uuid)
            .map(|&t| unix_now().saturating_sub(t))
            .unwrap_or(0);
        saved + live
    }

    fn flush_all_sessions(&self) {
        let now = unix_now();
        let mut store = self.store.lock().unwrap();
        let uuids: Vec<String> = store.sessions.keys().cloned().collect();
        for uuid in uuids {
            if let Some(joined_at) = store.sessions.remove(&uuid) {
                *store.totals.entry(uuid).or_insert(0) += now.saturating_sub(joined_at);
            }
        }
        store.save_to_disk(&self.db_path);
    }
}

// ---------------------------------------------------------------------------
// Event handlers
// ---------------------------------------------------------------------------

struct JoinHandler {
    service: PlaytimeService,
}

impl EventHandler<PlayerJoinEvent> for JoinHandler {
    fn handle(
        &self,
        _server: Server,
        event: EventData<PlayerJoinEvent>,
    ) -> EventData<PlayerJoinEvent> {
        match event.player.get_id() {
            Ok(uuid) => self.service.record_join(uuid),
            Err(e) => warn!("get_id failed on join: {e}"),
        }
        event
    }
}

struct LeaveHandler {
    service: PlaytimeService,
}

impl EventHandler<PlayerLeaveEvent> for LeaveHandler {
    fn handle(
        &self,
        _server: Server,
        event: EventData<PlayerLeaveEvent>,
    ) -> EventData<PlayerLeaveEvent> {
        match event.player.get_id() {
            Ok(uuid) => self.service.record_leave(uuid),
            Err(e) => warn!("get_id failed on leave: {e}"),
        }
        event
    }
}

// ---------------------------------------------------------------------------
// Command executor
// ---------------------------------------------------------------------------

struct PlaytimeCommand {
    service: PlaytimeService,
}

impl CommandHandler for PlaytimeCommand {
    fn handle(
        &self,
        sender: CommandSender,
        _server: Server,
        _args: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let Some(player) = sender.as_player() else {
            sender.send_message(TextComponent::text("Only players can use /playtime."));
            return Ok(0);
        };

        let uuid = player.get_id().map_err(|e| {
            CommandError::CommandFailed(TextComponent::text(&format!(
                "Could not identify you: {e}"
            )))
        })?;
        let name = player.get_name().unwrap_or_else(|_| uuid.clone());

        let total = self.service.total_playtime(&uuid);

        let msg = TextComponent::text(&format!("{}'s playtime: {}", name, format_duration(total)));
        msg.color_named(NamedColor::Aqua);
        sender.send_message(msg);
        Ok(1)
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

struct PlaytimePumpkin {
    service: Option<PlaytimeService>,
}

impl Plugin for PlaytimePumpkin {
    fn new() -> Self {
        Self { service: None }
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "playtime-pumpkin".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: vec!["FabienLaurence".into()],
            description: "Tracks and displays player playtime.".into(),
        }
    }

    fn on_load(&mut self, context: Context) -> pumpkin_plugin_api::Result<()> {
        let db_path = PathBuf::from(context.get_data_folder()).join(DB_FILE_NAME);
        let service = PlaytimeService::new(db_path);
        service.load();

        context.register_event_handler(
            JoinHandler {
                service: service.clone(),
            },
            EventPriority::Normal,
            true,
        )?;
        context.register_event_handler(
            LeaveHandler {
                service: service.clone(),
            },
            EventPriority::Normal,
            true,
        )?;

        context.register_permission(&Permission {
            node: PERMISSION_PLAYTIME.to_string(),
            description: "Allows use of /playtime".to_string(),
            default: PermissionDefault::Allow,
            children: Vec::new(),
        })?;

        let command = Command::new(&["playtime".to_string()], "Show your total playtime.")
            .execute(PlaytimeCommand {
                service: service.clone(),
            });
        context.register_command(command, PERMISSION_PLAYTIME);

        self.service = Some(service);
        info!("PlaytimePumpkin loaded.");
        Ok(())
    }

    fn on_unload(&mut self, _context: Context) -> pumpkin_plugin_api::Result<()> {
        if let Some(service) = &self.service {
            service.flush_all_sessions();
        }
        info!("PlaytimePumpkin unloaded.");
        Ok(())
    }
}

pumpkin_plugin_api::register_plugin!(PlaytimePumpkin);
