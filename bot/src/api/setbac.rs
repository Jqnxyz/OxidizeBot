//! setbac.tv API helpers.

use crate::{
    api::base::RequestBuilder,
    bus,
    injector::Injector,
    oauth2,
    player::{self, Player},
    prelude::*,
    settings::Settings,
    utils,
};
use reqwest::{header, r#async::Client, Method, Url};
use std::sync::Arc;

static DEFAULT_API_URL: &'static str = "https://setbac.tv";

fn parse_url(url: &str) -> Option<Url> {
    match str::parse(url) {
        Ok(api_url) => Some(api_url),
        Err(e) => {
            log::warn!("bad api url: {}: {}", url, e);
            None
        }
    }
}

struct RemoteBuilder {
    token: oauth2::SyncToken,
    global_bus: Arc<bus::Bus<bus::Global>>,
    player: Option<Player>,
    enabled: bool,
    api_url: Option<Url>,
}

impl RemoteBuilder {
    fn init(&self, remote: &mut Remote) {
        if !self.enabled {
            remote.rx = None;
            remote.client = None;
            remote.setbac = None;
            return;
        }

        remote.rx = Some(self.global_bus.add_rx());

        remote.client = match self.player.as_ref() {
            Some(player) => Some(player.clone()),
            None => None,
        };

        remote.setbac = match self.api_url.as_ref() {
            Some(api_url) => Some(SetBac::new(self.token.clone(), api_url.clone())),
            None => None,
        };
    }
}

#[derive(Default)]
struct Remote {
    rx: Option<bus::Reader<bus::Global>>,
    client: Option<player::Player>,
    setbac: Option<SetBac>,
}

/// Run update loop shipping information to the remote server.
pub fn run(
    settings: &Settings,
    injector: &Injector,
    token: oauth2::SyncToken,
    global_bus: Arc<bus::Bus<bus::Global>>,
) -> Result<impl Future<Output = Result<(), failure::Error>>, failure::Error> {
    let settings = settings.scoped("remote");

    let (mut api_url_stream, api_url) = settings
        .stream("api-url")
        .or(Some(String::from(DEFAULT_API_URL)))
        .optional()?;

    let (mut enabled_stream, enabled) = settings.stream("enabled").or_with(false)?;

    let (mut player_stream, player) = injector.stream::<Player>();

    let mut remote_builder = RemoteBuilder {
        token,
        global_bus,
        player: None,
        enabled: false,
        api_url: None,
    };

    remote_builder.enabled = enabled;
    remote_builder.player = player;
    remote_builder.api_url = match api_url.and_then(|s| parse_url(&s)) {
        Some(api_url) => Some(api_url),
        None => None,
    };

    let mut remote = Remote::default();
    remote_builder.init(&mut remote);

    Ok(async move {
        loop {
            futures::select! {
                update = player_stream.select_next_some() => {
                    remote_builder.player = update;
                    remote_builder.init(&mut remote);
                }
                update = api_url_stream.select_next_some() => {
                    remote_builder.api_url = match update.and_then(|s| parse_url(&s)) {
                        Some(api_url) => Some(api_url),
                        None => None,
                    };

                    remote_builder.init(&mut remote);
                }
                update = enabled_stream.select_next_some() => {
                    remote_builder.enabled = update;
                    remote_builder.init(&mut remote);
                }
                event = remote.rx.select_next_some() => {
                    /// Only update on switches to current song.
                    match event {
                        bus::Global::SongModified => (),
                        _ => continue,
                    };

                    let setbac = match remote.setbac.as_ref() {
                        Some(setbac) => setbac,
                        None => continue,
                    };

                    let client = match remote.client.as_ref() {
                        Some(client) => client,
                        None => continue,
                    };

                    log::trace!("pushing remote player update");

                    let mut update = PlayerUpdate::default();

                    update.current = client.current().map(|c| c.item.into());

                    for i in client.list() {
                        update.items.push(i.into());
                    }

                    if let Err(e) = setbac.player_update(update).await {
                        log::error!("failed to perform remote player update: {}", e);
                    }
                }
            }
        }
    })
}

/// API integration.
#[derive(Clone, Debug)]
pub struct SetBac {
    client: Client,
    api_url: Url,
    token: oauth2::SyncToken,
}

impl SetBac {
    /// Create a new API integration.
    pub fn new(token: oauth2::SyncToken, api_url: Url) -> Self {
        SetBac {
            client: Client::new(),
            api_url,
            token,
        }
    }

    /// Get request against API.
    fn request(&self, method: Method, path: &[&str]) -> RequestBuilder {
        let mut url = self.api_url.clone();
        url.path_segments_mut().expect("bad base").extend(path);

        RequestBuilder::new(self.client.clone(), method, url)
            .token(self.token.clone())
            .use_oauth2_header()
    }

    /// Update the channel information.
    pub async fn player_update(&self, request: PlayerUpdate) -> Result<(), failure::Error> {
        let body = serde_json::to_vec(&request)?;

        let req = self
            .request(Method::POST, &["api", "player"])
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);

        let _ = req.execute().await?.ok()?;
        Ok(())
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PlayerUpdate {
    /// Current song.
    #[serde(default)]
    current: Option<Item>,
    /// Songs.
    #[serde(default)]
    items: Vec<Item>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Item {
    /// Name of the song.
    name: String,
    /// Artists of the song.
    #[serde(default)]
    artists: Option<String>,
    /// Track ID of the song.
    track_id: String,
    /// URL of the song.
    track_url: String,
    /// User who requested the song.
    #[serde(default)]
    user: Option<String>,
    /// Length of the song.
    duration: String,
}

impl From<Arc<player::Item>> for Item {
    fn from(i: Arc<player::Item>) -> Self {
        Item {
            name: i.track.name(),
            artists: i.track.artists(),
            track_id: i.track_id.to_string(),
            track_url: i.track_id.url(),
            user: i.user.clone(),
            duration: utils::compact_duration(&i.duration),
        }
    }
}
