use super::BotCmdAuthLvl;
use super::BotCmdResult;
use super::BotCommand;
use super::ErrorKind;
use super::MsgMetadata;
use super::MsgPrefix;
use super::MsgTarget;
use super::Reaction;
use super::Result;
use super::State;
use super::irc_msgs::PrivMsg;
use super::irc_msgs::parse_privmsg;
use super::irc_send;
use super::parse_msg_to_nick;
use irc::client::Reaction as LibReaction;
use irc::connection::prelude::*;
use itertools::Itertools;
use pircolate;
use std::borrow::Borrow;
use std::borrow::Cow;
use std::cmp;
use std::fmt::Display;
use std::iter;

const UPDATE_MSG_PREFIX_STR: &'static str = "!!! UPDATE MESSAGE PREFIX !!!";

impl<'server, 'modl> State<'server, 'modl> {
    fn say<S1, S2>(&self, target: MsgTarget, addressee: S1, msg: S2) -> Result<LibReaction>
    where
        S1: Borrow<str>,
        S2: Display,
    {
        let final_msg =
            format!(
            "{}{}{}",
            addressee.borrow(),
            if addressee.borrow().is_empty() {
                ""
            } else {
                &self.addressee_suffix
            },
            msg,
        );
        info!("Sending message to {:?}: {:?}", target, final_msg);
        let mut wrapped_msg = vec![];
        wrap_msg(self, target, &final_msg, |line| {
            use pircolate::message::client::priv_msg;
            Ok(wrapped_msg.push(
                LibReaction::RawMsg(priv_msg(target.0, line)?),
            ))
        })?;
        // TODO: optimize for case where no wrapping, and thus no `Vec`, is needed.
        Ok(LibReaction::Multi(wrapped_msg))
    }

    fn prefix_len(&self) -> Result<usize> {
        Ok(self.msg_prefix.read().len())
    }
}

fn wrap_msg<F>(state: &State, MsgTarget(target): MsgTarget, msg: &str, mut f: F) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    // :nick!user@host PRIVMSG target :message
    // :nick!user@host NOTICE target :message
    let raw_len_limit = 512;
    let punctuation_len = {
        let line_terminator_len = 2;
        let spaces = 3;
        let colons = 2;
        colons + spaces + line_terminator_len
    };
    let cmd_len = "PRIVMSG".len();
    let metadata_len = state.prefix_len()? + cmd_len + target.len() + punctuation_len;
    let msg_len_limit = raw_len_limit - metadata_len;

    if msg.len() < msg_len_limit {
        return f(msg);
    }

    let mut split_end_idx = 0;

    let lines = msg.match_indices(char::is_whitespace).peekable().batching(
        |mut iter| {
            debug_assert!(msg.len() >= msg_len_limit);

            let split_start_idx = split_end_idx;

            if split_start_idx >= msg.len() {
                return None;
            }

            while let Some(&(next_space_idx, _)) = iter.peek() {
                if msg[split_start_idx..next_space_idx].len() < msg_len_limit {
                    split_end_idx = next_space_idx;
                    iter.next();
                } else {
                    break;
                }
            }

            if iter.peek().is_none() {
                split_end_idx = msg.len()
            } else if split_end_idx <= split_start_idx {
                split_end_idx = cmp::min(split_start_idx + msg_len_limit, msg.len())
            }

            Some(msg[split_start_idx..split_end_idx].trim())
        },
    );

    for line in lines {
        f(line)?
    }

    Ok(())
}

