use crate::{
    aliases, commands, counters, currency::Currency, db, oauth2, player, secrets, spotify, twitch,
    utils, words,
};
use chrono::{DateTime, Utc};
use failure::format_err;
use futures::{
    future::{self, Future},
    stream::Stream,
};
use hashbrown::{HashMap, HashSet};
use irc::{
    client::{data::config, ext::ClientExt, Client, IrcClient, PackedIrcClient},
    proto::{
        command::{CapSubCommand, Command},
        message::{Message, Tag},
    },
};
use setmod_notifier::{Notification, Notifier};
use std::{
    fmt,
    sync::{Arc, Mutex, RwLock},
    time,
};
use tokio::timer;
use tokio_core::reactor::Core;
use tokio_threadpool::ThreadPool;

type BoxFuture =
    Box<dyn Future<Item = Option<player::TrackId>, Error = failure::Error> + Send + 'static>;

const SERVER: &'static str = "irc.chat.twitch.tv";
const TWITCH_TAGS_CAP: &'static str = "twitch.tv/tags";
const TWITCH_COMMANDS_CAP: &'static str = "twitch.tv/commands";

/// Configuration for twitch integration.
#[derive(Debug, serde::Deserialize)]
pub struct Config {
    bot: String,
    channels: Vec<Channel>,
    #[serde(default)]
    moderators: HashSet<String>,
    #[serde(default)]
    whitelisted_hosts: HashSet<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, fixed_map::Key)]
pub enum Feature {
    /// Song features.
    #[serde(rename = "song")]
    Song,
    /// Custom commands.
    #[serde(rename = "command")]
    Command,
    /// Counter commands.
    #[serde(rename = "counter")]
    Counter,
    /// Add afterstream notifications.
    #[serde(rename = "afterstream")]
    AfterStream,
    /// Feature to remove messages which matches a bad words filter.
    #[serde(rename = "bad-words")]
    BadWords,
    /// If URL-whitelisting is enabled.
    #[serde(rename = "url-whitelist")]
    UrlWhitelist,
    /// Admin features.
    #[serde(rename = "admin")]
    Admin,
}

/// Configuration for a twitch channel.
#[derive(Debug, serde::Deserialize)]
pub struct Channel {
    pub name: Arc<String>,
    /// Per-channel override of streamer.
    streamer: Option<String>,
    /// Per-channel currency.
    currency: Option<Currency>,
    /// Whether or not to notify on currency rewards.
    #[serde(default)]
    notify_rewards: bool,
    /// Player configuration file.
    player: Option<player::Config>,
    /// Features to add.
    features: HashSet<Feature>,
    /// Aliases in use for channel.
    aliases: aliases::Aliases,
}

