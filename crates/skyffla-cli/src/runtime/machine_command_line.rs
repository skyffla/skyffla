use skyffla_protocol::room::{ChannelId, MachineCommand, MemberId, Route};

use crate::cli_error::CliError;

pub(super) fn parse_machine_command_line(line: &str) -> Result<MachineCommand, CliError> {
    let tokens = split_shell_words(line)?;
    let Some((head, tail)) = tokens.split_first() else {
        return Err(CliError::usage("machine command must not be empty"));
    };
    let head = head.strip_prefix('/').unwrap_or(head.as_str());
    match head {
        "file" => parse_file_command(tail),
        "channel" => parse_channel_command(tail),
        "chat" => parse_chat_command(tail),
        other => Err(CliError::usage(format!(
            "unknown machine command {other:?}; expected JSON or one of /file, /channel, /chat"
        ))),
    }
}

fn parse_file_command(tokens: &[String]) -> Result<MachineCommand, CliError> {
    let Some((subcommand, tail)) = tokens.split_first() else {
        return Err(CliError::usage(
            "file command requires a subcommand: send or export",
        ));
    };
    match subcommand.as_str() {
        "send" => {
            let mut channel_id = None;
            let mut to = None;
            let mut path = None;
            let mut name = None;
            let mut mime = None;
            let mut iter = tail.iter();
            while let Some(token) = iter.next() {
                match token.as_str() {
                    "--channel" => channel_id = Some(required_value(&mut iter, "--channel")?),
                    "--to" => to = Some(parse_route(&required_value(&mut iter, "--to")?)?),
                    "--path" => path = Some(required_value(&mut iter, "--path")?),
                    "--name" => name = Some(required_value(&mut iter, "--name")?),
                    "--mime" => mime = Some(required_value(&mut iter, "--mime")?),
                    other => {
                        return Err(CliError::usage(format!(
                            "unexpected file send argument {other:?}"
                        )));
                    }
                }
            }
            let command = MachineCommand::SendFile {
                channel_id: ChannelId::new(
                    channel_id.ok_or_else(|| CliError::usage("missing --channel"))?,
                )
                .map_err(|error| CliError::usage(error.to_string()))?,
                to: to.ok_or_else(|| CliError::usage("missing --to"))?,
                path: path.ok_or_else(|| CliError::usage("missing --path"))?,
                name,
                mime,
            };
            command
                .validate()
                .map_err(|error| CliError::usage(error.to_string()))?;
            Ok(command)
        }
        "export" => {
            let mut channel_id = None;
            let mut path = None;
            let mut iter = tail.iter();
            while let Some(token) = iter.next() {
                match token.as_str() {
                    "--channel" => channel_id = Some(required_value(&mut iter, "--channel")?),
                    "--path" => path = Some(required_value(&mut iter, "--path")?),
                    other => {
                        return Err(CliError::usage(format!(
                            "unexpected file export argument {other:?}"
                        )));
                    }
                }
            }
            let command = MachineCommand::ExportChannelFile {
                channel_id: ChannelId::new(
                    channel_id.ok_or_else(|| CliError::usage("missing --channel"))?,
                )
                .map_err(|error| CliError::usage(error.to_string()))?,
                path: path.ok_or_else(|| CliError::usage("missing --path"))?,
            };
            command
                .validate()
                .map_err(|error| CliError::usage(error.to_string()))?;
            Ok(command)
        }
        other => Err(CliError::usage(format!(
            "unknown file subcommand {other:?}; expected send or export"
        ))),
    }
}

fn parse_channel_command(tokens: &[String]) -> Result<MachineCommand, CliError> {
    let Some((subcommand, tail)) = tokens.split_first() else {
        return Err(CliError::usage(
            "channel command requires a subcommand: accept, reject, close, or send",
        ));
    };
    match subcommand.as_str() {
        "accept" => {
            let channel_id = first_required(tail, "channel accept requires <channel_id>")?;
            let command = MachineCommand::AcceptChannel {
                channel_id: ChannelId::new(channel_id.clone())
                    .map_err(|error| CliError::usage(error.to_string()))?,
            };
            command
                .validate()
                .map_err(|error| CliError::usage(error.to_string()))?;
            Ok(command)
        }
        "reject" => {
            let channel_id = first_required(tail, "channel reject requires <channel_id>")?;
            let reason = remainder_after_first(tail);
            let command = MachineCommand::RejectChannel {
                channel_id: ChannelId::new(channel_id.clone())
                    .map_err(|error| CliError::usage(error.to_string()))?,
                reason,
            };
            command
                .validate()
                .map_err(|error| CliError::usage(error.to_string()))?;
            Ok(command)
        }
        "close" => {
            let channel_id = first_required(tail, "channel close requires <channel_id>")?;
            let reason = remainder_after_first(tail);
            let command = MachineCommand::CloseChannel {
                channel_id: ChannelId::new(channel_id.clone())
                    .map_err(|error| CliError::usage(error.to_string()))?,
                reason,
            };
            command
                .validate()
                .map_err(|error| CliError::usage(error.to_string()))?;
            Ok(command)
        }
        "send" => {
            let channel_id = first_required(tail, "channel send requires <channel_id> <body>")?;
            let body = remainder_after_first(tail)
                .ok_or_else(|| CliError::usage("channel send requires <channel_id> <body>"))?;
            let command = MachineCommand::SendChannelData {
                channel_id: ChannelId::new(channel_id.clone())
                    .map_err(|error| CliError::usage(error.to_string()))?,
                body,
            };
            command
                .validate()
                .map_err(|error| CliError::usage(error.to_string()))?;
            Ok(command)
        }
        other => Err(CliError::usage(format!(
            "unknown channel subcommand {other:?}; expected accept, reject, close, or send"
        ))),
    }
}

