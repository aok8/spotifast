//! Durable, account-scoped playlist rotation with guarded, recoverable edits.
use crate::api::{
    ApiClient, ApiError, ApiGateway, Operation as ApiOperation, PlayRequest,
    models::{PlayableItem, Playlist, PlaylistItem, Track},
};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug)]
pub enum Request {
    Load,
    Sync,
    Configure {
        source: Box<Playlist>,
        archive: Option<Box<Playlist>>,
    },
    Restart,
    ResolveReload,
    Finish {
        device_id: Option<String>,
    },
    Select {
        index: usize,
    },
    Play {
        index: usize,
        device_id: String,
    },
    Pause {
        device_id: String,
    },
    Resume {
        device_id: String,
    },
    StopRemoved {
        device_id: String,
    },
    Seek {
        device_id: String,
        position_ms: u32,
    },
    Remove {
        item_key: String,
        without_archive: bool,
    },
    Retry,
}
impl Request {
    /// Playback work belongs to the currently enabled mode. Playlist edits
    /// survive leaving that mode so their durable recovery markers settle.
    pub(crate) fn is_transport(&self) -> bool {
        matches!(
            self,
            Self::Play { .. }
                | Self::Pause { .. }
                | Self::Resume { .. }
                | Self::Seek { .. }
                | Self::StopRemoved { .. }
                | Self::Finish { .. }
                | Self::Sync
        )
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub key: String,
    pub raw_position: u32,
    pub track: Track,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Stage {
    Pending,
    ArchiveUncertain,
    Archived,
    DeleteUncertain,
    Complete,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Operation {
    pub item_key: String,
    pub stage: Stage,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QueuedWindow {
    pub start_index: usize,
    pub end_index: usize,
    pub device_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct State {
    version: u32,
    pub account_id: String,
    pub source: Option<Playlist>,
    pub archive: Option<Playlist>,
    pub items: Vec<Entry>,
    pub order: Vec<String>,
    pub current_index: usize,
    pub operation: Option<Operation>,
    #[serde(default)]
    pub queued_window: Option<QueuedWindow>,
    #[serde(default)]
    pub playback_cooldown_until: u64,
    #[serde(skip)]
    pub error: Option<String>,
    #[serde(skip)]
    pub playback: Option<crate::api::models::PlaybackState>,
}
impl Default for State {
    fn default() -> Self {
        Self::new(String::new())
    }
}
impl State {
    #[cfg(any(test, feature = "demo"))]
    pub fn demo(source: Playlist, archive: Option<Playlist>, tracks: Vec<Track>) -> Self {
        let mut state = Self::new("demo".into());
        let rows: Vec<_> = tracks
            .into_iter()
            .map(|track| PlaylistItem {
                item: Some(PlayableItem::Track(track)),
                ..PlaylistItem::default()
            })
            .collect();
        state.replace(source, &rows);
        state.archive = archive;
        state.order = state.items.iter().map(|entry| entry.key.clone()).collect();
        state
    }
    pub fn new(account_id: String) -> Self {
        Self {
            version: 1,
            account_id,
            source: None,
            archive: None,
            items: vec![],
            order: vec![],
            current_index: 0,
            operation: None,
            queued_window: None,
            playback_cooldown_until: 0,
            error: None,
            playback: None,
        }
    }
    pub fn current(&self) -> Option<&Entry> {
        let key = self.order.get(self.current_index)?;
        self.items.iter().find(|entry| &entry.key == key)
    }
    pub fn ordered_items(&self) -> Vec<&Entry> {
        let entries: HashMap<_, _> = self
            .items
            .iter()
            .map(|entry| (entry.key.as_str(), entry))
            .collect();
        self.order
            .iter()
            .filter_map(|key| entries.get(key.as_str()).copied())
            .collect()
    }
    pub fn unresolved(&self) -> bool {
        self.operation
            .as_ref()
            .is_some_and(|operation| !matches!(operation.stage, Stage::Complete | Stage::Failed))
    }
    fn validate(&self, account: &str) -> Result<(), String> {
        if self.version != 1 || self.account_id != account {
            return Err("Rotation state belongs to another account or format.".into());
        }
        if self
            .source
            .as_ref()
            .zip(self.archive.as_ref())
            .is_some_and(|(source, archive)| source.id == archive.id)
        {
            return Err("Choose a different archive playlist.".into());
        }
        let keys: HashSet<_> = self.items.iter().map(|entry| &entry.key).collect();
        let order: HashSet<_> = self.order.iter().collect();
        if keys.len() != self.items.len()
            || order != keys
            || order.len() != self.order.len()
            || self.current_index > self.order.len()
        {
            return Err(
                "Rotation state is damaged. Its saved recovery information has been kept.".into(),
            );
        }
        if self.unresolved()
            && self
                .operation
                .as_ref()
                .map(|operation| operation.item_key.as_str())
                != self.current().map(|entry| entry.key.as_str())
        {
            return Err("Rotation's saved removal refers to a different current song. Its recovery information has been kept.".into());
        }
        if self.source.is_none()
            && (!self.items.is_empty() || self.archive.is_some() || self.operation.is_some())
        {
            return Err(
                "Rotation's saved source is missing. Its recovery information has been kept."
                    .into(),
            );
        }
        if self.queued_window.as_ref().is_some_and(|window| {
            window.device_id.is_empty()
                || window.start_index >= window.end_index
                || window.end_index > self.order.len()
                || self.current_index < window.start_index
                || self.current_index >= window.end_index
        }) {
            return Err("Rotation's saved playback window is damaged. Its recovery information has been kept.".into());
        }
        Ok(())
    }
    fn replace(&mut self, source: Playlist, rows: &[PlaylistItem]) {
        let previous = self.current().map(|entry| entry.track.uri.clone());
        let previous_key = self.current().map(|entry| entry.key.clone());
        self.items = rows
            .iter()
            .enumerate()
            .filter_map(|(position, row)| match row.playable() {
                Some(PlayableItem::Track(track))
                    if !row.is_local
                        && !track.is_local
                        && track.is_playable != Some(false)
                        && track.restrictions.is_none()
                        && track.uri.strip_prefix("spotify:track:").is_some_and(|id| {
                            !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
                        }) =>
                {
                    Some(Entry {
                        key: format!("{position}:{}", track.uri),
                        raw_position: position as u32,
                        track: track.clone(),
                    })
                }
                _ => None,
            })
            .collect();
        self.order = self.items.iter().map(|entry| entry.key.clone()).collect();
        self.order.shuffle(&mut rand::rng());
        if self.order.len() > 1
            && let Some(index) = self.order.iter().position(|key| {
                self.items.iter().any(|entry| {
                    &entry.key == key && Some(entry.track.uri.as_str()) != previous.as_deref()
                })
            })
        {
            self.order.swap(0, index);
        }
        if self.order.len() > 1
            && self.order.first() == previous_key.as_ref()
            && let Some(index) = self
                .order
                .iter()
                .position(|key| Some(key) != previous_key.as_ref())
        {
            self.order.swap(0, index);
        }
        self.current_index = 0;
        self.source = Some(source);
        self.operation = None;
        self.queued_window = None;
    }
    fn guard(&self, source: &Playlist, rows: &[PlaylistItem]) -> Result<(), String> {
        let saved = self
            .source
            .as_ref()
            .ok_or("Choose a source playlist first.")?;
        let entry = self.current().ok_or("There is no current song.")?;
        if saved.snapshot_id.is_none() || saved.snapshot_id != source.snapshot_id {
            return Err(
                "The source playlist changed. Restart Rotation before removing a song.".into(),
            );
        }
        let count = rows
            .iter()
            .filter_map(PlaylistItem::playable)
            .filter(|item| item.uri() == entry.track.uri)
            .count();
        if count != 1 {
            return Err("This song occurs more than once in the source playlist. Removal is blocked to protect the other copies.".into());
        }
        if rows
            .get(entry.raw_position as usize)
            .and_then(PlaylistItem::playable)
            .map(PlayableItem::uri)
            != Some(entry.track.uri.as_str())
        {
            return Err(
                "The song moved in the source playlist. Restart Rotation before removing it."
                    .into(),
            );
        }
        Ok(())
    }
    fn complete(&mut self, snapshot: String) {
        self.queued_window = None;
        let entry = self.current().expect("guarded current item").clone();
        self.items.retain(|item| item.key != entry.key);
        for item in &mut self.items {
            if item.raw_position > entry.raw_position {
                item.raw_position -= 1;
            }
        }
        self.order.retain(|key| key != &entry.key);
        self.current_index = self.current_index.min(self.order.len());
        let source = self.source.as_mut().expect("guarded source");
        source.snapshot_id = Some(snapshot);
        for count in [&mut source.items_count, &mut source.tracks]
            .into_iter()
            .flatten()
        {
            count.total = count.total.saturating_sub(1);
        }
        self.operation.as_mut().expect("persisted operation").stage = Stage::Complete;
    }
}
pub fn state_path(root: &Path, account: &str) -> PathBuf {
    let name: String = account.bytes().map(|byte| format!("{byte:02x}")).collect();
    root.join("rotation").join(format!("{name}.json"))
}
async fn load(path: &Path, account: &str) -> Result<State, String> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(State::new(account.into()));
        }
        Err(_) => {
            return Err("Could not read Rotation state. No playlist changes were made.".into());
        }
    };
    let state: State = serde_json::from_slice(&bytes)
        .map_err(|_| "Rotation state is damaged. Its saved recovery information has been kept.")?;
    state.validate(account)?;
    Ok(state)
}
tokio::task_local! {
    // An in-flight disk write keeps serialization after cancellation drops the async task.
    static STORAGE_HOLD: std::sync::Arc<tokio::sync::OwnedMutexGuard<()>>;
    static STORAGE_IO: std::sync::Arc<tokio::sync::Mutex<()>>;
}
async fn save(path: &Path, state: &State) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(state).map_err(|_| "Could not encode Rotation state.")?;
    let path = path.to_owned();
    let hold = STORAGE_HOLD.try_with(std::sync::Arc::clone).ok();
    let io = match STORAGE_IO.try_with(std::sync::Arc::clone).ok() {
        Some(io) => Some(io.lock_owned().await),
        None => None,
    };
    tokio::task::spawn_blocking(move || {
        let _hold = hold;
        let _io = io;
        fastframe_file::write(&path, bytes)
    })
    .await
    .map_err(|_| "Rotation storage worker failed.")?
    .map_err(|_| "Could not save Rotation state. No further playlist writes were sent.".into())
}
fn rejected(error: &ApiError) -> bool {
    matches!(
        error,
        ApiError::NotSignedIn
            | ApiError::SignInExpired { .. }
            | ApiError::RateLimited
            | ApiError::QuotaExhausted
            | ApiError::Status {
                status: 400..=499,
                ..
            }
    )
}
fn safe_error(error: ApiError) -> String {
    match error {
        ApiError::NotSignedIn | ApiError::SignInExpired { .. } => {
            "Sign in again to use Rotation.".into()
        }
        ApiError::RateLimited | ApiError::QuotaExhausted => {
            "Spotify is limiting requests. Try again later.".into()
        }
        ApiError::Status { status: 403, .. } => {
            "Spotify denied this request. Check your account and playlist permissions.".into()
        }
        _ => {
            "Spotify did not confirm the request. Check the saved recovery status before trying again.".into()
        }
    }
}
const UNCONFIRMED_PLAYBACK: &str =
    "Spotify did not confirm playback. Wait a moment, then press Play.";
fn safe_playback_error(error: ApiError) -> String {
    match error {
        ApiError::Status { status: 403, .. } => {
            "Spotify denied playback. Check Premium and the selected device.".into()
        }
        ApiError::NotSignedIn
        | ApiError::SignInExpired { .. }
        | ApiError::RateLimited
        | ApiError::QuotaExhausted => safe_error(error),
        ApiError::Status {
            status: 400..=499, ..
        } => "Spotify rejected playback. Try another available device.".into(),
        _ => UNCONFIRMED_PLAYBACK.into(),
    }
}
async fn client(
    api: &ApiGateway,
    id: &str,
    write: bool,
) -> Result<std::sync::Arc<ApiClient>, String> {
    let access = api.playlist_access(id);
    api.client_for(if write {
        ApiOperation::PlaylistMutation(access)
    } else {
        ApiOperation::PlaylistItems(access)
    })
    .await
    .map_err(safe_error)
}
async fn read_source(api: &ApiGateway, id: &str) -> Result<(Playlist, Vec<PlaylistItem>), String> {
    let api_client = client(api, id, false).await?;
    let source = api_client.playlist(id).await.map_err(safe_error)?;
    api.observe_playlist(&source);
    let api_client = client(api, id, false).await?;
    let mut rows = vec![];
    let mut offset = 0;
    let mut total = None;
    loop {
        let page = api_client
            .playlist_items(id, offset, 50)
            .await
            .map_err(safe_error)?;
        if page.offset != offset
            || total.is_some_and(|total| total != page.total)
            || page.items.len() as u32 != page.total.saturating_sub(offset).min(50)
        {
            return Err(
                "Spotify returned an inconsistent playlist page. Reload before editing.".into(),
            );
        }
        total = Some(page.total);
        let next = page.next_offset();
        if (offset + (page.items.len() as u32) < page.total) != next.is_some() {
            return Err("Spotify returned an incomplete playlist. Reload before editing.".into());
        }
        rows.extend(page.items);
        match next {
            Some(next) if next > offset => offset = next,
            Some(_) => return Err("Spotify returned an inconsistent playlist page.".into()),
            None => break,
        }
    }
    if source
        .items_count
        .as_ref()
        .or(source.tracks.as_ref())
        .is_some_and(|count| count.total as usize != rows.len())
    {
        return Err(
            "Spotify returned an incomplete source playlist. Reload before editing.".into(),
        );
    }
    let after = api_client.playlist(id).await.map_err(safe_error)?;
    if source.snapshot_id.is_none() || source.snapshot_id != after.snapshot_id {
        return Err("The playlist changed while it was loading. Try again.".into());
    }
    Ok((after, rows))
}
trait EditTransport: Sync {
    fn read(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<(Playlist, Vec<PlaylistItem>), String>> + Send;
    fn archive(
        &self,
        id: &str,
        uri: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, ApiError>> + Send;
    fn delete(
        &self,
        id: &str,
        uri: &str,
        snapshot: Option<&str>,
    ) -> impl std::future::Future<Output = Result<Option<String>, ApiError>> + Send;
}
struct GatewayEdit<'a>(&'a ApiGateway);
impl EditTransport for GatewayEdit<'_> {
    async fn read(&self, id: &str) -> Result<(Playlist, Vec<PlaylistItem>), String> {
        read_source(self.0, id).await
    }
    async fn archive(&self, id: &str, uri: &str) -> Result<Option<String>, ApiError> {
        let access = self.0.playlist_access(id);
        let client = self
            .0
            .client_for(ApiOperation::PlaylistMutation(access))
            .await?;
        client.add_playlist_items(id, &[uri.into()], None).await
    }
    async fn delete(
        &self,
        id: &str,
        uri: &str,
        snapshot: Option<&str>,
    ) -> Result<Option<String>, ApiError> {
        let access = self.0.playlist_access(id);
        let client = self
            .0
            .client_for(ApiOperation::PlaylistMutation(access))
            .await?;
        client
            .remove_playlist_items(id, &[uri.into()], snapshot)
            .await
    }
}
async fn remove<T: EditTransport>(
    api: &T,
    path: &Path,
    state: &mut State,
    key: &str,
    without_archive: bool,
    retry: bool,
) -> Result<(), String> {
    if state.current().map(|entry| entry.key.as_str()) != Some(key) {
        return Err("The current song changed. Confirm the new song before removing it.".into());
    }
    if state.unresolved() {
        let operation = state.operation.as_ref().expect("unresolved");
        if operation.item_key != key
            || !(operation.stage == Stage::Archived
                || (without_archive
                    && matches!(operation.stage, Stage::ArchiveUncertain | Stage::Pending)))
        {
            return Err("Resolve the saved playlist operation before another removal. A delete with an unknown result must be checked in Spotify before restarting.".into());
        }
    } else if retry {
        return Err("There is no archived removal to retry.".into());
    }
    let id = state
        .source
        .as_ref()
        .ok_or("Choose a source playlist.")?
        .id
        .clone();
    let (source, rows) = api.read(&id).await?;
    state.guard(&source, &rows)?;
    let archived = state
        .operation
        .as_ref()
        .is_some_and(|operation| operation.item_key == key && operation.stage == Stage::Archived);
    let uri = state.current().expect("guarded item").track.uri.clone();
    if !archived {
        state.operation = Some(Operation {
            item_key: key.into(),
            stage: Stage::Pending,
        });
        save(path, state).await?;
        if let Some(archive) = state.archive.as_ref().filter(|_| !without_archive) {
            let archive_id = archive.id.clone();
            state.operation.as_mut().expect("operation").stage = Stage::ArchiveUncertain;
            save(path, state).await?;
            match tokio::time::timeout(Duration::from_secs(30), api.archive(&archive_id, &uri))
                .await
            {
                Ok(Ok(snapshot)) => {
                    let archive = state.archive.as_mut().expect("configured archive");
                    archive.snapshot_id = snapshot;
                    for count in [&mut archive.items_count, &mut archive.tracks]
                        .into_iter()
                        .flatten()
                    {
                        count.total = count.total.saturating_add(1);
                    }
                    state.operation.as_mut().expect("operation").stage = Stage::Archived;
                    save(path, state).await?;
                }
                Ok(Err(error)) => {
                    if rejected(&error) {
                        state.operation.as_mut().expect("operation").stage = Stage::Failed;
                        save(path, state).await?;
                    }
                    return Err(safe_error(error));
                }
                Err(_) => {
                    return Err("Archive result is unknown. Check the archive in Spotify, then explicitly remove without adding again.".into());
                }
            }
        }
    }
    let (source, rows) = api.read(&id).await?;
    state.guard(&source, &rows)?;
    let previous = state.operation.as_ref().expect("operation").stage;
    state.operation.as_mut().expect("operation").stage = Stage::DeleteUncertain;
    save(path, state).await?;
    match tokio::time::timeout(Duration::from_secs(30), api.delete(&id, &uri, source.snapshot_id.as_deref())).await {
        Ok(Ok(Some(snapshot))) => {
            state.complete(snapshot);
            save(path, state).await?;
            Ok(())
        }
        Ok(Ok(None)) | Err(_) => {
            Err("Source removal result is unknown. Check the source in Spotify before restarting Rotation. The archive will not be added again.".into())
        }
        Ok(Err(error)) => {
            if rejected(&error) {
                state.operation.as_mut().expect("operation").stage = if previous == Stage::Archived {
                    Stage::Archived
                } else {
                    Stage::Failed
                };
                save(path, state).await?;
            }
            Err(safe_error(error))
        }
    }
}
trait PlayerTransport: Sync {
    fn load_devices(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<crate::api::models::Device>, ApiError>> + Send;
    fn observe(
        &self,
    ) -> impl std::future::Future<
        Output = Result<Option<crate::api::models::PlaybackState>, ApiError>,
    > + Send;
    fn start(
        &self,
        device: &str,
        uris: &[String],
    ) -> impl std::future::Future<Output = Result<(), ApiError>> + Send;
    fn resume(
        &self,
        device: &str,
    ) -> impl std::future::Future<Output = Result<(), ApiError>> + Send;
    fn stop(&self, device: &str) -> impl std::future::Future<Output = Result<(), ApiError>> + Send;
    fn jump(
        &self,
        device: &str,
        position: u32,
    ) -> impl std::future::Future<Output = Result<(), ApiError>> + Send;
    fn shuffle_off(
        &self,
        device: &str,
    ) -> impl std::future::Future<Output = Result<(), ApiError>> + Send;
    fn repeat_off(
        &self,
        device: &str,
    ) -> impl std::future::Future<Output = Result<(), ApiError>> + Send;
}
impl PlayerTransport for ApiClient {
    async fn load_devices(&self) -> Result<Vec<crate::api::models::Device>, ApiError> {
        self.rotation_devices().await
    }
    async fn observe(&self) -> Result<Option<crate::api::models::PlaybackState>, ApiError> {
        self.rotation_playback_state().await
    }
    async fn start(&self, device: &str, uris: &[String]) -> Result<(), ApiError> {
        self.play(
            Some(device),
            Some(&PlayRequest::tracks(uris.to_vec()).starting_at_index(0)),
        )
        .await
    }
    async fn resume(&self, device: &str) -> Result<(), ApiError> {
        self.play(Some(device), None).await
    }
    async fn stop(&self, device: &str) -> Result<(), ApiError> {
        self.pause(Some(device)).await
    }
    async fn jump(&self, device: &str, position: u32) -> Result<(), ApiError> {
        self.seek(position, Some(device)).await
    }
    async fn shuffle_off(&self, device: &str) -> Result<(), ApiError> {
        self.set_shuffle(false, Some(device)).await
    }
    async fn repeat_off(&self, device: &str) -> Result<(), ApiError> {
        self.set_repeat("off", Some(device)).await
    }
}
async fn playback(
    api: &ApiGateway,
    path: &Path,
    state: &mut State,
    request: &Request,
) -> Result<(), String> {
    let api_client = api
        .client_for(ApiOperation::Playback)
        .await
        .map_err(safe_playback_error)?;
    player(api_client.as_ref(), path, state, request).await
}
async fn player<T: PlayerTransport>(
    api_client: &T,
    path: &Path,
    state: &mut State,
    request: &Request,
) -> Result<(), String> {
    let device = match request {
        Request::Play { device_id, .. }
        | Request::Pause { device_id }
        | Request::Resume { device_id }
        | Request::Seek { device_id, .. } => device_id,
        _ => unreachable!(),
    };
    if device.is_empty() {
        return Err("Choose a playback device first.".into());
    }
    let devices = api_client
        .load_devices()
        .await
        .map_err(safe_playback_error)?;
    if !devices
        .iter()
        .any(|candidate| candidate.id.as_deref() == Some(device) && !candidate.is_restricted)
    {
        return Err("The selected device is unavailable or restricted. Refresh devices.".into());
    }
    match request {
        Request::Play { index, .. } => {
            let entry = state
                .ordered_items()
                .get(*index)
                .copied()
                .ok_or("Choose an available Rotation song.")?
                .clone();
            let mut verified_modes = false;
            let before = api_client.observe().await.map_err(safe_playback_error)?;
            state.playback = before.clone();
            if let Some(live) = before.as_ref().filter(|live| {
                live.device.as_ref().is_some_and(|candidate| {
                    candidate.id.as_deref() == Some(device)
                        && candidate.is_active
                        && !candidate.is_restricted
                })
            }) {
                if live.shuffle_state {
                    api_client
                        .shuffle_off(device)
                        .await
                        .map_err(safe_playback_error)?;
                }
                if live.repeat_state != "off" {
                    api_client
                        .repeat_off(device)
                        .await
                        .map_err(safe_playback_error)?;
                }
                if live.shuffle_state || live.repeat_state != "off" {
                    let modes = api_client.observe().await.map_err(safe_playback_error)?;
                    state.playback = modes.clone();
                    if !modes.as_ref().is_some_and(|live| {
                        live.device.as_ref().is_some_and(|candidate| {
                            candidate.id.as_deref() == Some(device)
                                && candidate.is_active
                                && !candidate.is_restricted
                        }) && !live.shuffle_state
                            && live.repeat_state == "off"
                    }) {
                        return Err(
                            "Spotify did not confirm Shuffle and Repeat off. Turn on Rotation again."
                                .into(),
                        );
                    }
                }
                verified_modes = true;
            }
            let max_end = index
                .saturating_add(if verified_modes { 20 } else { 1 })
                .min(state.order.len());
            // Spotify reports a URI, not a playlist occurrence. Keep the
            // submitted window unique so repeated songs can still be selected
            // deliberately with Next without making every observation ambiguous.
            let mut seen = std::collections::HashSet::new();
            let uris: Vec<_> = state.ordered_items()[*index..max_end]
                .iter()
                .map(|entry| entry.track.uri.clone())
                .take_while(|uri| seen.insert(uri.clone()))
                .collect();
            let end_index = index.saturating_add(uris.len());
            api_client
                .start(device, &uris)
                .await
                .map_err(safe_playback_error)?;
            state.current_index = *index;
            state.queued_window = Some(QueuedWindow {
                start_index: *index,
                end_index,
                device_id: device.clone(),
            });
            save(path, state).await?;
            for _ in 0..2 {
                let observed = api_client.observe().await.map_err(safe_playback_error)?;
                state.playback = observed.clone();
                if observed.as_ref().is_some_and(|live| {
                    live.device.as_ref().is_some_and(|candidate| {
                        candidate.id.as_deref() == Some(device)
                            && candidate.is_active
                            && !candidate.is_restricted
                    }) && live.item.as_ref().map(PlayableItem::uri)
                        == Some(entry.track.uri.as_str())
                        && live.is_playing
                        && !live.shuffle_state
                        && live.repeat_state == "off"
                }) {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
            Err("Play was accepted, but playback on the selected device is unconfirmed. Turn on Rotation again to try.".into())
        }
        Request::Pause { .. } | Request::Resume { .. } | Request::Seek { .. } => {
            let entry = state
                .current()
                .ok_or("There is no current Rotation song.")?
                .clone();
            let observed = api_client.observe().await.map_err(safe_playback_error)?;
            state.playback = observed.clone();
            if !observed.as_ref().is_some_and(|live| {
                live.device.as_ref().is_some_and(|candidate| {
                    candidate.id.as_deref() == Some(device)
                        && candidate.is_active
                        && !candidate.is_restricted
                }) && live.item.as_ref().map(PlayableItem::uri) == Some(entry.track.uri.as_str())
            }) {
                return Err(
                    "The selected device is playing something else. Press Rotation Play first."
                        .into(),
                );
            }
            if observed
                .as_ref()
                .is_some_and(|live| live.shuffle_state || live.repeat_state != "off")
            {
                return Err(
                    "Playback modes changed. Turn on Rotation again to restore its order.".into(),
                );
            }
            match request {
                Request::Pause { .. } | Request::Resume { .. } => {
                    let resume = matches!(request, Request::Resume { .. });
                    if observed
                        .as_ref()
                        .is_some_and(|live| live.is_playing == resume)
                    {
                        return Ok(());
                    }
                    if resume {
                        api_client
                            .resume(device)
                            .await
                            .map_err(safe_playback_error)?;
                    } else {
                        api_client.stop(device).await.map_err(safe_playback_error)?;
                    }
                    for attempt in 0..2 {
                        let observed = api_client.observe().await.map_err(safe_playback_error)?;
                        state.playback = observed.clone();
                        if observed.as_ref().is_some_and(|live| {
                            live.device.as_ref().is_some_and(|candidate| {
                                candidate.id.as_deref() == Some(device)
                                    && candidate.is_active
                                    && !candidate.is_restricted
                            }) && live.item.as_ref().map(PlayableItem::uri)
                                == Some(entry.track.uri.as_str())
                                && live.is_playing == resume
                                && !live.shuffle_state
                                && live.repeat_state == "off"
                        }) {
                            return Ok(());
                        }
                        if attempt == 0 {
                            tokio::time::sleep(Duration::from_millis(700)).await;
                        }
                    }
                    Err(if resume {
                        "Resume was accepted, but playback on the selected device is unconfirmed."
                    } else {
                        "Pause was accepted, but playback on the selected device is unconfirmed."
                    }
                    .into())
                }
                Request::Seek { position_ms, .. } if *position_ms <= entry.track.duration_ms => {
                    api_client
                        .jump(device, *position_ms)
                        .await
                        .map_err(safe_playback_error)?;
                    state.playback = api_client.observe().await.map_err(safe_playback_error)?;
                    Ok(())
                }
                _ => Err("Seek is outside this song.".into()),
            }
        }
        _ => unreachable!(),
    }
}
async fn stop_removed<T: PlayerTransport>(
    client: &T,
    state: &State,
    device: &str,
) -> Result<(), String> {
    if state.current().is_some() {
        return Err("Rotation still has a current song.".into());
    }
    let operation = state
        .operation
        .as_ref()
        .filter(|operation| operation.stage == Stage::Complete)
        .ok_or("There is no confirmed removal to stop.")?;
    let (position, uri) = operation
        .item_key
        .split_once(':')
        .ok_or("The removed song identifier is invalid.")?;
    if position.parse::<u32>().is_err() || !uri.starts_with("spotify:track:") {
        return Err("The removed song identifier is invalid.".into());
    }
    if device.is_empty() {
        return Ok(());
    }
    let devices = client.load_devices().await.map_err(safe_playback_error)?;
    if !devices
        .iter()
        .any(|candidate| candidate.id.as_deref() == Some(device) && !candidate.is_restricted)
    {
        return Ok(());
    }
    if let Some(live) = client.observe().await.map_err(safe_playback_error)?
        && live.is_playing
        && live.device.as_ref().is_some_and(|candidate| {
            candidate.id.as_deref() == Some(device)
                && candidate.is_active
                && !candidate.is_restricted
        })
        && live.item.as_ref().map(PlayableItem::uri) == Some(uri)
    {
        client.stop(device).await.map_err(safe_playback_error)?;
    }
    Ok(())
}
fn synced_index(
    state: &State,
    live: &crate::api::models::PlaybackState,
) -> Result<Option<usize>, String> {
    let Some(window) = state.queued_window.as_ref() else {
        return Ok(None);
    };
    if !live.device.as_ref().is_some_and(|device| {
        device.id.as_deref() == Some(window.device_id.as_str())
            && device.is_active
            && !device.is_restricted
    }) || live.shuffle_state
        || live.repeat_state != "off"
    {
        return Err(
            "Rotation playback changed devices or modes. Turn on Rotation again to restore its order.".into(),
        );
    }
    let Some(uri) = live.item.as_ref().map(PlayableItem::uri) else {
        return Ok(None);
    };
    if !live.is_playing {
        return Ok(None);
    }
    let ordered = state.ordered_items();
    let Some(entries) = ordered.get(window.start_index..window.end_index) else {
        return Err(
            "The saved Rotation playback window is invalid. Turn on Rotation again.".into(),
        );
    };
    let matches: Vec<_> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.track.uri == uri)
        .map(|(offset, _)| window.start_index + offset)
        .collect();
    match matches.as_slice() {
        [index] if *index>state.current_index=>Ok(Some(*index)),
        [_]=>Ok(None),
        []=>Err("The selected device is playing outside Rotation. Turn on Rotation again to restore its order.".into()),
        _=>Err("This song appears more than once in the playback window. Turn on Rotation again to choose its saved occurrence.".into()),
    }
}
async fn sync(api: &ApiGateway, path: &Path, state: &mut State) -> Result<(), String> {
    if state.queued_window.is_none() {
        return Ok(());
    }
    let client = api
        .client_for(ApiOperation::Playback)
        .await
        .map_err(safe_playback_error)?;
    state.playback = client
        .rotation_playback_state()
        .await
        .map_err(safe_playback_error)?;
    if let Some(live) = state.playback.as_ref()
        && let Some(index) = synced_index(state, live)?
    {
        state.current_index = index;
        save(path, state).await?;
    }
    Ok(())
}
fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
/// The caller serializes this function and cancels it when its account generation changes.
/// Cancellation leaves a durable uncertainty marker before any ambiguous write.
pub async fn execute(
    api: &ApiGateway,
    root: &Path,
    account: &str,
    request: Request,
    hold: std::sync::Arc<tokio::sync::OwnedMutexGuard<()>>,
) -> Result<State, String> {
    STORAGE_IO
        .scope(
            std::sync::Arc::default(),
            STORAGE_HOLD.scope(hold, execute_inner(api, root, account, request)),
        )
        .await
}
async fn execute_inner(
    api: &ApiGateway,
    root: &Path,
    account: &str,
    request: Request,
) -> Result<State, String> {
    if api.account().as_ref().map(|id| id.as_str()) != Some(account) {
        return Err("The Spotify account changed. Open Rotation again.".into());
    }
    let path = state_path(root, account);
    let mut state = load(&path, account).await?;
    state.error = None;
    let playback_request = request.is_transport();
    if playback_request && state.playback_cooldown_until > now_seconds() {
        state.error = Some(
            "Playback is cooling down after an uncertain request. Try again in a few seconds."
                .into(),
        );
        return Ok(state);
    }
    let result = async {
        match request {
            Request::Load => Ok(()),
            Request::Sync => {
                if state.unresolved() { return Ok(()); }
                tokio::time::timeout(Duration::from_secs(10), sync(api,&path,&mut state)).await.unwrap_or_else(|_|Err("Playback observation timed out. Wait a moment before trying again.".into()))
            },
            Request::Configure { source, archive } => {
                if state.unresolved() {
                    return Err("Resolve the saved removal before changing playlists.".into());
                }
                if archive
                    .as_ref()
                    .is_some_and(|archive| archive.id == source.id)
                {
                    return Err("Choose a different archive playlist.".into());
                }
                let archive = if let Some(archive) = &archive {
                    let header = client(api, &archive.id, false)
                        .await?
                        .playlist(&archive.id)
                        .await
                        .map_err(safe_error)?;
                    api.observe_playlist(&header);
                    Some(header)
                } else { None };
                let (source, rows) = read_source(api, &source.id).await?;
                state.archive = archive;
                state.replace(source, &rows);
                save(&path, &state).await
            }
            reload @ (Request::Restart | Request::ResolveReload) => {
                if matches!(reload, Request::Restart) && state.unresolved() { return Err("Check the source and archive in Spotify, then choose the explicit recovery reload.".into()); }
                let id = state
                    .source
                    .as_ref()
                    .ok_or("Choose a source playlist first.")?
                    .id
                    .clone();
                let (source, rows) = read_source(api, &id).await?;
                state.replace(source, &rows);
                save(&path, &state).await
            }
            Request::StopRemoved { device_id } => {
                tokio::time::timeout(Duration::from_secs(10),async {
                    let client=api.client_for(ApiOperation::Playback).await.map_err(safe_playback_error)?;
                    stop_removed(client.as_ref(),&state,&device_id).await
                }).await.unwrap_or_else(|_|Err("Playback timed out. Wait a moment before trying again.".into()))
            }
            Request::Finish { device_id } => {
                if state.unresolved() { return Err("Resolve the saved removal before changing songs.".into()); }
                if let Some(device_id) = device_id {
                    tokio::time::timeout(Duration::from_secs(10),playback(api, &path, &mut state, &Request::Pause { device_id })).await.unwrap_or_else(|_|Err("Playback timed out. Wait a moment before trying again.".into()))?;
                }
                state.current_index = state.order.len();
                state.queued_window = None; save(&path, &state).await
            }
            Request::Select { index } => {
                if state.unresolved() {
                    return Err("Resolve the saved removal before changing songs.".into());
                }
                if index >= state.order.len() {
                    return Err("There is no song at this position.".into());
                }
                state.current_index = index;
                state.queued_window = None;
                save(&path, &state).await
            }
            Request::Remove {
                item_key,
                without_archive,
            } => remove(&GatewayEdit(api), &path, &mut state, &item_key, without_archive, false).await,
            Request::Retry => {
                let key = state
                    .operation
                    .as_ref()
                    .filter(|operation| operation.stage == Stage::Archived)
                    .ok_or("There is no archived removal to retry.")?
                    .item_key
                    .clone();
                remove(&GatewayEdit(api), &path, &mut state, &key, true, true).await
            }
            request @ Request::Play { .. }
            | request @ Request::Pause { .. }
            | request @ Request::Resume { .. }
            | request @ Request::Seek { .. } => {
                if state.unresolved() {
                    return Err(
                        "Resolve the saved removal before controlling Rotation playback.".into(),
                    );
                }
                tokio::time::timeout(
                    Duration::from_secs(10),
                    playback(api, &path, &mut state, &request),
                )
                .await
                .unwrap_or_else(|_| {
                    Err("Playback timed out. Wait a moment before trying again.".into())
                })
            }
        }
    }
    .await;
    state.error = result.err();
    if playback_request
        && state.error.as_ref().is_some_and(|error| {
            error.contains("timed out")
                || error.contains("unconfirmed")
                || error == UNCONFIRMED_PLAYBACK
        })
    {
        state.playback_cooldown_until = now_seconds().saturating_add(10);
        if let Err(error) = save(&path, &state).await {
            state.error = Some(error);
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> Playlist {
        Playlist {
            id: "source".into(),
            snapshot_id: Some("snapshot".into()),
            ..Playlist::default()
        }
    }
    fn row(uri: &str, playable: bool) -> PlaylistItem {
        PlaylistItem {
            item: Some(PlayableItem::Track(Track {
                uri: uri.into(),
                is_playable: Some(playable),
                ..Track::default()
            })),
            ..PlaylistItem::default()
        }
    }
    fn state(rows: &[PlaylistItem]) -> State {
        let mut state = State::new("listener".into());
        state.replace(source(), rows);
        state.order = state.items.iter().map(|entry| entry.key.clone()).collect();
        state
    }
    #[test]
    fn filtered_and_null_slots_keep_raw_positions_and_duplicate_guards() {
        let rows = vec![
            PlaylistItem::default(),
            row("spotify:track:a", true),
            row("spotify:track:a", false),
            row("spotify:track:b", true),
        ];
        let state = state(&rows);
        assert_eq!(state.items.len(), 2);
        assert_eq!(state.items[0].raw_position, 1);
        assert_eq!(state.items[1].raw_position, 3);
        assert!(
            state
                .guard(&source(), &rows)
                .unwrap_err()
                .contains("more than once")
        );
    }
    #[test]
    fn malformed_and_restricted_tracks_are_filtered_without_losing_raw_positions() {
        let mut restricted = row("spotify:track:restricted", true);
        if let Some(PlayableItem::Track(track)) = &mut restricted.item {
            track.restrictions = Some(serde_json::json!({"reason":"market"}));
        }
        let rows = vec![
            row("spotify:track:", true),
            row("spotify:track:bad:id", true),
            row("spotify:track:bad id", true),
            restricted,
            row("spotify:track:good", true),
        ];
        let state = state(&rows);
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.current().unwrap().raw_position, 4);
        assert_eq!(state.current().unwrap().track.uri, "spotify:track:good");
    }

    #[test]
    fn guard_blocks_snapshot_and_position_changes() {
        let rows = vec![row("spotify:track:a", true), row("spotify:track:b", true)];
        let state = state(&rows);
        let mut changed = source();
        changed.snapshot_id = Some("new".into());
        assert!(
            state
                .guard(&changed, &rows)
                .unwrap_err()
                .contains("changed")
        );
        let moved = vec![rows[1].clone(), rows[0].clone()];
        assert!(
            state
                .guard(&source(), &moved)
                .unwrap_err()
                .contains("moved")
        );
    }
    #[test]
    fn occurrences_are_distinct_and_stable_after_serialization() {
        let rows = vec![
            row("spotify:track:a", true),
            row("spotify:track:a", true),
            row("spotify:track:b", true),
        ];
        let mut original = State::new("listener".into());
        original.replace(source(), &rows);
        original.current_index = 1;
        let restored: State =
            serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
        assert_eq!(restored, original);
        assert_ne!(original.items[0].key, original.items[1].key);
        assert_eq!(
            restored.current().unwrap().key,
            original.current().unwrap().key
        );
    }
    #[test]
    fn completing_removal_shifts_raw_positions_and_keeps_next_slot() {
        let mut state = state(&[
            row("spotify:track:a", true),
            PlaylistItem::default(),
            row("spotify:track:b", true),
        ]);
        let key = state.current().unwrap().key.clone();
        state.operation = Some(Operation {
            item_key: key,
            stage: Stage::DeleteUncertain,
        });
        state.complete("next-snapshot".into());
        assert_eq!(state.current().unwrap().track.uri, "spotify:track:b");
        assert_eq!(state.current().unwrap().raw_position, 1);
        assert_eq!(
            state.source.unwrap().snapshot_id.as_deref(),
            Some("next-snapshot")
        );
        assert_eq!(state.operation.unwrap().stage, Stage::Complete);
    }
    #[test]
    fn removing_terminal_song_does_not_jump_to_previous_song() {
        let mut state = state(&[row("spotify:track:a", true), row("spotify:track:b", true)]);
        state.current_index = 1;
        state.operation = Some(Operation {
            item_key: state.current().unwrap().key.clone(),
            stage: Stage::DeleteUncertain,
        });
        state.complete("updated".into());
        assert!(state.current().is_none());
        state.validate("listener").unwrap();
    }
    #[test]
    fn empty_playlist_and_same_archive_validation() {
        let mut state = state(&[]);
        assert!(state.current().is_none());
        state.validate("listener").unwrap();
        state.archive = Some(source());
        assert!(state.validate("listener").is_err());
    }
    #[test]
    fn network_and_decode_errors_are_never_definite_rejections() {
        assert!(!rejected(&ApiError::Network("timeout".into())));
        assert!(!rejected(&ApiError::Decode("bad body".into())));
        assert!(!rejected(&ApiError::Status {
            status: 503,
            message: "unavailable".into()
        }));
        assert!(rejected(&ApiError::Status {
            status: 403,
            message: "forbidden".into()
        }));
    }
    #[tokio::test]
    async fn every_operation_stage_survives_restart_without_replaying_a_write() {
        let root = std::env::current_dir()
            .unwrap()
            .join(".cache")
            .join(format!("rotation-test-{}", std::process::id()));
        let path = state_path(&root, "listener");
        let mut state = state(&[row("spotify:track:a", true)]);
        for stage in [
            Stage::Pending,
            Stage::ArchiveUncertain,
            Stage::Archived,
            Stage::DeleteUncertain,
            Stage::Failed,
            Stage::Complete,
        ] {
            state.operation = Some(Operation {
                item_key: state.current().unwrap().key.clone(),
                stage,
            });
            save(&path, &state).await.unwrap();
            let restored = load(&path, "listener").await.unwrap();
            assert_eq!(restored, state);
            assert!(load(&path, "other-account").await.is_err());
        }
        assert!(state_path(&root, "../other").starts_with(root.join("rotation")));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
    struct FakeEdit {
        path: PathBuf,
        reads: std::sync::Mutex<std::collections::VecDeque<(Playlist, Vec<PlaylistItem>)>>,
        calls: std::sync::Mutex<Vec<Stage>>,
        archive_result: std::sync::Mutex<Option<Result<(), ApiError>>>,
        delete_result: std::sync::Mutex<Option<Result<Option<String>, ApiError>>>,
    }
    impl EditTransport for FakeEdit {
        async fn read(&self, _: &str) -> Result<(Playlist, Vec<PlaylistItem>), String> {
            Ok(self
                .reads
                .lock()
                .unwrap()
                .pop_front()
                .expect("expected read"))
        }
        async fn archive(&self, _: &str, _: &str) -> Result<Option<String>, ApiError> {
            let saved = load(&self.path, "listener").await.unwrap();
            let stage = saved.operation.unwrap().stage;
            assert_eq!(stage, Stage::ArchiveUncertain);
            self.calls.lock().unwrap().push(stage);
            self.archive_result
                .lock()
                .unwrap()
                .take()
                .expect("archive result")
                .map(|_| Some("archive-snapshot".into()))
        }
        async fn delete(
            &self,
            _: &str,
            _: &str,
            snapshot: Option<&str>,
        ) -> Result<Option<String>, ApiError> {
            assert_eq!(snapshot, Some("snapshot"));
            let saved = load(&self.path, "listener").await.unwrap();
            let stage = saved.operation.unwrap().stage;
            assert_eq!(stage, Stage::DeleteUncertain);
            self.calls.lock().unwrap().push(stage);
            self.delete_result
                .lock()
                .unwrap()
                .take()
                .expect("delete result")
        }
    }
    fn fake(
        name: &str,
        rows: Vec<PlaylistItem>,
        archive_result: Result<(), ApiError>,
        delete_result: Result<Option<String>, ApiError>,
    ) -> FakeEdit {
        let root = std::env::current_dir()
            .unwrap()
            .join(".cache")
            .join(format!("rotation-{name}-{}", std::process::id()));
        FakeEdit {
            path: state_path(&root, "listener"),
            reads: std::sync::Mutex::new(std::collections::VecDeque::from([
                (source(), rows.clone()),
                (source(), rows),
            ])),
            calls: std::sync::Mutex::default(),
            archive_result: std::sync::Mutex::new(Some(archive_result)),
            delete_result: std::sync::Mutex::new(Some(delete_result)),
        }
    }
    async fn cleanup(fake: &FakeEdit) {
        tokio::fs::remove_dir_all(fake.path.parent().unwrap().parent().unwrap())
            .await
            .unwrap();
    }
    fn with_archive(rows: &[PlaylistItem]) -> State {
        let mut state = state(rows);
        state.archive = Some(Playlist {
            id: "archive".into(),
            ..Playlist::default()
        });
        state
    }
    #[tokio::test]
    async fn archive_then_delete_writes_durable_markers_before_network() {
        let rows = vec![row("spotify:track:a", true)];
        let fake = fake("complete", rows.clone(), Ok(()), Ok(Some("updated".into())));
        let mut state = with_archive(&rows);
        let key = state.current().unwrap().key.clone();
        remove(&fake, &fake.path, &mut state, &key, false, false)
            .await
            .unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![Stage::ArchiveUncertain, Stage::DeleteUncertain]
        );
        assert_eq!(
            load(&fake.path, "listener")
                .await
                .unwrap()
                .operation
                .unwrap()
                .stage,
            Stage::Complete
        );
        assert!(state.current().is_none());
        cleanup(&fake).await;
    }
    #[tokio::test]
    async fn partial_delete_failure_retries_only_source_without_second_archive() {
        let rows = vec![row("spotify:track:a", true)];
        let fake = fake(
            "partial",
            rows.clone(),
            Ok(()),
            Err(ApiError::Status {
                status: 403,
                message: "denied".into(),
            }),
        );
        let mut state = with_archive(&rows);
        let key = state.current().unwrap().key.clone();
        assert!(
            remove(&fake, &fake.path, &mut state, &key, false, false)
                .await
                .is_err()
        );
        assert_eq!(state.operation.as_ref().unwrap().stage, Stage::Archived);
        assert!(state.current().is_some());
        *fake.delete_result.lock().unwrap() = Some(Ok(Some("updated".into())));
        fake.reads
            .lock()
            .unwrap()
            .extend([(source(), rows.clone()), (source(), rows)]);
        remove(&fake, &fake.path, &mut state, &key, true, true)
            .await
            .unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![
                Stage::ArchiveUncertain,
                Stage::DeleteUncertain,
                Stage::DeleteUncertain
            ]
        );
        cleanup(&fake).await;
    }
    #[tokio::test]
    async fn ambiguous_archive_cannot_be_replayed_but_explicit_source_only_is_allowed() {
        let rows = vec![row("spotify:track:a", true)];
        let fake = fake(
            "uncertain",
            rows.clone(),
            Err(ApiError::Network("timeout".into())),
            Ok(Some("updated".into())),
        );
        let mut state = with_archive(&rows);
        let key = state.current().unwrap().key.clone();
        assert!(
            remove(&fake, &fake.path, &mut state, &key, false, false)
                .await
                .is_err()
        );
        assert_eq!(
            state.operation.as_ref().unwrap().stage,
            Stage::ArchiveUncertain
        );
        assert!(
            remove(&fake, &fake.path, &mut state, &key, false, false)
                .await
                .is_err()
        );
        fake.reads.lock().unwrap().push_back((source(), rows));
        remove(&fake, &fake.path, &mut state, &key, true, false)
            .await
            .unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![Stage::ArchiveUncertain, Stage::DeleteUncertain]
        );
        cleanup(&fake).await;
    }
    #[tokio::test]
    async fn definite_archive_rejection_never_deletes_source() {
        let rows = vec![row("spotify:track:a", true)];
        let fake = fake(
            "rejected",
            rows.clone(),
            Err(ApiError::Status {
                status: 403,
                message: "denied".into(),
            }),
            Ok(Some("unused".into())),
        );
        let mut state = with_archive(&rows);
        let key = state.current().unwrap().key.clone();
        assert!(
            remove(&fake, &fake.path, &mut state, &key, false, false)
                .await
                .is_err()
        );
        assert_eq!(state.operation.unwrap().stage, Stage::Failed);
        assert_eq!(*fake.calls.lock().unwrap(), vec![Stage::ArchiveUncertain]);
        cleanup(&fake).await;
    }
    #[tokio::test]
    async fn duplicates_and_changed_snapshot_never_send_any_playlist_write() {
        let rows = vec![row("spotify:track:a", true), row("spotify:track:a", false)];
        let fake = fake("blocked", rows.clone(), Ok(()), Ok(Some("unused".into())));
        let mut state = with_archive(&rows);
        let key = state.current().unwrap().key.clone();
        assert!(
            remove(&fake, &fake.path, &mut state, &key, false, false)
                .await
                .is_err()
        );
        assert!(fake.calls.lock().unwrap().is_empty());
        assert!(!fake.path.exists());
        state.source.as_mut().unwrap().snapshot_id = Some("stale".into());
        assert!(
            remove(&fake, &fake.path, &mut state, &key, false, false)
                .await
                .is_err()
        );
        assert!(fake.calls.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn uncertain_delete_is_not_replayed_after_restart() {
        let rows = vec![row("spotify:track:a", true)];
        let fake = fake(
            "delete-uncertain",
            rows.clone(),
            Ok(()),
            Err(ApiError::Decode("missing snapshot".into())),
        );
        let mut state = with_archive(&rows);
        let key = state.current().unwrap().key.clone();
        assert!(
            remove(&fake, &fake.path, &mut state, &key, false, false)
                .await
                .is_err()
        );
        let mut restored = load(&fake.path, "listener").await.unwrap();
        assert_eq!(
            restored.operation.as_ref().unwrap().stage,
            Stage::DeleteUncertain
        );
        assert!(
            remove(&fake, &fake.path, &mut restored, &key, true, true)
                .await
                .is_err()
        );
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
        cleanup(&fake).await;
    }
    struct FakePlayer {
        submitted: std::sync::Mutex<Vec<Vec<String>>>,
        devices: Vec<crate::api::models::Device>,
        observed:
            std::sync::Mutex<std::collections::VecDeque<Option<crate::api::models::PlaybackState>>>,
        calls: std::sync::Mutex<Vec<String>>,
    }
    impl PlayerTransport for FakePlayer {
        async fn load_devices(&self) -> Result<Vec<crate::api::models::Device>, ApiError> {
            Ok(self.devices.clone())
        }
        async fn observe(&self) -> Result<Option<crate::api::models::PlaybackState>, ApiError> {
            Ok(self
                .observed
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default())
        }
        async fn start(&self, device: &str, uris: &[String]) -> Result<(), ApiError> {
            self.submitted.lock().unwrap().push(uris.to_vec());
            self.calls.lock().unwrap().push(format!("play:{device}"));
            Ok(())
        }
        async fn resume(&self, device: &str) -> Result<(), ApiError> {
            self.calls.lock().unwrap().push(format!("resume:{device}"));
            Ok(())
        }
        async fn stop(&self, device: &str) -> Result<(), ApiError> {
            self.calls.lock().unwrap().push(format!("pause:{device}"));
            Ok(())
        }
        async fn jump(&self, device: &str, _: u32) -> Result<(), ApiError> {
            self.calls.lock().unwrap().push(format!("seek:{device}"));
            Ok(())
        }
        async fn shuffle_off(&self, device: &str) -> Result<(), ApiError> {
            self.calls.lock().unwrap().push(format!("shuffle:{device}"));
            Ok(())
        }
        async fn repeat_off(&self, device: &str) -> Result<(), ApiError> {
            self.calls.lock().unwrap().push(format!("repeat:{device}"));
            Ok(())
        }
    }
    fn live(device: &str, uri: &str) -> crate::api::models::PlaybackState {
        crate::api::models::PlaybackState {
            device: Some(crate::api::models::Device {
                id: Some(device.into()),
                is_active: true,
                ..Default::default()
            }),
            repeat_state: "off".into(),
            is_playing: true,
            item: Some(PlayableItem::Track(Track {
                uri: uri.into(),
                ..Default::default()
            })),
            ..Default::default()
        }
    }
    fn fake_player(
        restricted: bool,
        observed: Vec<Option<crate::api::models::PlaybackState>>,
    ) -> FakePlayer {
        FakePlayer {
            submitted: std::sync::Mutex::default(),
            devices: vec![crate::api::models::Device {
                id: Some("chosen".into()),
                is_restricted: restricted,
                ..Default::default()
            }],
            observed: std::sync::Mutex::new(observed.into()),
            calls: std::sync::Mutex::default(),
        }
    }
    fn playback_path(name: &str) -> PathBuf {
        state_path(
            &std::env::current_dir()
                .unwrap()
                .join(".cache")
                .join(format!("rotation-player-{name}-{}", std::process::id())),
            "listener",
        )
    }
    async fn clean_playback(path: &Path) {
        tokio::fs::remove_dir_all(path.parent().unwrap().parent().unwrap())
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn missing_or_restricted_device_never_receives_a_command() {
        let mut state = state(&[row("spotify:track:a", true)]);
        let path = playback_path("blocked");
        for (device, restricted) in [("vanished", false), ("chosen", true), ("", false)] {
            let fake = fake_player(restricted, vec![]);
            assert!(
                player(
                    &fake,
                    &path,
                    &mut state,
                    &Request::Play {
                        index: 0,
                        device_id: device.into()
                    }
                )
                .await
                .is_err()
            );
            assert!(fake.calls.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn mismatched_live_track_or_device_blocks_pause_and_seek() {
        let mut state = state(&[row("spotify:track:a", true)]);
        let path = playback_path("mismatch");
        for observed in [
            live("other", "spotify:track:a"),
            live("chosen", "spotify:track:other"),
        ] {
            let fake = fake_player(false, vec![Some(observed)]);
            assert!(
                player(
                    &fake,
                    &path,
                    &mut state,
                    &Request::Pause {
                        device_id: "chosen".into()
                    }
                )
                .await
                .is_err()
            );
            assert!(fake.calls.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn resume_preserves_the_selected_occurrence_and_playback_window() {
        let mut state = state(&[row("spotify:track:a", true), row("spotify:track:b", true)]);
        state.current_index = 1;
        state.queued_window = Some(QueuedWindow {
            start_index: 0,
            end_index: 2,
            device_id: "chosen".into(),
        });
        let window = state.queued_window.clone();
        let uri = state.current().unwrap().track.uri.clone();
        let playing = live("chosen", &uri);
        let mut paused = playing.clone();
        paused.is_playing = false;
        paused.progress_ms = Some(42_000);
        let fake = fake_player(false, vec![Some(paused), Some(playing.clone())]);
        player(
            &fake,
            &playback_path("resume"),
            &mut state,
            &Request::Resume {
                device_id: "chosen".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(*fake.calls.lock().unwrap(), vec!["resume:chosen"]);
        assert!(fake.submitted.lock().unwrap().is_empty());
        assert_eq!(state.current_index, 1);
        assert_eq!(state.queued_window, window);
        assert_eq!(state.playback, Some(playing));
        let encoded = serde_json::to_value(&state).unwrap();
        assert!(encoded.get("playback").is_none());
    }
    #[tokio::test]
    async fn resume_refuses_mismatched_devices_tracks_and_modes_and_noops_when_playing() {
        let mut state = state(&[row("spotify:track:a", true)]);
        let uri = state.current().unwrap().track.uri.clone();
        let mut inactive = live("chosen", &uri);
        inactive.device.as_mut().unwrap().is_active = false;
        let mut restricted = live("chosen", &uri);
        restricted.device.as_mut().unwrap().is_restricted = true;
        let mut shuffle = live("chosen", &uri);
        shuffle.shuffle_state = true;
        let mut repeat = live("chosen", &uri);
        repeat.repeat_state = "track".into();
        for observed in [
            None,
            Some(live("other", &uri)),
            Some(live("chosen", "spotify:track:other")),
            Some(inactive),
            Some(restricted),
            Some(shuffle),
            Some(repeat),
        ] {
            let fake = fake_player(false, vec![observed]);
            assert!(
                player(
                    &fake,
                    &playback_path("resume-guard"),
                    &mut state,
                    &Request::Resume {
                        device_id: "chosen".into(),
                    }
                )
                .await
                .is_err()
            );
            assert!(fake.calls.lock().unwrap().is_empty());
            assert!(fake.submitted.lock().unwrap().is_empty());
        }
        let fake = fake_player(false, vec![Some(live("chosen", &uri))]);
        player(
            &fake,
            &playback_path("resume-noop"),
            &mut state,
            &Request::Resume {
                device_id: "chosen".into(),
            },
        )
        .await
        .unwrap();
        assert!(fake.calls.lock().unwrap().is_empty());
        assert!(fake.submitted.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn pause_and_resume_report_only_observed_playback() {
        let mut state = state(&[row("spotify:track:a", true)]);
        let uri = state.current().unwrap().track.uri.clone();
        let playing = live("chosen", &uri);
        let mut paused = playing.clone();
        paused.is_playing = false;
        let fake = fake_player(
            false,
            vec![
                Some(playing.clone()),
                Some(playing.clone()),
                Some(playing.clone()),
            ],
        );
        assert!(
            player(
                &fake,
                &playback_path("pause-unconfirmed"),
                &mut state,
                &Request::Pause {
                    device_id: "chosen".into(),
                }
            )
            .await
            .unwrap_err()
            .contains("unconfirmed")
        );
        assert_eq!(state.playback, Some(playing.clone()));
        let fake = fake_player(false, vec![Some(playing), Some(paused.clone())]);
        player(
            &fake,
            &playback_path("pause-confirmed"),
            &mut state,
            &Request::Pause {
                device_id: "chosen".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(state.playback, Some(paused.clone()));
        let fake = fake_player(
            false,
            vec![
                Some(paused.clone()),
                Some(live("other", &uri)),
                Some(paused.clone()),
            ],
        );
        assert!(
            player(
                &fake,
                &playback_path("resume-unconfirmed"),
                &mut state,
                &Request::Resume {
                    device_id: "chosen".into(),
                }
            )
            .await
            .unwrap_err()
            .contains("unconfirmed")
        );
        assert_eq!(state.playback, Some(paused));
        assert!(fake.submitted.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn explicit_single_song_play_confirms_target_and_normalizes_reported_modes() {
        let mut state = state(&[row("spotify:track:a", true)]);
        let path = playback_path("confirmed");
        let safe = live("chosen", "spotify:track:a");
        let mut unsafe_modes = safe.clone();
        unsafe_modes.shuffle_state = true;
        unsafe_modes.repeat_state = "context".into();
        let fake = fake_player(
            false,
            vec![Some(unsafe_modes), Some(safe.clone()), Some(safe)],
        );
        player(
            &fake,
            &path,
            &mut state,
            &Request::Play {
                index: 0,
                device_id: "chosen".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec!["shuffle:chosen", "repeat:chosen", "play:chosen"]
        );
        clean_playback(&path).await;
    }
    #[tokio::test]
    async fn play_acceptance_on_wrong_device_is_unconfirmed_and_never_transfers() {
        let mut state = state(&[row("spotify:track:a", true), row("spotify:track:b", true)]);
        let path = playback_path("unconfirmed");
        let fake = fake_player(
            false,
            vec![None, Some(live("other", "spotify:track:b")), None],
        );
        assert!(
            player(
                &fake,
                &path,
                &mut state,
                &Request::Play {
                    index: 1,
                    device_id: "chosen".into()
                }
            )
            .await
            .unwrap_err()
            .contains("unconfirmed")
        );
        assert_eq!(state.current_index, 1);
        assert_eq!(*fake.calls.lock().unwrap(), vec!["play:chosen"]);
        clean_playback(&path).await;
    }
    #[tokio::test]
    async fn shuffle_drift_and_out_of_bounds_seek_send_no_mutation() {
        let mut state = state(&[row("spotify:track:a", true)]);
        let path = playback_path("seek");
        let mut drift = live("chosen", "spotify:track:a");
        drift.shuffle_state = true;
        for observed in [drift, live("chosen", "spotify:track:a")] {
            let fake = fake_player(false, vec![Some(observed)]);
            assert!(
                player(
                    &fake,
                    &path,
                    &mut state,
                    &Request::Seek {
                        device_id: "chosen".into(),
                        position_ms: 10
                    }
                )
                .await
                .is_err()
            );
            assert!(fake.calls.lock().unwrap().is_empty());
        }
    }
    #[tokio::test]
    async fn concurrent_source_edit_after_archive_blocks_delete_and_keeps_recovery() {
        let rows = vec![row("spotify:track:a", true)];
        let fake = fake(
            "concurrent",
            rows.clone(),
            Ok(()),
            Ok(Some("unused".into())),
        );
        fake.reads.lock().unwrap().back_mut().unwrap().0.snapshot_id =
            Some("external-change".into());
        let mut state = with_archive(&rows);
        let key = state.current().unwrap().key.clone();
        assert!(
            remove(&fake, &fake.path, &mut state, &key, false, false)
                .await
                .is_err()
        );
        assert_eq!(state.operation.as_ref().unwrap().stage, Stage::Archived);
        assert_eq!(*fake.calls.lock().unwrap(), vec![Stage::ArchiveUncertain]);
        cleanup(&fake).await;
    }
    #[tokio::test]
    async fn failed_durable_marker_prevents_every_network_write() {
        let rows = vec![row("spotify:track:a", true)];
        let fake = fake(
            "disk-failure",
            rows.clone(),
            Ok(()),
            Ok(Some("unused".into())),
        );
        tokio::fs::create_dir_all(&fake.path).await.unwrap();
        let mut state = with_archive(&rows);
        let key = state.current().unwrap().key.clone();
        assert!(
            remove(&fake, &fake.path, &mut state, &key, false, false)
                .await
                .is_err()
        );
        assert!(fake.calls.lock().unwrap().is_empty());
        cleanup(&fake).await;
    }
    #[test]
    fn sync_advances_only_unique_uri_in_window_and_never_rolls_back() {
        let mut state = state(&[
            row("spotify:track:a", true),
            row("spotify:track:b", true),
            row("spotify:track:c", true),
        ]);
        state.queued_window = Some(QueuedWindow {
            start_index: 0,
            end_index: 2,
            device_id: "chosen".into(),
        });
        assert_eq!(
            synced_index(&state, &live("chosen", "spotify:track:b")).unwrap(),
            Some(1)
        );
        state.current_index = 1;
        assert_eq!(
            synced_index(&state, &live("chosen", "spotify:track:a")).unwrap(),
            None
        );
        assert!(synced_index(&state, &live("chosen", "spotify:track:c")).is_err());
        assert!(synced_index(&state, &live("other", "spotify:track:b")).is_err());
        let mut drift = live("chosen", "spotify:track:b");
        drift.repeat_state = "track".into();
        assert!(synced_index(&state, &drift).is_err());
    }
    #[test]
    fn repeated_uri_window_is_never_guessed_from_live_track() {
        let mut state = state(&[row("spotify:track:a", true), row("spotify:track:a", true)]);
        state.queued_window = Some(QueuedWindow {
            start_index: 0,
            end_index: 2,
            device_id: "chosen".into(),
        });
        assert!(
            synced_index(&state, &live("chosen", "spotify:track:a"))
                .unwrap_err()
                .contains("more than once")
        );
    }
    #[tokio::test]
    async fn play_window_is_twenty_ordered_occurrences_and_provisional_is_one() {
        let rows: Vec<_> = (0..25)
            .map(|index| row(&format!("spotify:track:{index}"), true))
            .collect();
        for (name, observed, expected_end) in [
            ("window", Some(live("chosen", "spotify:track:0")), 23),
            ("provisional", None, 4),
        ] {
            let mut state = state(&rows);
            let path = playback_path(name);
            let fake = fake_player(
                false,
                vec![observed, Some(live("chosen", "spotify:track:3"))],
            );
            player(
                &fake,
                &path,
                &mut state,
                &Request::Play {
                    index: 3,
                    device_id: "chosen".into(),
                },
            )
            .await
            .unwrap();
            let window = state.queued_window.as_ref().unwrap();
            assert_eq!(window.start_index, 3);
            assert_eq!(window.end_index, expected_end);
            assert_eq!(
                fake.submitted.lock().unwrap()[0],
                (3..expected_end)
                    .map(|index| format!("spotify:track:{index}"))
                    .collect::<Vec<_>>()
            );
            assert_eq!(state.current().unwrap().track.uri, "spotify:track:3");
            clean_playback(&path).await;
        }
    }
    #[tokio::test]
    async fn repeated_occurrences_stay_navigable_without_ambiguous_play_windows() {
        let rows = vec![
            row("spotify:track:a", true),
            row("spotify:track:b", true),
            row("spotify:track:a", true),
            row("spotify:track:c", true),
        ];
        let mut state = state(&rows);
        let path = playback_path("unique-occurrence-window");
        for (index, expected, end) in [
            (0, vec!["spotify:track:a", "spotify:track:b"], 2),
            (2, vec!["spotify:track:a", "spotify:track:c"], 4),
        ] {
            let fake = fake_player(
                false,
                vec![
                    Some(live("chosen", "spotify:track:a")),
                    Some(live("chosen", "spotify:track:a")),
                ],
            );
            player(
                &fake,
                &path,
                &mut state,
                &Request::Play {
                    index,
                    device_id: "chosen".into(),
                },
            )
            .await
            .unwrap();
            assert_eq!(fake.submitted.lock().unwrap()[0], expected);
            assert_eq!(state.queued_window.as_ref().unwrap().end_index, end);
            assert_eq!(
                synced_index(&state, &live("chosen", "spotify:track:a")).unwrap(),
                None
            );
            assert_eq!(
                state.order.len(),
                4,
                "both occurrences remain in the saved order"
            );
        }
        assert_eq!(
            synced_index(&state, &live("chosen", "spotify:track:c")).unwrap(),
            Some(3)
        );
        clean_playback(&path).await;
    }
    #[test]
    fn restarting_and_removing_invalidate_old_playback_windows() {
        let rows = vec![row("spotify:track:a", true), row("spotify:track:b", true)];
        let mut state = state(&rows);
        state.queued_window = Some(QueuedWindow {
            start_index: 0,
            end_index: 2,
            device_id: "chosen".into(),
        });
        state.replace(source(), &rows);
        assert!(state.queued_window.is_none());
        state.queued_window = Some(QueuedWindow {
            start_index: 0,
            end_index: 2,
            device_id: "chosen".into(),
        });
        state.operation = Some(Operation {
            item_key: state.current().unwrap().key.clone(),
            stage: Stage::DeleteUncertain,
        });
        state.complete("updated".into());
        assert!(state.queued_window.is_none());
    }
    #[test]
    fn deliberate_restart_avoids_prior_song_or_occurrence_when_possible() {
        for rows in [
            vec![row("spotify:track:a", true), row("spotify:track:b", true)],
            vec![row("spotify:track:a", true), row("spotify:track:a", true)],
        ] {
            let mut state = state(&rows);
            for _ in 0..20 {
                let previous = state.current().unwrap().key.clone();
                state.replace(source(), &rows);
                assert_ne!(state.current().unwrap().key, previous);
            }
        }
    }
    #[tokio::test]
    async fn terminal_removed_song_is_stopped_only_on_matching_explicit_device() {
        let mut state = state(&[row("spotify:track:a", true)]);
        state.operation = Some(Operation {
            item_key: state.current().unwrap().key.clone(),
            stage: Stage::DeleteUncertain,
        });
        state.complete("updated".into());
        for observed in [
            None,
            Some(live("other", "spotify:track:a")),
            Some(live("chosen", "spotify:track:other")),
        ] {
            let fake = fake_player(false, vec![observed]);
            stop_removed(&fake, &state, "chosen").await.unwrap();
            assert!(fake.calls.lock().unwrap().is_empty());
        }
        let fake = fake_player(false, vec![Some(live("chosen", "spotify:track:a"))]);
        stop_removed(&fake, &state, "chosen").await.unwrap();
        assert_eq!(*fake.calls.lock().unwrap(), vec!["pause:chosen"]);
    }
}