pub fn run<'a>(
    core: &mut Core,
    db: db::Database,
    twitch: twitch::Twitch,
    spotify: Arc<spotify::Spotify>,
    streamer: Option<&'a str>,
    config: &'a Config,
    token: Arc<RwLock<oauth2::TwitchToken>>,
    commands: &'a commands::Commands<db::Database>,
    counters: &'a counters::Counters<db::Database>,
    bad_words: &'a words::Words<db::Database>,
    notifier: &'a Notifier,
    secrets: &'a secrets::Secrets,
) -> Result<impl Future<Item = (), Error = failure::Error> + 'a, failure::Error> {
    let access_token = token
        .read()
        .expect("poisoned lock")
        .access_token()
        .to_string();

    let irc_config = config::Config {
        nickname: Some(config.bot.clone()),
        channels: Some(config.channels.iter().map(|c| c.name.to_string()).collect()),
        password: Some(format!("oauth:{}", access_token)),
        server: Some(String::from(SERVER)),
        port: Some(6697),
        use_ssl: Some(true),
        ..config::Config::default()
    };

    let client = IrcClient::new_future(core.handle(), &irc_config)?;

    let PackedIrcClient(client, send_future) = core.run(client)?;
    client.identify()?;

    let sender = Sender::new(client.clone());
    sender.cap_req(TWITCH_TAGS_CAP);
    sender.cap_req(TWITCH_COMMANDS_CAP);

    let mut futures = Vec::<Box<dyn Future<Item = (), Error = failure::Error>>>::new();

    let mut currencies = HashMap::new();
    let mut stream_infos = HashMap::new();
    let mut players = HashMap::new();
    let mut channel_features = Features::default();
    let mut configs = HashMap::new();

    for channel in &config.channels {
        let mut features = FeatureSet::new();

        for feature in channel.features.iter().cloned() {
            features.insert(feature);
        }

        if let Some(currency) = channel.currency.as_ref() {
            let reward = 10;
            let interval = 60 * 10;

            let future = reward_loop(
                channel,
                reward,
                interval,
                db.clone(),
                twitch.clone(),
                sender.clone(),
                currency,
            );

            futures.push(Box::new(future));
            currencies.insert(channel.name.to_string(), currency);
        }

        let interval = 60 * 5;
        let stream_info = Arc::new(RwLock::new(None));
        let streamer = channel.streamer.as_ref().map(|s| s.as_str()).or(streamer);

        if let Some(streamer) = streamer {
            let future =
                stream_info_loop(interval, twitch.clone(), streamer, Arc::clone(&stream_info));
            futures.push(Box::new(future));
            stream_infos.insert(channel.name.to_string(), stream_info);
        }

        // Only setup if the song feature is enabled.
        if features.contains(Feature::Song) {
            // Setup a player if configured.
            if let Some(player_config) = channel.player.as_ref() {
                let (future, player) = player::run(
                    core,
                    db.clone(),
                    channel,
                    spotify.clone(),
                    player_config,
                    secrets,
                )?;

                players.insert(channel.name.to_string(), player.client());

                let sender = sender.clone();

                futures.push(Box::new(future));
                futures.push(Box::new(
                    player
                        .add_rx()
                        .map_err(|e| format_err!("failed to receive player update: {}", e))
                        .for_each(move |e| {
                            match e {
                                player::Event::Playing(_, item) => {
                                    let message = match item.user.as_ref() {
                                        Some(user) => {
                                            format!(
                                                "Now playing: {}, requested by {}.",
                                                item.what(),
                                                user
                                            )
                                        },
                                        None => format!(
                                            "Now playing: {}.",
                                            item.what(),
                                        )
                                    };

                                    sender.privmsg(channel.name.as_str(), message);
                                },
                                player::Event::Pausing => {
                                    sender.privmsg(
                                        channel.name.as_str(),
                                        "Pausing playback."
                                    );
                                },
                                player::Event::Empty => {
                                    sender.privmsg(
                                        channel.name.as_str(),
                                        format!(
                                            "Song queue is empty (use !song request <spotify-id> to add more).",
                                        ),
                                    );
                                },
                            }

                            Ok(())
                        }),
                ));
            }
        }

        channel_features.insert(channel.name.to_string(), features);
        configs.insert(channel.name.to_string(), channel);
    }

    futures.push(Box::new(send_future.map_err(failure::Error::from)));

    let mut handler = MessageHandler::new(
        twitch.clone(),
        db,
        sender.clone(),
        &config.moderators,
        &config.whitelisted_hosts,
        currencies,
        stream_infos,
        commands,
        counters,
        bad_words,
        notifier,
        players,
        channel_features,
        configs,
    );

    futures.push(Box::new(
        client
            .stream()
            .map_err(failure::Error::from)
            .and_then(move |m| handler.handle(&m))
            // handle any errors.
            .or_else(|e| {
                log::error!("failed to process message: {}", e);
                Ok(())
            })
            .for_each(|_| Ok(())),
    ));

    Ok(future::join_all(futures).map(|_| ()))
}

/// Set up a reward loop.
fn reward_loop<'a>(
    channel: &'a Channel,
    reward: i32,
    interval: u64,
    db: db::Database,
    twitch: twitch::Twitch,
    sender: Sender,
    currency: &'a Currency,
) -> impl Future<Item = (), Error = failure::Error> + 'a {
    // Add currency timer.
    timer::Interval::new_interval(time::Duration::from_secs(interval))
        .map_err(Into::into)
        // fetch all users.
        .and_then(move |_| {
            log::trace!("running reward loop");

            twitch.chatters(channel.name.as_str()).and_then(|chatters| {
                let mut u = HashSet::new();
                u.extend(chatters.viewers);
                u.extend(chatters.moderators);
                u.extend(chatters.broadcaster);

                if u.is_empty() {
                    Err(format_err!("no chatters to reward"))
                } else {
                    Ok(u)
                }
            })
        })
        // update database.
        .and_then(move |u| db.balances_increment(channel.name.as_str(), u, reward))
        .map(move |_| {
            if channel.notify_rewards {
                sender.privmsg(
                    channel.name.as_str(),
                    format!("/me has given {} {} to all viewers!", reward, currency.name),
                );
            }
        })
        // handle any errors.
        .or_else(|e| {
            log::error!("failed to reward users: {}", e);
            Ok(())
        })
        .for_each(|_| Ok(()))
}

