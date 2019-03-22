use crate::{command, db, irc, utils};
use failure::format_err;

/// Handler for the !badword command.
pub struct BadWord {
    pub bad_words: db::Words<db::Database>,
}

impl command::Handler for BadWord {
    fn handle<'m>(
        &mut self,
        mut ctx: command::Context<'_>,
        user: irc::User<'m>,
        it: &mut utils::Words<'m>,
    ) -> Result<(), failure::Error> {
        match it.next() {
            Some("edit") => {
                ctx.check_moderator(&user)?;

                let word = it.next().ok_or_else(|| format_err!("expected word"))?;
                let why = match it.rest() {
                    "" => None,
                    other => Some(other),
                };

                self.bad_words.edit(word, why)?;
                user.respond("Bad word edited");
            }
            Some("delete") => {
                ctx.check_moderator(&user)?;

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
}