fn parse_chat_command(tokens: &[String]) -> Result<MachineCommand, CliError> {
    if tokens.is_empty() {
        return Err(CliError::usage(
            "chat command requires --to <all|member_id> <text>",
        ));
    }
    let mut to = None;
    let mut idx = 0;
    while idx < tokens.len() {
        match tokens[idx].as_str() {
            "--to" => {
                idx += 1;
                let value = tokens
                    .get(idx)
                    .ok_or_else(|| CliError::usage("chat command expected a value after --to"))?;
                to = Some(parse_route(value)?);
                idx += 1;
            }
            _ => break,
        }
    }
    let text = tokens[idx..].join(" ");
    let command = MachineCommand::SendChat {
        to: to.ok_or_else(|| CliError::usage("missing --to"))?,
        text,
    };
    command
        .validate()
        .map_err(|error| CliError::usage(error.to_string()))?;
    Ok(command)
}

fn parse_route(value: &str) -> Result<Route, CliError> {
    if value == "all" {
        return Ok(Route::All);
    }
    Ok(Route::Member {
        member_id: MemberId::new(value.to_string())
            .map_err(|error| CliError::usage(error.to_string()))?,
    })
}

fn required_value<'a, I>(iter: &mut I, flag: &str) -> Result<String, CliError>
where
    I: Iterator<Item = &'a String>,
{
    iter.next()
        .cloned()
        .ok_or_else(|| CliError::usage(format!("expected a value after {flag}")))
}

fn first_required<'a>(tokens: &'a [String], message: &str) -> Result<&'a String, CliError> {
    tokens.first().ok_or_else(|| CliError::usage(message))
}

fn remainder_after_first(tokens: &[String]) -> Option<String> {
    let tail = tokens.get(1..)?;
    if tail.is_empty() {
        return None;
    }
    Some(tail.join(" "))
}

fn split_shell_words(line: &str) -> Result<Vec<String>, CliError> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = line.chars().peekable();
    let mut quote = None;

    while let Some(ch) = chars.next() {
        match quote {
            Some(active) => match ch {
                '\\' if active == '"' => {
                    let next = chars
                        .next()
                        .ok_or_else(|| CliError::usage("unfinished escape in command"))?;
                    current.push(next);
                }
                value if value == active => quote = None,
                _ => current.push(ch),
            },
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    let next = chars
                        .next()
                        .ok_or_else(|| CliError::usage("unfinished escape in command"))?;
                    current.push(next);
                }
                value if value.is_whitespace() => {
                    if !current.is_empty() {
                        out.push(std::mem::take(&mut current));
                    }
                }
                _ => current.push(ch),
            },
        }
    }

    if quote.is_some() {
        return Err(CliError::usage("unterminated quoted string in command"));
    }
    if !current.is_empty() {
        out.push(current);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_send_command_with_quotes() {
        let command = parse_machine_command_line(
            r#"/file send --channel f1 --to m2 --path "./folder name" --name "demo dir" --mime text/plain"#,
        )
        .unwrap();
        assert_eq!(
            command,
            MachineCommand::SendFile {
                channel_id: ChannelId::new("f1").unwrap(),
                to: Route::Member {
                    member_id: MemberId::new("m2").unwrap()
                },
                path: "./folder name".into(),
                name: Some("demo dir".into()),
                mime: Some("text/plain".into()),
            }
        );
    }

    #[test]
    fn parses_file_export_command() {
        let command =
            parse_machine_command_line(r#"/file export --channel f9 --path ./out/readme.txt"#)
                .unwrap();
        assert_eq!(
            command,
            MachineCommand::ExportChannelFile {
                channel_id: ChannelId::new("f9").unwrap(),
                path: "./out/readme.txt".into(),
            }
        );
    }

    #[test]
    fn parses_channel_accept_and_chat_commands() {
        let accept = parse_machine_command_line("/channel accept c1").unwrap();
        assert_eq!(
            accept,
            MachineCommand::AcceptChannel {
                channel_id: ChannelId::new("c1").unwrap()
            }
        );

        let chat = parse_machine_command_line(r#"/chat --to all "hello room""#).unwrap();
        assert_eq!(
            chat,
            MachineCommand::SendChat {
                to: Route::All,
                text: "hello room".into(),
            }
        );
    }

    #[test]
    fn rejects_unknown_commands() {
        let error = parse_machine_command_line("/nope").unwrap_err();
        assert!(error.to_string().contains("unknown machine command"));
    }
}