/// Set up a reward loop.
fn stream_info_loop<'a>(
    interval: u64,
    twitch: twitch::Twitch,
    streamer: &'a str,
    stream_info: Arc<RwLock<Option<StreamInfo>>>,
) -> impl Future<Item = (), Error = failure::Error> + 'a {
    // Add currency timer.
    timer::Interval::new(time::Instant::now(), time::Duration::from_secs(interval))
        .map_err(failure::Error::from)
        .map(move |_| {
            log::trace!("refreshing stream info for streamer: {}", streamer);
        })
        .and_then(move |_| {
            twitch
                .stream_by_id(streamer)
                .join(twitch.channel_by_id(streamer))
        })
        .and_then(move |(stream, channel)| {
            let mut u = stream_info
                .write()
                .map_err(|_| format_err!("lock poisoned"))?;

            *u = Some(StreamInfo {
                game: channel.game,
                title: channel.status,
                started_at: stream.map(|s| s.created_at),
            });

            Ok(())
        })
        // handle any errors.
        .or_else(move |e| {
            log::error!(
                "failed to refresh stream info for streamer: {}: {}",
                streamer,
                e
            );
            Ok(())
        })
        .for_each(|_| Ok(()))
}

#[derive(Clone)]
struct Sender {
    client: IrcClient,
    thread_pool: Arc<ThreadPool>,
    limiter: Arc<Mutex<ratelimit::Limiter>>,
}

impl Sender {
    pub fn new(client: IrcClient) -> Sender {
        let limiter = ratelimit::Builder::new().frequency(10).capacity(95).build();

        Sender {
            client,
            thread_pool: Arc::new(ThreadPool::new()),
            limiter: Arc::new(Mutex::new(limiter)),
        }
    }

    /// Send a message.
    fn send(&self, m: impl Into<Message>) {
        let client = self.client.clone();
        let m = m.into();
        let limiter = Arc::clone(&self.limiter);

        self.thread_pool.spawn(future::lazy(move || {
            limiter.lock().expect("lock poisoned").wait();

            if let Err(e) = client.send(m) {
                log::error!("failed to send message: {}", e);
            }

            Ok(())
        }));
    }

    /// Send a PRIVMSG.
    pub fn privmsg(&self, target: &str, f: impl fmt::Display) {
        self.send(Command::PRIVMSG(target.to_owned(), f.to_string()))
    }

    /// Send a capability request.
    pub fn cap_req(&self, cap: &str) {
        self.send(Command::CAP(
            None,
            CapSubCommand::REQ,
            Some(String::from(cap)),
            None,
        ))
    }
}

/// Handler for incoming messages.
struct MessageHandler<'a> {
    /// API access.
    twitch: twitch::Twitch,
    /// Database.
    db: db::Database,
    /// Queue for sending messages.
    sender: Sender,
    /// Moderators.
    moderators: &'a HashSet<String>,
    /// Whitelisted hosts for links.
    whitelisted_hosts: &'a HashSet<String>,
    /// Currency in use.
    currencies: HashMap<String, &'a Currency>,
    /// Per-channel stream_infos.
    stream_infos: HashMap<String, Arc<RwLock<Option<StreamInfo>>>>,
    /// All registered commands.
    commands: &'a commands::Commands<db::Database>,
    /// All registered counters.
    counters: &'a counters::Counters<db::Database>,
    /// Bad words.
    bad_words: &'a words::Words<db::Database>,
    /// For sending notifications.
    notifier: &'a Notifier,
    /// Music player.
    players: HashMap<String, player::PlayerClient>,
    /// Per-channel features.
    features: Features,
    /// Per-channel configurations.
    configs: HashMap<String, &'a Channel>,
    /// Thread pool used for driving futures.
    thread_pool: Arc<ThreadPool>,
}

impl<'a> MessageHandler<'a> {
    /// Build a new handler.
    pub fn new(
        twitch: twitch::Twitch,
        db: db::Database,
        sender: Sender,
        moderators: &'a HashSet<String>,
        whitelisted_hosts: &'a HashSet<String>,
        currencies: HashMap<String, &'a Currency>,
        stream_infos: HashMap<String, Arc<RwLock<Option<StreamInfo>>>>,
        commands: &'a commands::Commands<db::Database>,
        counters: &'a counters::Counters<db::Database>,
        bad_words: &'a words::Words<db::Database>,
        notifier: &'a Notifier,
        players: HashMap<String, player::PlayerClient>,
        features: Features,
        configs: HashMap<String, &'a Channel>,
    ) -> Self {
        Self {
            twitch,
            db,
            sender,
            moderators,
            whitelisted_hosts,
            currencies,
            stream_infos,
            commands,
            counters,
            bad_words,
            notifier,
            players,
            features,
            configs,
            thread_pool: Arc::new(ThreadPool::new()),
        }
    }

