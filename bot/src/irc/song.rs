use crate::{command, irc, player, utils, utils::BoxFuture};
use futures::future::{self, Future};
use std::sync::Arc;

/// Handler for the `!song` command.
pub struct Song {
    pub player: player::PlayerClient,
}

impl command::Handler for Song {
    fn handle<'m>(
        &mut self,
        mut ctx: command::Context<'_>,
        user: irc::User<'m>,
        it: &mut utils::Words<'m>,
    ) -> Result<(), failure::Error> {
        match it.next() {
            Some("theme") => {
                ctx.check_moderator(&user)?;

                let name = match it.next() {
                    Some(name) => name,
                    None => {
                        user.respond("Expected: !song theme <name>");
                        failure::bail!("bad command");
                    }
                };

                let future = self.player.play_theme(name).then({
                    let user = user.as_owned_user();

                    move |r| {
                        match r {
                            Ok(()) => {}
                            Err(player::PlayThemeError::NoSuchTheme) => {
                                user.respond("No such theme :(");
                            }
                            Err(player::PlayThemeError::Error(e)) => {
                                user.respond("There was a problem adding your song :(");
                                utils::log_err("failed to add song", e);
                            }
                        }

                        Ok(())
                    }
                });

                ctx.spawn(future);
            }
            Some("promote") => {
                ctx.check_moderator(&user)?;

                let index = match it.next() {
                    Some(index) => parse_queue_position(&user, index)?,
                    None => failure::bail!("bad command"),
                };

                if let Some(item) = self.player.promote_song(&user.name, index) {
                    user.respond(format!("Promoted song to head of queue: {}", item.what()));
                } else {
                    user.respond("No such song to promote");
                }
            }
            Some("close") => {
                ctx.check_moderator(&user)?;

                self.player.close(match it.rest() {
                    "" => None,
                    other => Some(other.to_string()),
                });
            }
            Some("open") => {
                ctx.check_moderator(&user)?;
                self.player.open();
            }
            Some("list") => {
                if let Some(api_url) = ctx.api_url {
                    user.respond(format!(
                        "You can find the queue at {}/player/{}",
                        api_url, ctx.streamer
                    ));
                    return Ok(());
                }

                let mut limit = 3usize;

                if let Some(n) = it.next() {
                    ctx.check_moderator(&user)?;

                    if let Ok(n) = str::parse(n) {
                        limit = n;
                    }
                }

                let items = self.player.list();

                let has_more = match items.len() > limit {
                    true => Some(items.len() - limit),
                    false => None,
                };

                display_songs(&user, has_more, items.iter().take(limit).cloned());
            }
            Some("current") => match self.player.current() {
                Some(item) => {
                    if let Some(name) = item.user.as_ref() {
                        user.respond(format!(
                            "Current song: {}, requested by {} ({duration}).",
                            item.what(),
                            name,
                            duration = item.duration(),
                        ));
                    } else {
                        user.respond(format!(
                            "Current song: {} ({duration})",
                            item.what(),
                            duration = item.duration()
                        ));
                    }
                }
                None => {
                    user.respond("No song :(");
                }
            },
            Some("purge") => {
                ctx.check_moderator(&user)?;
                self.player.purge()?;
                user.respond("Song queue purged.");
            }
            Some("delete") => {
                let removed = match it.next() {
                    Some("last") => match it.next() {
                        Some(last_user) => {
                            let last_user = last_user.to_lowercase();
                            ctx.check_moderator(&user)?;
                            self.player.remove_last_by_user(&last_user)?
                        }
                        None => {
                            ctx.check_moderator(&user)?;
                            self.player.remove_last()?
                        }
                    },
                    Some("mine") => self.player.remove_last_by_user(&user.name)?,
                    Some(n) => {
                        ctx.check_moderator(&user)?;
                        let n = parse_queue_position(&user, n)?;
                        self.player.remove_at(n)?
                    }
                    None => {
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
                        ctx.check_moderator(&user)?;

                        let (diff, argument) = match other.chars().next() {
                            Some('+') => (Some(true), &other[1..]),
                            Some('-') => (Some(false), &other[1..]),
                            _ => (None, other),
                        };

                        let argument = match str::parse::<u32>(argument) {
                            Ok(argument) => argument,
                            Err(_) => {
                                user.respond("expected whole number argument");
                                failure::bail!("bad command");
                            }
                        };

                        let argument = match diff {
                            Some(true) => self.player.current_volume().saturating_add(argument),
                            Some(false) => self.player.current_volume().saturating_sub(argument),
                            None => argument,
                        };

                        // clamp the volume.
                        let argument = u32::min(100, argument);
                        user.respond(format!("Volume set to {}.", argument));
                        self.player.volume(argument)?;
                    }
                    // reading volume
                    None => {
                        user.respond(format!("Current volume: {}.", self.player.current_volume()));
                    }
                }
            }
            Some("skip") => {
                ctx.check_moderator(&user)?;
                self.player.skip()?;
            }
            Some("request") => {
                let q = it.rest();

                if !it.next().is_some() {
                    user.respond("expected: !song request <id>|<text>");
                    failure::bail!("bad command");
                }

                let track_id_future: BoxFuture<Option<player::TrackId>, failure::Error> =
                    match player::TrackId::from_url_or_uri(q) {
                        Ok(track_id) => Box::new(future::ok(Some(track_id))),
                        Err(e) => {
                            log::info!("Failed to parse as URL/URI: {}: {}", q, e);
                            Box::new(self.player.search_track(q))
                        }
                    };

                let future = track_id_future.and_then({
                    let user = user.as_owned_user();

                    move |track_id| match track_id {
                        None => {
                            user.respond("Could not find a track matching your request, sorry :(");
                            return Err(failure::format_err!("bad track in request"));
                        }
                        Some(track_id) => return Ok(track_id),
                    }
                });

                let future = future.map_err(|e| {
                    utils::log_err("failed to add track", e);
                    ()
                });

                let future = future
                    .and_then({
                        let is_moderator = ctx.is_moderator(&user);
                        let user = user.as_owned_user();
                        let player = self.player.clone();

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
                                    Err(player::AddTrackError::PlayerClosed(reason)) => {
                                        match reason {
                                            Some(reason) => {
                                                user.respond(reason.as_str());
                                            },
                                            None => {
                                                user.respond("Player is closed from further requests, sorry :(");
                                            }
                                        }
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
                                        utils::log_err("failed to add song", e);
                                    }
                                }

                                Ok(())
                            })
                        }
                    });

                ctx.spawn(future);
            }
            Some("toggle") => {
                ctx.check_moderator(&user)?;
                self.player.toggle()?;
            }
            Some("play") => {
                ctx.check_moderator(&user)?;
                self.player.play()?;
            }
            Some("pause") => {
                ctx.check_moderator(&user)?;
                self.player.pause()?;
            }
            Some("length") => {
                let (count, seconds) = self.player.length();

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
                if ctx.is_moderator(&user) {
                    user.respond("Expected: request, skip, play, pause, toggle, delete.");
                } else {
                    user.respond("Expected: !song request <request>, !song list, !song length, or !song delete mine.");
                }
            }
        }

        Ok(())
    }
}

/// Parse a queue position.
fn parse_queue_position(user: &irc::User<'_>, n: &str) -> Result<usize, failure::Error> {
    match str::parse::<usize>(n) {
        Ok(0) => {
            user.respond("Can't remove the current song :(");
            failure::bail!("bad command");
        }
        Ok(n) => Ok(n.saturating_sub(1)),
        Err(e) => {
            user.respond("Expected whole number argument");
            failure::bail!("bad whole number argument: {}", e);
        }
    }
}

/// Display the collection of songs.
fn display_songs(
    user: &irc::User<'_>,
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