fn handle_reaction(state: &State, msg: &PrivMsg, reaction: Reaction) -> Result<LibReaction> {
    let &PrivMsg {
        metadata: MsgMetadata {
            target,
            prefix: MsgPrefix { nick, .. },
        },
        ..
    } = msg;

    let (reply_target, reply_addressee) = if target.0 == state.nick()? {
        (MsgTarget(nick.unwrap()), "")
    } else {
        (target, nick.unwrap_or(""))
    };

    match reaction {
        Reaction::None => Ok(LibReaction::None),
        Reaction::Msg(s) => state.say(reply_target, "", &s),
        Reaction::Msgs(a) => {
            Ok(LibReaction::Multi(a.iter()
                .map(|s| state.say(reply_target, "", &s))
                .collect::<Result<_>>()?))
        }
        Reaction::Reply(s) => state.say(reply_target, reply_addressee, &s),
        Reaction::Replies(a) => {
            Ok(LibReaction::Multi(a.iter()
                .map(|s| state.say(reply_target, reply_addressee, &s))
                .collect::<Result<_>>()?))
        }
        Reaction::RawMsg(s) => Ok(LibReaction::RawMsg(s.parse()?)),
        Reaction::BotCmd(cmd_ln) => handle_bot_command(state, msg, cmd_ln),
        Reaction::Quit(msg) => bail!(ErrorKind::ModuleRequestedQuit(msg)),
    }
}

fn handle_bot_command<C>(state: &State, msg: &PrivMsg, command_line: C) -> Result<LibReaction>
where
    C: Borrow<str>,
{
    let cmd_ln = command_line.borrow();

    debug_assert!(!cmd_ln.trim().is_empty());

    let mut cmd_name_and_args = cmd_ln.splitn(2, char::is_whitespace);
    let cmd_name = cmd_name_and_args.next().unwrap_or("");
    let cmd_args = cmd_name_and_args.next().unwrap_or("");

    handle_reaction(
        state,
        msg,
        bot_command_reaction(state, msg, cmd_name, cmd_args),
    )
}

    fn run_bot_command(state: &State, &PrivMsg {ref metadata, ..}: &PrivMsg, &BotCommand {
                 ref name,
                 ref provider,
                 ref auth_lvl,
                 ref handler,
                 usage: _,
                 help_msg: _,
}: &BotCommand, cmd_args: &str) -> BotCmdResult{

    let user_authorized = match auth_lvl {
        &BotCmdAuthLvl::Public => Ok(true),
        &BotCmdAuthLvl::Admin => state.have_admin(metadata.prefix),
    };

    let result = match user_authorized {
        Ok(true) => handler.run(state, &metadata, cmd_args),
        Ok(false) => BotCmdResult::Unauthorized,
        Err(e) => BotCmdResult::LibErr(e),
    };

    match result {
        BotCmdResult::Ok(Reaction::Quit(ref s)) if *auth_lvl != BotCmdAuthLvl::Admin => {
            BotCmdResult::BotErrMsg(
                format!(
                    "Only commands at authorization level {auth_lvl_owner:?} may tell the bot to \
                     quit, but the command {cmd_name:?} from module {provider_name:?}, at \
                     authorization level {cmd_auth_lvl:?}, has told the bot to quit with quit \
                     message {quit_msg:?}.",
                    auth_lvl_owner = BotCmdAuthLvl::Admin,
                    cmd_name = name,
                    provider_name = provider.name,
                    cmd_auth_lvl = auth_lvl,
                    quit_msg = s
                ).into(),
            )
        }
        r => r,
    }
}