    /// Run as user.
    fn as_user<'sender, 'm>(&self, m: &Message) -> Result<User, failure::Error> {
        let name = m
            .source_nickname()
            .ok_or_else(|| format_err!("expected user info"))?;

        let target = m
            .response_target()
            .ok_or_else(|| format_err!("expected user info"))?;

        Ok(User {
            sender: self.sender.clone(),
            name: name.to_string(),
            target: target.to_string(),
        })
    }

    /// Test if moderator.
    fn is_moderator(&self, user: &User) -> bool {
        self.moderators.contains(&user.name)
    }

    /// Check that the given user is a moderator.
    fn check_moderator(&self, user: &User) -> Result<(), failure::Error> {
        if self.is_moderator(user) {
            return Ok(());
        }

        self.sender.privmsg(
            &user.target,
            format!(
                "Do you think this is a democracy {name}? LUL",
                name = user.name
            ),
        );

        failure::bail!("moderator access required for action");
    }

    /// Handle the !badword command.
    fn handle_bad_word(
        &mut self,
        user: &User,
        it: &mut utils::Words<'_>,
    ) -> Result<(), failure::Error> {
        match it.next() {
            Some("edit") => {
                self.check_moderator(user)?;

                let word = it.next().ok_or_else(|| format_err!("expected word"))?;
                let why = match it.rest() {
                    "" => None,
                    other => Some(other),
                };
;
                self.bad_words.edit(word, why)?;
                user.respond("Bad word edited");
            }
            Some("delete") => {
                self.check_moderator(user)?;

                let word = it.next().ok_or_else(|| format_err!("expected word"))?;

                if self.bad_words.delete(word)? {
                    user.respond("Bad word removed.");
                } else {
                    user.respond("Bad word did not exist.");
                }
            }
            None => {
                user.respond("!badword is a word filter, removing unwanted messages.");
            }
            Some(_) => {
                user.respond("Expected: edit, or delete.");
            }
        }

        Ok(())
    }

