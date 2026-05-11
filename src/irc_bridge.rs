#![allow(
    clippy::collapsible_if,
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::large_enum_variant,
    clippy::enum_variant_names,
    clippy::let_and_return,
)]

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use futures::prelude::*;
use irc::client::prelude::*;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Parser)]
#[command(
    name = "bro-irc",
    about = "LAN IRC bridge for blackbox bro orchestration"
)]
struct Args {
    /// IRC server host. Defaults to the LAN bind address used by deploy/irc.
    #[arg(long, default_value = "192.168.0.251")]
    irc_host: String,
    /// IRC server port.
    #[arg(long, default_value_t = 6667)]
    irc_port: u16,
    /// Bot nickname.
    #[arg(long, default_value = "blackbox")]
    nick: String,
    /// Channel to join and relay bro activity into.
    #[arg(long, default_value = "#bros")]
    channel: String,
    /// blackboxd base URL. This should normally stay loopback-only.
    #[arg(long, default_value = "http://127.0.0.1:7264")]
    daemon_url: String,
    /// Command prefix accepted from IRC.
    #[arg(long, default_value = "!")]
    prefix: String,
    /// Optional comma-separated nick allowlist. Empty means permissive LAN mode.
    #[arg(long)]
    allow_nicks: Option<String>,
    /// Announce command help in the main channel on startup.
    #[arg(long, default_value_t = false)]
    announce: bool,
}