fn bot_command_reaction(state: &State, msg: &PrivMsg, cmd_name: &str, cmd_args: &str) -> Reaction {
    let cmd = match state.commands.get(cmd_name) {
        Some(c) => c,
        None => {
            return Reaction::Reply(format!("Unknown command {:?}; apologies.", cmd_name).into())
        }
    };

    let &BotCommand {
        ref name,
        ref usage,
        ..
    } = cmd;

    let cmd_result = match run_bot_command(state, msg, cmd, cmd_args) {
        BotCmdResult::Ok(r) => Ok(r),
        BotCmdResult::Unauthorized => {
            Err(format!(
                "My apologies, but you do not appear to have sufficient \
                 authority to use my {:?} command.",
                name
            ))
        }
        BotCmdResult::SyntaxErr => Err(format!("Syntax: {} {}", name, usage)),
        BotCmdResult::ArgMissing(arg_name) => {
            Err(format!(
                "Syntax error: For command {:?}, the argument {:?} is \
                 required, but it was not given.",
                name,
                arg_name
            ))
        }
        BotCmdResult::ArgMissing1To1(arg_name) => {
            Err(format!(
                "Syntax error: When command {:?} is used outside of a \
                 channel, the argument {:?} is required, but it was not \
                 given.",
                name,
                arg_name
            ))
        }
        BotCmdResult::LibErr(e) => Err(format!("Error: {}", e)),
        BotCmdResult::UserErrMsg(s) => Err(format!("User error: {}", s)),
        BotCmdResult::BotErrMsg(s) => Err(format!("Internal error: {}", s)),
    };

    match cmd_result {
        Ok(r) => r,
        Err(s) => Reaction::Reply(s.into()),
    }
}

pub fn quit<'a>(state: &State, msg: Option<Cow<'a, str>>) -> LibReaction {
    let default_quit_msg = format!(
        "<{}> v{}",
        env!("CARGO_PKG_HOMEPAGE"),
        env!("CARGO_PKG_VERSION")
    );

    let msg: Option<&str> = msg.as_ref().map(Borrow::borrow);

    info!("Quitting. Quit message: {:?}.", msg);

    let quit = match format!("QUIT :{}", msg.unwrap_or(&default_quit_msg))
        .parse()
        .map_err(Into::into) {
        Ok(m) => m,
        Err(e) => {
            (state.error_handler)(e);
            error!("Failed to construct quit message.");
            return LibReaction::None;
        }
    };

    LibReaction::RawMsg(quit)
}

pub fn handle_msg(state: &State, input_msg: pircolate::Message) -> Result<LibReaction> {
    if let Some(msg) = parse_privmsg(&input_msg) {
        handle_privmsg(state, &msg)
    } else if let Some(pircolate::command::ServerInfo(..)) =
        input_msg.command::<pircolate::command::ServerInfo>()
    {
        handle_004(state)
    } else {
        Ok(LibReaction::None)
    }
}

fn handle_privmsg(state: &State, msg: &PrivMsg) -> Result<LibReaction> {
    trace!("Handling PRIVMSG: {:?}", msg);

    let &PrivMsg {
        metadata: MsgMetadata {
            ref target,
            ref prefix,
        },
        text,
    } = msg;

    let msg_for_bot = match parse_msg_to_nick(state, msg, &state.nick()?) {
        Some(m) => m,
        None => return Ok(LibReaction::None),
    };

    if msg_for_bot.is_empty() {
        handle_reaction(state, msg, Reaction::Reply("Yes?".into()))
    } else if prefix.nick == Some(target.0) && text == UPDATE_MSG_PREFIX_STR {
        update_prefix_info(state, prefix)
    } else {
        handle_bot_command(state, msg, msg_for_bot)
    }
}

fn update_prefix_info(state: &State, prefix: &MsgPrefix) -> Result<LibReaction> {
    debug!(
        "Updating stored message prefix information from received {:?}",
        prefix
    );

    state.msg_prefix.write().update_from(prefix);

    Ok(LibReaction::None)
}

fn handle_004(state: &State) -> Result<LibReaction> {
    // The server has finished sending the protocol-mandated welcome messages.

    send_msg_prefix_update_request(state)
}

fn send_msg_prefix_update_request(state: &State) -> Result<LibReaction> {
    use pircolate::message::client::priv_msg;

    Ok(LibReaction::RawMsg(
        priv_msg(&state.nick()?, UPDATE_MSG_PREFIX_STR)?,
    ))
}

fn connection_sequence(state: &State) -> Result<Vec<pircolate::Message>> {
    use pircolate::message::client::nick;
    use pircolate::message::client::user;

    Ok(vec![
        nick(&state.config.nick)?,
        user(&state.config.username, &state.config.realname)?,
    ])
}