    /// Handle song command.
    fn handle_song(
        &mut self,
        user: &User,
        it: &mut utils::Words<'_>,
    ) -> Result<(), failure::Error> {
        let player = match self.players.get(user.target.as_str()) {
            Some(player) => player,
            None => {
                log::warn!("No player configured for channel :(");
                return Ok(());
            }
        };

        match it.next() {
            Some("mine") => {}
            Some("list") => {
                let mut limit = 3usize;

                if let Some(n) = it.next() {
                    self.check_moderator(user)?;

                    if let Ok(n) = str::parse(n) {
                        limit = n;
                    }
                }

                let items = player.list(limit + 1);

                let has_more = match items.len() > limit {
                    true => Some(items.len() - limit),
                    false => None,
                };

                self.display_songs(user, has_more, items.iter().take(limit).cloned());
            }
            Some("current") => match player.current() {
                Some(item) => {
                    if let Some(name) = item.user.as_ref() {
                        user.respond(format!(
                            "Current song: {}, requested by {}.",
                            item.what(),
                            name
                        ));
                    } else {
                        user.respond(format!("Current song: {}", item.what()));
                    }
                }
                None => {
                    user.respond("No song :(");
                }
            },
            Some("delete") => {
                let removed = match it.next() {
                    Some("last") => match it.next() {
                        Some(last_user) => {
                            let last_user = last_user.to_lowercase();
                            self.check_moderator(&user)?;
                            player.remove_last_by_user(&last_user)?
                        }
                        None => {
                            self.check_moderator(&user)?;
                            player.remove_last()?
                        }
                    },
                    Some("mine") => player.remove_last_by_user(&user.name)?,
                    Some(_) | None => {
                        user.respond(format!("Expected: last, last <user>, or mine"));
                        failure::bail!("bad command");
                    }
                };

                match removed {
                    None => user.respond("No song removed, sorry :("),
                    Some(item) => user.respond(format!("Removed: {}!", item.what())),
                }
            }
            Some("volume") => {
                match it.next() {
                    // setting volume
                    Some(other) => {
                        self.check_moderator(&user)?;

                        let (diff, argument) = match other.chars().next() {
                            Some('+') => (Some(true), &other[1..]),
                            Some('-') => (Some(false), &other[1..]),
                            _ => (None, other),
                        };

                        let argument = match str::parse::<u32>(argument) {
                            // clamp the volume
                            Ok(argument) => argument,
                            Err(_) => {
                                user.respond("expected numeric argument");
                                failure::bail!("bad command");
                            }
                        };

                        let argument = match diff {
                            Some(true) => player.current_volume().saturating_add(argument),
                            Some(false) => player.current_volume().saturating_sub(argument),
                            None => argument,
                        };

                        let argument = u32::min(100, argument);
                        user.respond(format!("Volume set to {}.", argument));
                        player.volume(argument)?;
                    }
                    // reading volume
                    None => {
                        user.respond(format!("Current volume: {}.", player.current_volume()));
                    }
                }
            }
            Some("skip") => {
                self.check_moderator(&user)?;
                player.skip()?;
            }
            Some("request") => {
                let q = it.rest();

                if !it.next().is_some() {
                    user.respond("expected: !song request <id>|<text>");
                    failure::bail!("bad command");
                }

                let track_id_future: BoxFuture = match player::TrackId::from_url_or_uri(q) {
                    Ok(track_id) => Box::new(future::ok(Some(track_id))),
                    Err(e) => {
                        log::info!("Failed to parse as URL/URI: {}: {}", q, e);
                        Box::new(player.search_track(q))
                    }
                };

                let future = track_id_future
                    .and_then({
                        let user = user.clone();

                        move |track_id| match track_id {
                            None => {
                                user.respond(
                                    "Could not find a track matching your request, sorry :(",
                                );
                                return Err(failure::format_err!("bad track in request"));
                            }
                            Some(track_id) => return Ok(track_id),
                        }
                    })
                    .map_err(|e| {
                        log::error!("failed to add track: {}", e);
                        ()
                    })
                    .and_then({
                        let is_moderator = self.is_moderator(user);
                        let user = user.clone();
                        let player = player.clone();

                        move |track_id| {
                            player.add_track(&user.name, track_id, is_moderator).then(move |result| {
                                match result {
                                    Ok((pos, item)) => {
                                        user.respond(format!(
                                            "Added {what} at position #{pos}!",
                                            what = item.what(),
                                            pos = pos + 1
                                        ));
                                    }
                                    Err(player::AddTrackError::QueueContainsTrack(pos)) => {
                                        user.respond(format!(
                                            "Player already contains that track (position #{pos}).",
                                            pos = pos + 1,
                                        ));
                                    }
                                    Err(player::AddTrackError::TooManyUserTracks(count)) => {
                                        match count {
                                            0 => {
                                                user.respond("Unfortunately you are not allowed to add tracks :(");
                                            }
                                            1 => {
                                                user.respond(
                                                    "<3 your enthusiasm, but you already have a track in the queue.",
                                                );
                                            }
                                            count => {
                                                user.respond(format!(
                                                    "<3 your enthusiasm, but you already have {count} tracks in the queue.",
                                                    count = count,
                                                ));
                                            }
                                        }
                                    }
                                    Err(player::AddTrackError::QueueFull) => {
                                        user.respond("Player is full, try again later!");
                                    }
                                    Err(player::AddTrackError::Error(e)) => {
                                        user.respond("There was a problem adding your song :(");
                                        log::error!("failed to add song: {}", e);
                                    }
                                }

                                Ok(())
                            })
                        }
                    });

                self.thread_pool.spawn(future);
            }
            Some("toggle") => {
                self.check_moderator(&user)?;
                player.toggle()?;
            }
            Some("play") => {
                self.check_moderator(&user)?;
                player.play()?;
            }
            Some("pause") => {
                self.check_moderator(&user)?;
                player.pause()?;
            }
            Some("length") => {
                let (count, seconds) = player.length();

                match count {
                    0 => user.respond("No songs in queue :("),
                    1 => {
                        let length = utils::human_time(seconds as i64);
                        user.respond(format!("One song in queue with {} of play time.", length));
                    }
                    count => {
                        let length = utils::human_time(seconds as i64);
                        user.respond(format!(
                            "{} songs in queue with {} of play time.",
                            count, length
                        ));
                    }
                }
            }
            None | Some(..) => {
                user.respond("Expected: skip, request, or toggle.");
            }
        }

        Ok(())
    }

    /// Handle command administration.
    fn handle_command(
        &mut self,
        user: &User,
        it: &mut utils::Words<'_>,
    ) -> Result<(), failure::Error> {
        match it.next() {
            Some("list") => {
                let mut names = self
                    .commands
                    .list(user.target.as_str())
                    .into_iter()
                    .map(|c| format!("!{}", c.key.name))
                    .collect::<Vec<_>>();

                if names.is_empty() {
                    user.respond("No custom commands.");
                } else {
                    names.sort();
                    user.respond(format!("Custom commands: {}", names.join(", ")));
                }
            }
            Some("edit") => {
                self.check_moderator(&user)?;

                let name = match it.next() {
                    Some(name) => name,
                    None => {
                        user.respond("Expected name.");
                        failure::bail!("bad command");
                    }
                };

                self.commands.edit(user.target.as_str(), name, it.rest())?;
                user.respond("Edited command.");
            }
            Some("delete") => {
                self.check_moderator(&user)?;

                let name = match it.next() {
                    Some(name) => name,
                    None => {
                        user.respond("Expected name.");
                        failure::bail!("bad command");
                    }
                };

                if self.commands.delete(user.target.as_str(), name)? {
                    user.respond(format!("Deleted command `{}`.", name));
                } else {
                    user.respond("No such command.");
                }
            }
            None | Some(..) => {
                user.respond("Expected: list, edit, or delete.");
            }
        }

        Ok(())
    }