#[derive(Debug, Deserialize)]
struct ToolResult {
    #[serde(default)]
    content: Vec<Value>,
    #[serde(default, rename = "isError")]
    is_error: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ExecRequest {
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bro: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_dir: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResumeRequest {
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bro: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_dir: Option<String>,
}

#[derive(Debug, Serialize)]
struct BroadcastRequest {
    team: String,
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_dir: Option<String>,
}

#[derive(Debug, Serialize)]
struct CancelRequest {
    task_id: String,
}

#[derive(Debug, Serialize)]
struct CreateCouncilRequest {
    team: String,
    topic: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    charter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateCouncilResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
struct CouncilListEntry {
    id: String,
    team_id: String,
    topic: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct TeamMembersResponse {
    members: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CouncilPostRequest {
    body: String,
    sender: String,
}

#[derive(Clone)]
struct BridgeState {
    server: IrcServerConfig,
    council_channels: Arc<Mutex<HashMap<String, String>>>,
    instances: Arc<Mutex<HashMap<String, CouncilInstances>>>,
}

#[derive(Clone)]
struct IrcServerConfig {
    host: String,
    port: u16,
}

#[derive(Clone, Default)]
struct CouncilInstances {
    by_member: HashMap<String, BroInstance>,
    nick_to_member: HashMap<String, String>,
}

#[derive(Clone)]
struct BroInstance {
    nick: String,
    tx: tokio::sync::mpsc::UnboundedSender<InstanceOutbound>,
}

#[derive(Debug)]
enum InstanceOutbound {
    Privmsg { target: String, body: String },
    Join { channel: String },
    Part { channel: String },
}

impl BridgeState {
    fn new(server: IrcServerConfig) -> Self {
        Self {
            server,
            council_channels: Arc::new(Mutex::new(HashMap::new())),
            instances: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn is_instance_nick(&self, nick: &str) -> bool {
        let nick = nick.to_ascii_lowercase();
        self.instances
            .lock()
            .expect("instance map")
            .values()
            .any(|instances| instances.nick_to_member.contains_key(&nick))
    }
}

#[derive(Debug)]
enum Outbound {
    Privmsg { target: String, body: String },
    Join { channel: String },
    Part { channel: String },
    Topic { channel: String, topic: String },
    Invite { nick: String, channel: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let allow_nicks = parse_allow_nicks(args.allow_nicks.as_deref());
    let daemon = Url::parse(&args.daemon_url).context("parse daemon url")?;

    let config = Config {
        nickname: Some(args.nick.clone()),
        username: Some(args.nick.clone()),
        realname: Some("blackbox bro IRC bridge".to_string()),
        server: Some(args.irc_host.clone()),
        port: Some(args.irc_port),
        channels: vec![args.channel.clone()],
        ..Default::default()
    };

    let mut client = Client::from_config(config).await?;
    client.identify()?;

    let sender = client.sender();
    let mut stream = client.stream()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Outbound>();
    let bridge_state = BridgeState::new(IrcServerConfig {
        host: args.irc_host.clone(),
        port: args.irc_port,
    });

    tokio::spawn(tail_loop(daemon.clone(), args.channel.clone(), tx.clone()));
    tokio::spawn(restore_council_channels_loop(
        daemon.clone(),
        bridge_state.clone(),
        tx.clone(),
    ));

    if args.announce {
        sender.send_privmsg(&args.channel, help_text())?;
    }

    loop {
        tokio::select! {
            maybe_msg = stream.next() => {
                let Some(message) = maybe_msg.transpose()? else {
                    return Ok(());
                };
                if let Command::PRIVMSG(ref target, ref body) = message.command {
                    let nick = message.source_nickname().unwrap_or("unknown");
                    if !allowed(&allow_nicks, nick) {
                        continue;
                    }
                    if bridge_state.is_instance_nick(nick) {
                        continue;
                    }
                    if let Some(command) = body.strip_prefix(&args.prefix) {
                        let reply_target = if target == &args.nick { nick } else { target };
                        if let Err(e) = handle_command(&daemon, &args.channel, &bridge_state, nick, target, command.trim(), reply_target, &tx).await {
                            tx.send(Outbound::Privmsg {
                                target: reply_target.to_string(),
                                body: format!("error: {e:#}"),
                            }).ok();
                        }
                    } else if nick != args.nick
                        && let Some(council_id) = ensure_council_for_target(&bridge_state, &tx, daemon.clone(), target) {
                            let client = reqwest::Client::new();
                            let body = normalize_instance_mentions(&bridge_state, target, body);
                            if let Err(e) = post_council_turn(&client, &daemon, &council_id, nick, &body).await {
                                tx.send(Outbound::Privmsg {
                                    target: target.to_string(),
                                    body: format!("error: {e:#}"),
                                }).ok();
                            }
                        }
                }
            }
            Some(outbound) = rx.recv() => {
                match outbound {
                    Outbound::Privmsg { target, body } => {
                        for line in irc_lines(&body) {
                            sender.send_privmsg(&target, &line)?;
                        }
                    }
                    Outbound::Join { channel } => {
                        sender.send_join(&channel)?;
                    }
                    Outbound::Part { channel } => {
                        sender.send_part(&channel)?;
                    }
                    Outbound::Topic { channel, topic } => {
                        sender.send_topic(&channel, &topic)?;
                    }
                    Outbound::Invite { nick, channel } => {
                        sender.send_invite(&nick, &channel)?;
                    }
                }
            }
        }
    }
}

async fn handle_command(
    daemon: &Url,
    bridge_channel: &str,
    bridge_state: &BridgeState,
    nick: &str,
    command_target: &str,
    command: &str,
    reply_target: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<Outbound>,
) -> Result<()> {
    let client = reqwest::Client::new();
    let mut parts = command.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();

    let reply = match verb {
        "help" => help_text(),
        "dashboard" | "tasks" => {
            let result = get_tool(&client, daemon, "irc/dashboard?limit=8").await?;
            summarize_dashboard(&result)
        }
        "rooms" | "threads" => {
            let councils: Vec<CouncilListEntry> = get_json(&client, daemon, "council").await?;
            summarize_council_rooms(&councils)
        }
        "where" => {
            let Some(council_id) = council_for_target(bridge_state, command_target) else {
                return Ok(());
            };
            let councils: Vec<CouncilListEntry> = get_json(&client, daemon, "council").await?;
            summarize_council_room(&councils, &council_id)
        }
        "status" => {
            let task_id = require_arg(rest, "usage: !status <task-id>")?;
            let path = format!("irc/status/{task_id}?tail=3");
            let result = get_tool(&client, daemon, &path).await?;
            summarize_status(&result)
        }
        "cancel" => {
            let task_id = require_arg(rest, "usage: !cancel <task-id>")?;
            let result = post_tool(
                &client,
                daemon,
                "irc/cancel",
                &CancelRequest {
                    task_id: task_id.to_string(),
                },
            )
            .await?;
            summarize_task_launch("cancel", &result)
        }
        "exec" => {
            let (bro, prompt) = split_two(rest, "usage: !exec <bro> <prompt>")?;
            let prompt = build_irc_prompt(nick, bridge_channel, prompt);
            let result = post_tool(
                &client,
                daemon,
                "irc/exec",
                &ExecRequest {
                    prompt,
                    bro: Some(bro.to_string()),
                    provider: None,
                    project_dir: None,
                },
            )
            .await?;
            summarize_task_launch(bro, &result)
        }
        "provider" => {
            let (provider, prompt) = split_two(rest, "usage: !provider <provider> <prompt>")?;
            let prompt = build_irc_prompt(nick, bridge_channel, prompt);
            let result = post_tool(
                &client,
                daemon,
                "irc/exec",
                &ExecRequest {
                    prompt,
                    bro: None,
                    provider: Some(provider.to_string()),
                    project_dir: None,
                },
            )
            .await?;
            summarize_task_launch(provider, &result)
        }
        "team" => {
            let (team, prompt) = split_two(rest, "usage: !team <team> <prompt>")?;
            let prompt = build_irc_prompt(nick, bridge_channel, prompt);
            let result = post_tool(
                &client,
                daemon,
                "irc/broadcast",
                &BroadcastRequest {
                    team: team.to_string(),
                    prompt,
                    project_dir: None,
                },
            )
            .await?;
            summarize_team_launch(team, &result)
        }
        "thread" | "room" | "council" => {
            let (team, topic) = split_two(rest, "usage: !thread <team> <topic>")?;
            create_council_thread(&client, daemon, bridge_state, tx, nick, team, topic).await?
        }
        "attach" => {
            let council_id = require_arg(rest, "usage: !attach <council-id>")?;
            if !command_target.starts_with('#') {
                return Err(anyhow!("!attach must be used inside the channel to bind"));
            }
            bind_council_channel(
                bridge_state,
                tx,
                daemon.clone(),
                command_target.to_string(),
                council_id.to_string(),
            );
            format!(
                "{command_target}: attached to {council_id}; plain messages now go to the council"
            )
        }
        "close" => {
            let council_id = if rest.is_empty() {
                council_for_target(bridge_state, command_target)
                    .ok_or_else(|| anyhow!("usage: !close [council-id]"))?
            } else {
                rest.to_string()
            };
            close_council_room(&client, daemon, bridge_state, tx, &council_id).await?
        }
        "resume" => {
            let (target, prompt) =
                split_two(rest, "usage: !resume <bro|provider:session-id> <prompt>")?;
            let prompt = build_irc_prompt(nick, bridge_channel, prompt);
            let req = if let Some((provider, session_id)) = target.split_once(':') {
                ResumeRequest {
                    prompt,
                    bro: None,
                    session_id: Some(session_id.to_string()),
                    provider: Some(provider.to_string()),
                    project_dir: None,
                }
            } else {
                ResumeRequest {
                    prompt,
                    bro: Some(target.to_string()),
                    session_id: None,
                    provider: None,
                    project_dir: None,
                }
            };
            let result = post_tool(&client, daemon, "irc/resume", &req).await?;
            summarize_task_launch(target, &result)
        }
        "" => return Ok(()),
        other => format!("unknown command: {other}. try !help"),
    };

    if !reply.trim().is_empty() {
        tx.send(Outbound::Privmsg {
            target: reply_target.to_string(),
            body: reply,
        })
        .ok();
    }
    Ok(())
}

async fn tail_loop(daemon: Url, channel: String, tx: tokio::sync::mpsc::UnboundedSender<Outbound>) {
    let client = reqwest::Client::new();
    let mut labels = HashMap::new();
    let mut last_outputs = HashMap::new();
    loop {
        if let Err(e) = tail_once(
            &client,
            &daemon,
            &channel,
            &tx,
            &mut labels,
            &mut last_outputs,
        )
        .await
        {
            eprintln!("tail disconnected: {e:#}; reconnecting");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

async fn tail_once(
    client: &reqwest::Client,
    daemon: &Url,
    channel: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    labels: &mut HashMap<String, String>,
    last_outputs: &mut HashMap<String, String>,
) -> Result<()> {
    let mut url = daemon.clone();
    url.set_path("tail");
    let resp = client
        .get(url)
        .header("Accept", "text/event-stream")
        .send()
        .await?
        .error_for_status()?;
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(split) = buf.find("\n\n") {
            let frame = buf[..split].to_string();
            buf.drain(..split + 2);
            let payload = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<String>();
            if payload.is_empty() {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(&payload)
                && let Some(line) =
                    summarize_tail_event(client, daemon, &value, labels, last_outputs).await
                {
                    tx.send(Outbound::Privmsg {
                        target: channel.to_string(),
                        body: line,
                    })
                    .ok();
                }
        }
    }
    Err(anyhow!("tail stream ended"))
}

async fn post_tool<T: Serialize>(
    client: &reqwest::Client,
    daemon: &Url,
    path: &str,
    body: &T,
) -> Result<Value> {
    let url = daemon_path(daemon, path);
    let tool: ToolResult = client
        .post(url)
        .json(body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    tool_text_value(tool)
}

async fn get_tool(client: &reqwest::Client, daemon: &Url, path: &str) -> Result<Value> {
    let url = daemon_path(daemon, path);
    let tool: ToolResult = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    tool_text_value(tool)
}

async fn post_json<T, R>(client: &reqwest::Client, daemon: &Url, path: &str, body: &T) -> Result<R>
where
    T: Serialize,
    R: for<'de> Deserialize<'de>,
{
    let url = daemon_path(daemon, path);
    let resp = client.post(url).json(body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("{status}: {text}"));
    }
    resp.json().await.map_err(Into::into)
}

async fn get_json<R>(client: &reqwest::Client, daemon: &Url, path: &str) -> Result<R>
where
    R: for<'de> Deserialize<'de>,
{
    let url = daemon_path(daemon, path);
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("{status}: {text}"));
    }
    resp.json().await.map_err(Into::into)
}

async fn delete_json(client: &reqwest::Client, daemon: &Url, path: &str) -> Result<()> {
    let url = daemon_path(daemon, path);
    let resp = client.delete(url).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow!("{status}: {text}"));
    }
    Ok(())
}

async fn restore_council_channels_loop(
    daemon: Url,
    bridge_state: BridgeState,
    tx: tokio::sync::mpsc::UnboundedSender<Outbound>,
) {
    let client = reqwest::Client::new();
    loop {
        match restore_council_channels_once(&client, &daemon, &bridge_state, &tx).await {
            Ok(()) => tokio::time::sleep(Duration::from_secs(60)).await,
            Err(e) => {
                eprintln!("restore council channels failed: {e:#}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn restore_council_channels_once(
    client: &reqwest::Client,
    daemon: &Url,
    bridge_state: &BridgeState,
    tx: &tokio::sync::mpsc::UnboundedSender<Outbound>,
) -> Result<()> {
    let councils: Vec<CouncilListEntry> = get_json(client, daemon, "council").await?;
    for council in councils {
        if council.status != "open" {
            continue;
        }
        let channel = council_channel(&council.id);
        ensure_council_instances(
            client,
            daemon,
            bridge_state,
            &council.id,
            &council.team_id,
            &channel,
        )
        .await?;
        let is_new = bind_council_channel(
            bridge_state,
            tx,
            daemon.clone(),
            channel.clone(),
            council.id.clone(),
        );
        if is_new {
            tx.send(Outbound::Join {
                channel: channel.clone(),
            })
            .ok();
            tx.send(Outbound::Topic {
                channel,
                topic: format!("{}: {}", council.team_id, one_line(&council.topic, 160)),
            })
            .ok();
        }
    }
    Ok(())
}

async fn create_council_thread(
    client: &reqwest::Client,
    daemon: &Url,
    bridge_state: &BridgeState,
    tx: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    nick: &str,
    team: &str,
    topic: &str,
) -> Result<String> {
    let created: CreateCouncilResponse = post_json(
        client,
        daemon,
        "council",
        &CreateCouncilRequest {
            team: team.to_string(),
            topic: topic.to_string(),
            charter: Some(irc_council_charter().to_string()),
            project: None,
        },
    )
    .await?;
    let channel = council_channel(&created.id);
    ensure_council_instances(client, daemon, bridge_state, &created.id, team, &channel).await?;
    let is_new = bind_council_channel(
        bridge_state,
        tx,
        daemon.clone(),
        channel.clone(),
        created.id.clone(),
    );
    if is_new {
        tx.send(Outbound::Join {
            channel: channel.clone(),
        })
        .ok();
        tx.send(Outbound::Topic {
            channel: channel.clone(),
            topic: format!("{team}: {}", one_line(topic, 160)),
        })
        .ok();
    }
    tx.send(Outbound::Invite {
        nick: nick.to_string(),
        channel: channel.clone(),
    })
    .ok();
    Ok(format!(
        "{created_id}: join {channel}; plain messages there go to {team}",
        created_id = created.id
    ))
}

async fn ensure_council_instances(
    client: &reqwest::Client,
    daemon: &Url,
    bridge_state: &BridgeState,
    council_id: &str,
    team: &str,
    channel: &str,
) -> Result<()> {
    let roster: TeamMembersResponse = get_json(client, daemon, &format!("irc/team/{team}")).await?;
    let mut used = HashSet::new();
    let mut to_spawn = Vec::new();

    {
        let mut all = bridge_state.instances.lock().expect("instance map");
        let instances = all.entry(council_id.to_string()).or_default();
        for (idx, member) in roster.members.iter().enumerate() {
            if let Some(existing) = instances.by_member.get(member) {
                used.insert(existing.nick.to_ascii_lowercase());
                existing
                    .tx
                    .send(InstanceOutbound::Join {
                        channel: channel.to_string(),
                    })
                    .ok();
                continue;
            }
            let nick = unique_instance_nick(member, council_id, idx, &mut used);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            instances
                .nick_to_member
                .insert(nick.to_ascii_lowercase(), member.clone());
            instances.by_member.insert(
                member.clone(),
                BroInstance {
                    nick: nick.clone(),
                    tx: tx.clone(),
                },
            );
            to_spawn.push((member.clone(), nick, tx, rx));
        }
    }

    for (_member, nick, tx, rx) in to_spawn {
        tokio::spawn(instance_loop(bridge_state.server.clone(), nick, rx));
        tx.send(InstanceOutbound::Join {
            channel: channel.to_string(),
        })
        .ok();
    }

    Ok(())
}

async fn instance_loop(
    server: IrcServerConfig,
    nick: String,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<InstanceOutbound>,
) {
    loop {
        match run_instance_once(&server, &nick, &mut rx).await {
            Ok(()) => return,
            Err(e) => {
                eprintln!("irc instance {nick} disconnected: {e:#}; reconnecting");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn run_instance_once(
    server: &IrcServerConfig,
    nick: &str,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<InstanceOutbound>,
) -> Result<()> {
    let config = Config {
        nickname: Some(nick.to_string()),
        username: Some(nick.to_string()),
        realname: Some(format!("blackbox bro instance {nick}")),
        server: Some(server.host.clone()),
        port: Some(server.port),
        ..Default::default()
    };
    let mut client = Client::from_config(config).await?;
    client.identify()?;
    let sender = client.sender();
    let mut stream = client.stream()?;

    loop {
        tokio::select! {
            maybe_msg = stream.next() => {
                let Some(message) = maybe_msg.transpose()? else {
                    return Err(anyhow!("instance stream ended"));
                };
                if let Command::INVITE(_, ref channel) = message.command {
                    sender.send_join(channel)?;
                }
            }
            outbound = rx.recv() => {
                let Some(outbound) = outbound else {
                    return Ok(());
                };
                match outbound {
                    InstanceOutbound::Privmsg { target, body } => {
                        for line in irc_lines(&body) {
                            sender.send_privmsg(&target, &line)?;
                        }
                    }
                    InstanceOutbound::Join { channel } => {
                        sender.send_join(&channel)?;
                    }
                    InstanceOutbound::Part { channel } => {
                        sender.send_part(&channel)?;
                    }
                }
            }
        }
    }
}

async fn close_council_room(
    client: &reqwest::Client,
    daemon: &Url,
    bridge_state: &BridgeState,
    tx: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    council_id: &str,
) -> Result<String> {
    delete_json(client, daemon, &format!("council/{council_id}")).await?;
    let channels = unbind_council(bridge_state, council_id);
    let instances = remove_council_instances(bridge_state, council_id);
    let part_channel = if channels.is_empty() {
        council_channel(council_id)
    } else {
        channels[0].clone()
    };
    tx.send(Outbound::Part {
        channel: part_channel.clone(),
    })
    .ok();
    for instance in instances {
        instance
            .tx
            .send(InstanceOutbound::Part {
                channel: part_channel.clone(),
            })
            .ok();
    }
    Ok(format!("closed {council_id}; parted {part_channel}"))
}

async fn post_council_turn(
    client: &reqwest::Client,
    daemon: &Url,
    council_id: &str,
    sender: &str,
    body: &str,
) -> Result<()> {
    let _: Value = post_json(
        client,
        daemon,
        &format!("council/{council_id}/post"),
        &CouncilPostRequest {
            body: body.to_string(),
            sender: sender.to_string(),
        },
    )
    .await?;
    Ok(())
}

fn bind_council_channel(
    bridge_state: &BridgeState,
    tx: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    daemon: Url,
    channel: String,
    council_id: String,
) -> bool {
    let channel_key = normalize_channel(&channel);
    let should_spawn = {
        let mut channels = bridge_state.council_channels.lock().expect("council map");
        channels.insert(channel_key, council_id.clone()).is_none()
    };
    if should_spawn {
        tokio::spawn(council_tail_loop(
            daemon,
            bridge_state.clone(),
            channel,
            council_id,
            tx.clone(),
        ));
    }
    should_spawn
}

fn unbind_council(bridge_state: &BridgeState, council_id: &str) -> Vec<String> {
    let mut removed = Vec::new();
    let mut channels = bridge_state.council_channels.lock().expect("council map");
    channels.retain(|channel, bound| {
        if bound == council_id {
            removed.push(channel.clone());
            false
        } else {
            true
        }
    });
    removed
}

fn remove_council_instances(bridge_state: &BridgeState, council_id: &str) -> Vec<BroInstance> {
    bridge_state
        .instances
        .lock()
        .expect("instance map")
        .remove(council_id)
        .map(|instances| instances.by_member.into_values().collect())
        .unwrap_or_default()
}

fn channel_is_bound_to(bridge_state: &BridgeState, channel: &str, council_id: &str) -> bool {
    let key = normalize_channel(channel);
    bridge_state
        .council_channels
        .lock()
        .expect("council map")
        .get(&key)
        .map(|bound| bound == council_id)
        .unwrap_or(false)
}

fn council_for_target(bridge_state: &BridgeState, target: &str) -> Option<String> {
    if !target.starts_with('#') {
        return None;
    }
    let key = normalize_channel(target);
    bridge_state
        .council_channels
        .lock()
        .expect("council map")
        .get(&key)
        .cloned()
        .or_else(|| parse_council_channel(target))
}

fn ensure_council_for_target(
    bridge_state: &BridgeState,
    tx: &tokio::sync::mpsc::UnboundedSender<Outbound>,
    daemon: Url,
    target: &str,
) -> Option<String> {
    let existing = council_for_target(bridge_state, target)?;
    let key = normalize_channel(target);
    let already_bound = bridge_state
        .council_channels
        .lock()
        .expect("council map")
        .contains_key(&key);
    if !already_bound {
        bind_council_channel(
            bridge_state,
            tx,
            daemon,
            target.to_string(),
            existing.clone(),
        );
    }
    Some(existing)
}

async fn council_tail_loop(
    daemon: Url,
    bridge_state: BridgeState,
    channel: String,
    council_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<Outbound>,
) {
    let client = reqwest::Client::new();
    loop {
        if !channel_is_bound_to(&bridge_state, &channel, &council_id) {
            return;
        }
        if let Err(e) =
            council_tail_once(&client, &daemon, &bridge_state, &channel, &council_id, &tx).await
        {
            eprintln!("council tail disconnected: {e:#}; reconnecting");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}

async fn council_tail_once(
    client: &reqwest::Client,
    daemon: &Url,
    bridge_state: &BridgeState,
    channel: &str,
    council_id: &str,
    tx: &tokio::sync::mpsc::UnboundedSender<Outbound>,
) -> Result<()> {
    let mut url = daemon.clone();
    url.set_path(&format!("council/{council_id}/tail"));
    let resp = client
        .get(url)
        .header("Accept", "text/event-stream")
        .send()
        .await?
        .error_for_status()?;
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buf.push_str(&String::from_utf8_lossy(&bytes));
        while let Some(split) = buf.find("\n\n") {
            let frame = buf[..split].to_string();
            buf.drain(..split + 2);
            let payload = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<String>();
            if payload.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            if let Some(relay) = summarize_council_event(&value) {
                if let Some(sender) = relay.sender {
                    send_instance_privmsg(
                        bridge_state,
                        council_id,
                        &sender,
                        channel,
                        &display_instance_mentions(bridge_state, council_id, &relay.body),
                    );
                } else {
                    tx.send(Outbound::Privmsg {
                        target: channel.to_string(),
                        body: relay.body,
                    })
                    .ok();
                }
            }
        }
    }
    Err(anyhow!("council tail stream ended"))
}

struct CouncilRelay {
    sender: Option<String>,
    body: String,
}

fn summarize_council_event(v: &Value) -> Option<CouncilRelay> {
    match v.get("kind")?.as_str()? {
        "post" => {
            let post = v.get("post")?;
            let kind = post.get("sender_kind").and_then(Value::as_str)?;
            if kind != "bro" && kind != "system" {
                return None;
            }
            let sender = post
                .get("sender_id")
                .and_then(Value::as_str)
                .unwrap_or("bro");
            let body = post.get("body").and_then(Value::as_str)?.trim();
            if body.is_empty() {
                None
            } else if kind == "system" {
                Some(CouncilRelay {
                    sender: None,
                    body: body.to_string(),
                })
            } else {
                Some(CouncilRelay {
                    sender: Some(sender.to_string()),
                    body: body.to_string(),
                })
            }
        }
        "closed" => Some(CouncilRelay {
            sender: None,
            body: "council closed".to_string(),
        }),
        _ => None,
    }
}

fn council_channel(council_id: &str) -> String {
    format!("#{council_id}")
}

fn parse_council_channel(channel: &str) -> Option<String> {
    let rest = channel.strip_prefix("#council-")?;
    if rest.len() == 8 && rest.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(format!("council-{rest}"))
    } else {
        None
    }
}

fn unique_instance_nick(
    member: &str,
    council_id: &str,
    index: usize,
    used: &mut HashSet<String>,
) -> String {
    for attempt in 0..10 {
        let candidate = candidate_instance_nick(member, council_id, attempt);
        if used.insert(candidate.to_ascii_lowercase()) {
            return candidate;
        }
    }
    let fallback = format!("bro{:06}", index % 1_000_000);
    used.insert(fallback.clone());
    fallback
}

fn candidate_instance_nick(member: &str, council_id: &str, index: usize) -> String {
    const NICK_LIMIT: usize = 9;
    let suffix = council_id
        .strip_prefix("council-")
        .unwrap_or(council_id)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(2)
        .collect::<String>()
        .to_ascii_lowercase();
    let suffix = if suffix.is_empty() {
        "xx".to_string()
    } else {
        suffix
    };
    let discriminator = (index > 0).then(|| char::from(b'0' + (index % 10) as u8));
    let reserved = 1 + suffix.len() + discriminator.map(|_| 1).unwrap_or(0);
    let base_len = NICK_LIMIT.saturating_sub(reserved).max(1);
    let base = member
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(base_len)
        .collect::<String>()
        .to_ascii_lowercase();
    let base = if base.is_empty() {
        "bro".to_string()
    } else {
        base
    };
    match discriminator {
        Some(d) => format!("{base}-{suffix}{d}"),
        None => format!("{base}-{suffix}"),
    }
}

fn send_instance_privmsg(
    bridge_state: &BridgeState,
    council_id: &str,
    member: &str,
    target: &str,
    body: &str,
) -> bool {
    let tx = bridge_state
        .instances
        .lock()
        .expect("instance map")
        .get(council_id)
        .and_then(|instances| instances.by_member.get(member))
        .map(|instance| instance.tx.clone());
    tx.map(|tx| {
        tx.send(InstanceOutbound::Privmsg {
            target: target.to_string(),
            body: body.to_string(),
        })
        .is_ok()
    })
    .unwrap_or(false)
}

fn normalize_instance_mentions(bridge_state: &BridgeState, channel: &str, body: &str) -> String {
    let Some(council_id) = council_for_target(bridge_state, channel) else {
        return body.to_string();
    };
    let mappings = instance_mappings(bridge_state, &council_id);
    let mut out = body.to_string();

    for (member, nick) in &mappings {
        for marker in [":", ","] {
            let prefix = format!("{nick}{marker}");
            if out
                .get(..prefix.len())
                .map(|s| s.eq_ignore_ascii_case(&prefix))
                .unwrap_or(false)
            {
                let rest = out[prefix.len()..].trim_start();
                out = format!("@{member} {rest}");
                break;
            }
        }
        out = replace_ascii_case(&out, &format!("@{nick}"), &format!("@{member}"));
    }

    out
}

fn display_instance_mentions(bridge_state: &BridgeState, council_id: &str, body: &str) -> String {
    let mut out = body.to_string();
    for (member, nick) in instance_mappings(bridge_state, council_id) {
        out = replace_ascii_case(&out, &format!("@{member}"), &format!("@{nick}"));
    }
    out
}

fn instance_mappings(bridge_state: &BridgeState, council_id: &str) -> Vec<(String, String)> {
    bridge_state
        .instances
        .lock()
        .expect("instance map")
        .get(council_id)
        .map(|instances| {
            instances
                .by_member
                .iter()
                .map(|(member, instance)| (member.clone(), instance.nick.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn replace_ascii_case(input: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return input.to_string();
    }
    let lower_input = input.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let mut out = String::new();
    let mut cursor = 0;
    while let Some(pos) = lower_input[cursor..].find(&lower_needle) {
        let abs = cursor + pos;
        out.push_str(&input[cursor..abs]);
        out.push_str(replacement);
        cursor = abs + needle.len();
    }
    out.push_str(&input[cursor..]);
    out
}

fn normalize_channel(channel: &str) -> String {
    channel.to_ascii_lowercase()
}

fn irc_council_charter() -> &'static str {
    "\
You are ONE participant in a small IRC-backed team chat. Other agents and the operator may also participate.

Rules:
- If you are directly addressed with @yourname, you MUST respond.
- Otherwise responding is optional; reply only when you add useful signal.
- If you have nothing useful to add, emit a single line `pass` and stop.
- Reply in plain chat text only.
- Do not use Markdown.
- No headings, bullets, numbered lists, tables, fenced code blocks, blockquotes, Markdown links, bold, italic, or decorative formatting.
- Keep replies short enough for IRC: usually 1-4 lines.
- Address other participants by name with @name when your reply is specifically for them.
- Cite file paths, line numbers, and concrete claims when relevant. Brevity is welcome; pad is not."
}

fn daemon_path(daemon: &Url, path: &str) -> Url {
    let mut url = daemon.clone();
    let (path_part, query_part) = path.split_once('?').unwrap_or((path, ""));
    url.set_path(path_part);
    url.set_query((!query_part.is_empty()).then_some(query_part));
    url
}

fn tool_text_value(tool: ToolResult) -> Result<Value> {
    let text = tool
        .content
        .iter()
        .find_map(|c| c.get("text").and_then(Value::as_str))
        .unwrap_or_default();
    if tool.is_error.unwrap_or(false) {
        return Err(anyhow!(text.to_string()));
    }
    serde_json::from_str(text).or_else(|_| Ok(json!({ "text": text })))
}

fn build_irc_prompt(nick: &str, channel: &str, body: &str) -> String {
    format!(
        "[irc]\n\
your name: respond as the addressed bro\n\
operator nick: {nick}\n\
channel: {channel}\n\n\
You are replying into IRC. Output is shown directly in a chat client.\n\n\
Rules:\n\
- Answer only when you have useful signal for the operator.\n\
- Reply in plain chat text only.\n\
- Do not use Markdown.\n\
- No headings, bullets, numbered lists, tables, fenced code blocks, blockquotes, Markdown links, bold, italic, or decorative formatting.\n\
- Keep replies short enough for IRC: usually 1-4 lines.\n\
- If code or a patch is explicitly requested, provide only the minimal snippet needed.\n\
- If you have no useful answer, leave the response empty.\n\n\
[irc: operator turn]\n\
[{nick}] {body}\n"
    )
}

async fn summarize_tail_event(
    client: &reqwest::Client,
    daemon: &Url,
    v: &Value,
    labels: &mut HashMap<String, String>,
    last_outputs: &mut HashMap<String, String>,
) -> Option<String> {
    let kind = v.get("type")?.as_str()?;
    let task = v.get("task_id").and_then(Value::as_str).unwrap_or("");
    match kind {
        "task_started" => {
            let who = label_for_event(v);
            labels.insert(task.to_string(), who);
            None
        }
        "task_completed" => {
            let who = label_for_task(labels, v, task);
            let result = fetch_task_result(client, daemon, task).await;
            let last_output = last_outputs.remove(task);
            labels.remove(task);
            if is_council_label(&who) {
                return None;
            }
            match result.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(text) if last_output.as_deref().map(str::trim) == Some(text) => None,
                Some(text) => Some(format!("{who}: {text}")),
                None => None,
            }
        }
        "task_failed" => {
            let who = label_for_task(labels, v, task);
            last_outputs.remove(task);
            labels.remove(task);
            if is_council_label(&who) {
                return None;
            }
            Some(format!(
                "{who} failed {task}: {}",
                v.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            ))
        }
        "task_cancelled" => {
            let who = label_for_task(labels, v, task);
            last_outputs.remove(task);
            labels.remove(task);
            if is_council_label(&who) {
                return None;
            }
            Some(format!("{who} cancelled {task}"))
        }
        "task_progress" => {
            // Progress events are intentionally suppressed in the IRC bridge —
            // too chatty for a chat channel. Keep the activity extraction so
            // future filtering can reuse the parsed value.
            let _activity = v.get("activity").and_then(Value::as_str)?.trim();
            None
        }
        _ => None,
    }
}

async fn fetch_task_result(
    client: &reqwest::Client,
    daemon: &Url,
    task_id: &str,
) -> Option<String> {
    let result = get_tool(client, daemon, &format!("irc/status/{task_id}?tail=0"))
        .await
        .ok()?;
    result
        .get("result")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn label_for_event(v: &Value) -> String {
    v.get("bro_selector")
        .or_else(|| v.get("bro_name"))
        .or_else(|| v.get("provider"))
        .and_then(Value::as_str)
        .unwrap_or("bro")
        .to_string()
}

fn label_for_task(labels: &HashMap<String, String>, v: &Value, task_id: &str) -> String {
    let current = label_for_event(v);
    if current != "bro" {
        return current;
    }
    labels.get(task_id).cloned().unwrap_or(current)
}

fn is_council_label(label: &str) -> bool {
    label
        .strip_prefix("council-")
        .and_then(|rest| rest.split_once("::"))
        .is_some()
}

fn summarize_task_launch(label: &str, v: &Value) -> String {
    let task = v.get("taskId").and_then(Value::as_str).unwrap_or("?");
    let status = v.get("status").and_then(Value::as_str).unwrap_or("?");
    if status == "running" {
        return String::new();
    }
    format!("{label}: {status} {}", short_id(task))
}

fn summarize_team_launch(team: &str, v: &Value) -> String {
    let Some(tasks) = v.get("tasks").and_then(Value::as_array) else {
        return format!("{team}: launched");
    };
    let errors = tasks
        .iter()
        .filter_map(|x| {
            let bro = x.get("bro").and_then(Value::as_str).unwrap_or("?");
            x.get("error")
                .and_then(Value::as_str)
                .map(|err| format!("{bro}=error:{err}"))
        })
        .collect::<Vec<_>>();
    if errors.is_empty() {
        format!("{team}: launched {} tasks", tasks.len())
    } else {
        format!("{team}: launched with errors {}", errors.join(" "))
    }
}

fn summarize_status(v: &Value) -> String {
    let task = v.get("taskId").and_then(Value::as_str).unwrap_or("?");
    let status = v.get("status").and_then(Value::as_str).unwrap_or("?");
    let elapsed = v.get("elapsed").and_then(Value::as_str).unwrap_or("");
    let tail = v
        .get("events")
        .and_then(Value::as_array)
        .and_then(|xs| xs.last())
        .and_then(|x| x.get("message").or_else(|| x.get("text")))
        .and_then(Value::as_str)
        .map(|s| format!(" tail={}", one_line(s, 180)))
        .unwrap_or_default();
    format!("{task}: {status} {elapsed}{tail}")
}

fn summarize_dashboard(v: &Value) -> String {
    let Some(tasks) = v.get("tasks").and_then(Value::as_array) else {
        return "no tasks".to_string();
    };
    if tasks.is_empty() {
        return "no tasks".to_string();
    }
    tasks
        .iter()
        .map(|t| {
            let task = t.get("taskId").and_then(Value::as_str).unwrap_or("?");
            let status = t.get("status").and_then(Value::as_str).unwrap_or("?");
            let bro = t.get("bro").and_then(Value::as_str).unwrap_or("bro");
            format!("{bro}:{}={status}", short_id(task))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn summarize_council_rooms(councils: &[CouncilListEntry]) -> String {
    let rooms = councils
        .iter()
        .filter(|c| c.status == "open")
        .map(|c| {
            format!(
                "{} {}: {}",
                council_channel(&c.id),
                c.team_id,
                one_line(&c.topic, 80)
            )
        })
        .collect::<Vec<_>>();
    if rooms.is_empty() {
        "no open rooms".to_string()
    } else {
        rooms.join(" | ")
    }
}

fn summarize_council_room(councils: &[CouncilListEntry], council_id: &str) -> String {
    councils
        .iter()
        .find(|c| c.id == council_id)
        .map(|c| {
            format!(
                "{} {}: {} ({})",
                council_channel(&c.id),
                c.team_id,
                one_line(&c.topic, 160),
                c.status
            )
        })
        .unwrap_or_else(|| format!("{council_id}: unknown"))
}

fn help_text() -> String {
    "commands: !thread <team> <topic> | !rooms | !where | !close [council-id] | !attach <council-id> | !dashboard | !status <task> | !exec <bro> <prompt> | !provider <provider> <prompt> | !team <team> <prompt> | !resume <bro|provider:session> <prompt> | !cancel <task>".to_string()
}

fn parse_allow_nicks(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn allowed(allow_nicks: &[String], nick: &str) -> bool {
    allow_nicks.is_empty()
        || allow_nicks
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(nick))
}

fn require_arg<'a>(s: &'a str, usage: &str) -> Result<&'a str> {
    if s.trim().is_empty() {
        Err(anyhow!(usage.to_string()))
    } else {
        Ok(s.trim())
    }
}

fn split_two<'a>(s: &'a str, usage: &str) -> Result<(&'a str, &'a str)> {
    let mut parts = s.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or_default().trim();
    let second = parts.next().unwrap_or_default().trim();
    if first.is_empty() || second.is_empty() {
        Err(anyhow!(usage.to_string()))
    } else {
        Ok((first, second))
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut out = flat.chars().take(max.saturating_sub(1)).collect::<String>();
        out.push_str("...");
        out
    }
}

fn short_id(s: &str) -> String {
    s.chars().take(8).collect()
}

fn irc_lines(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in body.lines() {
        let mut line = raw.trim().to_string();
        while line.chars().count() > 380 {
            let chunk = line.chars().take(380).collect::<String>();
            out.push(chunk);
            line = line.chars().skip(380).collect();
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}