    /// Handle counter administration.
    fn handle_counter(
        &mut self,
        user: &User,
        it: &mut utils::Words<'_>,
    ) -> Result<(), failure::Error> {
        match it.next() {
            Some("list") => {
                let mut names = self
                    .counters
                    .list(user.target.as_str())
                    .into_iter()
                    .map(|c| format!("!{}", c.key.name))
                    .collect::<Vec<_>>();

                if names.is_empty() {
                    user.respond("No custom counters.");
                } else {
                    names.sort();
                    user.respond(format!("Custom counters: {}", names.join(", ")));
                }
            }
            Some("edit") => {
                self.check_moderator(&user)?;

                let name = match it.next() {
                    Some(name) => name,
                    None => {
                        user.respond("Expected name.");
                        failure::bail!("bad command");
                    }
                };

                self.counters.edit(user.target.as_str(), name, it.rest())?;
                user.respond("Edited command.");
            }
            Some("delete") => {
                self.check_moderator(&user)?;

                let name = match it.next() {
                    Some(name) => name,
                    None => {
                        user.respond("Expected name.");
                        failure::bail!("bad command");
                    }
                };

                if self.counters.delete(user.target.as_str(), name)? {
                    user.respond(format!("Deleted command `{}`.", name));
                } else {
                    user.respond("No such command.");
                }
            }
            None | Some(..) => {
                user.respond("Expected: list, edit, or delete.");
            }
        }

        Ok(())
    }

    /// Handle the uptime command.
    fn handle_uptime(&mut self, user: &User) {
        let started_at = self.stream_infos.get(&user.target).and_then(|s| {
            s.read()
                .expect("lock poisoned")
                .as_ref()
                .and_then(|s| s.started_at.clone())
        });

        match started_at {
            Some(started_at) => {
                let now = Utc::now();
                let uptime = utils::human_time((now - started_at).num_seconds());

                user.respond(format!(
                    "Stream has been live for {uptime}.",
                    uptime = uptime
                ));
            }
            None => {
                user.respond("Stream is not live right now, try again later!");
            }
        };
    }

    /// Handle the title command.
    fn handle_title(&mut self, user: &User) {
        let title = self.stream_infos.get(&user.target).and_then(|s| {
            s.read()
                .expect("lock poisoned")
                .as_ref()
                .map(|s| s.title.clone())
        });

        match title {
            Some(title) => {
                user.respond(title);
            }
            None => {
                user.respond("Stream is not live right now, try again later!");
            }
        };
    }

    /// Handle the title update.
    fn handle_update_title(&mut self, user: &User, title: &str) -> Result<(), failure::Error> {
        let channel_id = user.target.trim_start_matches('#');

        let twitch = self.twitch.clone();
        let user = user.to_owned();
        let title = title.to_string();

        let mut request = twitch::UpdateChannelRequest::default();
        request.channel.status = Some(title);

        self.thread_pool.spawn(
            twitch
                .update_channel(channel_id, &request)
                .and_then(move |_| {
                    user.respond("Title updated!");
                    Ok(())
                })
                .or_else(|e| {
                    log::error!("failed to update title: {}", e);
                    Ok(())
                }),
        );

        Ok(())
    }

    /// Handle the game command.
    fn handle_game(&mut self, user: &User) {
        let game = self.stream_infos.get(&user.target).and_then(|s| {
            s.read()
                .expect("lock poisoned")
                .as_ref()
                .and_then(|s| s.game.clone())
        });

        match game {
            Some(game) => {
                user.respond(game);
            }
            None => {
                user.respond("Unfortunately I don't know the game, sorry!");
            }
        };
    }

    /// Handle the game update.
    fn handle_update_game(&mut self, user: &User, game: &str) -> Result<(), failure::Error> {
        let channel_id = user.target.trim_start_matches('#');

        let twitch = self.twitch.clone();
        let user = user.to_owned();
        let game = game.to_string();

        let mut request = twitch::UpdateChannelRequest::default();
        request.channel.game = Some(game);

        self.thread_pool.spawn(
            twitch
                .update_channel(channel_id, &request)
                .and_then(move |_| {
                    user.respond("Game updated!");
                    Ok(())
                })
                .or_else(|e| {
                    log::error!("failed to update game: {}", e);
                    Ok(())
                }),
        );

        Ok(())
    }

    /// Handle a command.
    pub fn process_command<'local>(
        &mut self,
        features: &FeatureSet,
        command: &str,
        m: &'local Message,
        it: &mut utils::Words<'local>,
    ) -> Result<(), failure::Error> {
        let user = self.as_user(m)?;

        match command {
            "ping" => {
                user.respond("What do you want?");
                self.notifier.send(Notification::Ping)?;
            }
            "song" if features.contains(Feature::Song) => {
                self.handle_song(&user, it)?;
            }
            "command" if features.contains(Feature::Command) => {
                self.handle_command(&user, it)?;
            }
            "counter" => {
                self.handle_counter(&user, it)?;
            }
            "afterstream" if features.contains(Feature::AfterStream) => {
                self.db.insert_afterstream(&user.name, it.rest())?;
                user.respond("Reminder added.");
            }
            "badword" if features.contains(Feature::BadWords) => {
                self.handle_bad_word(&user, it)?;
            }
            "uptime" if features.contains(Feature::Admin) => {
                self.handle_uptime(&user);
            }
            "title" if features.contains(Feature::Admin) => {
                let rest = it.rest();

                if rest.is_empty() {
                    self.handle_title(&user);
                } else {
                    self.check_moderator(&user)?;
                    self.handle_update_title(&user, rest)?;
                }
            }
            "game" if features.contains(Feature::Admin) => {
                let rest = it.rest();

                if rest.is_empty() {
                    self.handle_game(&user);
                } else {
                    self.check_moderator(&user)?;
                    self.handle_update_game(&user, rest)?;
                }
            }
            other => {
                if let Some(currency) = self.currencies.get(&user.target) {
                    if currency.name == other {
                        let balance = self.db.balance_of(&user.name)?.unwrap_or(0);
                        user.respond(format!(
                            "You have {balance} {name}.",
                            balance = balance,
                            name = currency.name
                        ));
                    }
                }

                if features.contains(Feature::Command) {
                    if let Some(command) = self.commands.get(user.target.as_str(), other) {
                        let vars = TemplateVars {
                            name: &user.name,
                            target: &user.target,
                        };
                        let response = command.render(&vars)?;
                        self.sender.privmsg(&user.target, response);
                    }
                }

                if features.contains(Feature::Counter) {
                    if let Some(counter) = self.counters.get(user.target.as_str(), other) {
                        self.counters.increment(&*counter)?;

                        let vars = CounterVars {
                            name: &user.name,
                            target: &user.target,
                            count: counter.count(),
                        };

                        let response = counter.render(&vars)?;
                        self.sender.privmsg(&user.target, response);
                    }
                }
            }
        }

        Ok(())
    }

    /// Extract tags from message.
    fn tags<'local>(m: &'local Message) -> Tags<'local> {
        let mut message_id = None;

        if let Some(tags) = m.tags.as_ref() {
            for t in tags {
                match *t {
                    Tag(ref name, Some(ref value)) if name == "id" => {
                        message_id = Some(value.as_str());
                    }
                    _ => {}
                }
            }
        }

        Tags { message_id }
    }

    /// Delete the given message.
    fn delete_message<'local>(
        &mut self,
        source: &str,
        tags: Tags<'local>,
    ) -> Result<(), failure::Error> {
        let message_id = match tags.message_id {
            Some(message_id) => message_id,
            None => return Ok(()),
        };

        log::info!("Attempting to delete message: {}", message_id);

        self.sender
            .privmsg(source, format!("/delete {}", message_id));
        Ok(())
    }

    /// Test if the message should be deleted.
    fn should_be_deleted(&mut self, features: &FeatureSet, m: &Message, message: &str) -> bool {
        let user = m.source_nickname();

        // Moderators can say whatever they want.
        if user.map(|u| self.moderators.contains(u)).unwrap_or(false) {
            return false;
        }

        if features.contains(Feature::BadWords) {
            if let Some(word) = self.test_bad_words(message) {
                if let (Some(why), Some(user), Some(target)) =
                    (word.why.as_ref(), user, m.response_target())
                {
                    let why = why.render_to_string(&TemplateVars {
                        name: user,
                        target: target,
                    });

                    match why {
                        Ok(why) => {
                            self.sender.privmsg(target, &why);
                        }
                        Err(e) => {
                            log::error!("failed to render response: {}", e);
                        }
                    }
                }

                return true;
            }
        }

        if features.contains(Feature::UrlWhitelist) {
            if self.has_bad_link(message) {
                return true;
            }
        }

        false
    }

    /// Test the message for bad words.
    fn test_bad_words(&self, message: &str) -> Option<Arc<words::Word>> {
        let tester = self.bad_words.tester();

        for word in utils::TrimmedWords::new(message) {
            if let Some(word) = tester.test(word) {
                return Some(word);
            }
        }

        None
    }

    /// Check if the given iterator has URLs that need to be
    fn has_bad_link(&mut self, message: &str) -> bool {
        for url in utils::Urls::new(message) {
            if let Some(host) = url.host_str() {
                if !self.whitelisted_hosts.contains(host) {
                    return true;
                }
            }
        }

        false
    }

    /// Handle the given command.
    pub fn handle<'local>(&mut self, m: &'local Message) -> Result<(), failure::Error> {
        let (features, config) = match m.response_target() {
            Some(target) => (
                self.features.features(target),
                self.configs.get(target).cloned(),
            ),
            None => (Default::default(), None),
        };

        match m.command {
            Command::PRIVMSG(ref source, ref message) => {
                let tags = Self::tags(&m);

                let alias;
                let mut it = utils::Words::new(message);

                if let Some(aliases) = config.map(|c| &c.aliases) {
                    // NB: needs to store locally to maintain a reference to it.
                    alias = aliases.lookup(it.clone());

                    if let Some(alias) = alias.as_ref() {
                        it = utils::Words::new(alias.as_str());
                    }
                }

                if let Some(command) = it.next() {
                    if command.starts_with('!') {
                        let command = &command[1..];

                        if let Err(e) = self.process_command(&features, command, m, &mut it) {
                            log::error!("failed to process command: {}", e);
                        }
                    }
                }

                if self.should_be_deleted(&features, m, message) {
                    self.delete_message(source, tags)?;
                }
            }
            Command::CAP(_, CapSubCommand::ACK, _, ref what) => {
                log::info!(
                    "Capability Acknowledged: {}",
                    what.as_ref().map(|w| w.as_str()).unwrap_or("*")
                );
            }
            Command::JOIN(ref channel, _, _) => {
                let user = m.source_nickname().unwrap_or("?");
                log::info!("{} joined {}", user, channel);
            }
            Command::Response(..) => {
                log::info!("Response: {}", m);
            }
            Command::PING(..) | Command::PONG(..) => {
                // ignore
            }
            Command::Raw(..) => {
                log::info!("raw command: {:?}", m);
            }
            _ => {
                log::info!("unhandled: {:?}", m);
            }
        }

        Ok(())
    }

    /// Display the collection of songs.
    fn display_songs(
        &mut self,
        user: &User,
        has_more: Option<usize>,
        it: impl IntoIterator<Item = Arc<player::Item>>,
    ) {
        let mut lines = Vec::new();

        for (index, item) in it.into_iter().enumerate() {
            match item.user.as_ref() {
                Some(user) => {
                    lines.push(format!("#{}: {} ({user})", index, item.what(), user = user));
                }
                None => {
                    lines.push(format!("#{}: {}", index, item.what()));
                }
            }
        }

        if lines.is_empty() {
            user.respond("Song queue is empty.");
            return;
        }

        if let Some(more) = has_more {
            user.respond(format!("{} ... and {} more.", lines.join("; "), more));

            return;
        }

        user.respond(format!("{}.", lines.join("; ")));
    }
}

#[derive(Clone)]
pub struct User {
    sender: Sender,
    name: String,
    target: String,
}

impl User {
    /// Respond to the user with a message.
    pub fn respond(&self, m: impl fmt::Display) {
        self.sender
            .privmsg(self.target.as_str(), format!("{} -> {}", self.name, m));
    }
}

#[derive(Debug)]
pub struct StreamInfo {
    title: String,
    game: Option<String>,
    started_at: Option<DateTime<Utc>>,
}

/// Struct of tags.
#[derive(Debug)]
pub struct Tags<'a> {
    message_id: Option<&'a str>,
}

#[derive(Debug)]
pub enum SenderThreadItem {
    Exit,
    Send(Message),
}

#[derive(serde::Serialize)]
pub struct TemplateVars<'a> {
    name: &'a str,
    target: &'a str,
}

#[derive(serde::Serialize)]
pub struct CounterVars<'a> {
    name: &'a str,
    target: &'a str,
    count: i32,
}

/// By-channel features that are enabled.
#[derive(Default)]
pub struct Features {
    features: HashMap<String, FeatureSet>,
}

type FeatureSet = fixed_map::Set<Feature>;

impl Features {
    /// Insert the given feature for the channel
    pub fn insert(&mut self, channel: String, features: FeatureSet) {
        self.features.insert(channel, features);
    }

    /// Get the feature set for the given channel.
    pub fn features(&self, channel: &str) -> FeatureSet {
        self.features
            .get(channel)
            .cloned()
            .unwrap_or_else(Default::default)
    }
}
