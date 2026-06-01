use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "codex-threads",
    version,
    about = "Query and control Codex app-server threads"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[arg(long, global = true)]
    pub connect: Option<String>,
    #[arg(
        long = "no-yolo",
        global = true,
        help = "Use Codex app-server approval and sandbox defaults instead of forcing approvalPolicy=never and full-access sandboxing"
    )]
    pub no_yolo: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Servers(ServersCommand),
    List(ListCommand),
    Search(SearchCommand),
    Show(ShowCommand),
    #[command(
        about = "Show flattened messages from a bounded recent turn scan",
        after_help = "Message selection order: fetch recent turns with --max-turns, flatten user/assistant messages, apply --since, apply --role, then apply --last.\n\n--max-turns is the recent turn scan window, not the final message display limit. Use --last for the final number of messages to print. Role filters only see messages inside the scanned turns, so widen --max-turns when searching for sparse or older roles.\n\nThere is no messages --first. For beginning-of-thread or older exact paging, use show --asc and/or show --cursor with the needed --items view."
    )]
    Messages(MessagesCommand),
    New(NewCommand),
    Send(SendCommand),
    Settings(SettingsCommand),
    Status(StatusCommand),
    Steer(SteerCommand),
    Interrupt(InterruptCommand),
    Name(NameCommand),
    Archive(ThreadOnlyCommand),
    Unarchive(ThreadOnlyCommand),
    Models(ModelsCommand),
    Goal(GoalCommand),
}

#[derive(Debug, Args)]
pub struct ServerOpt {
    #[arg(long)]
    pub server: Option<String>,
}

#[derive(Debug, Args)]
pub struct JsonOpt {
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ServersCommand {
    #[command(subcommand)]
    pub command: Option<ServersSubcommand>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ServersSubcommand {
    Ping(ServersPingCommand),
}

#[derive(Debug, Args)]
pub struct ServersPingCommand {
    #[arg(long)]
    pub server: Option<String>,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ListCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    #[arg(long)]
    pub limit: Option<u32>,
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long)]
    pub archived: bool,
    #[arg(long, value_enum)]
    pub sort: Option<SortKey>,
    #[arg(long, conflicts_with = "desc")]
    pub asc: bool,
    #[arg(long)]
    pub desc: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SearchCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub query: String,
    #[arg(long)]
    pub limit: Option<u32>,
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub archived: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ShowCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    #[arg(long)]
    pub last: Option<u32>,
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long, conflicts_with = "desc")]
    pub asc: bool,
    #[arg(long)]
    pub desc: bool,
    #[arg(long, value_enum, default_value_t = ItemsView::Summary)]
    pub items: ItemsView,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct MessagesCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    #[arg(
        long,
        help = "Return the last N messages after flattening and filtering"
    )]
    pub last: Option<usize>,
    #[arg(
        long,
        help = "Keep messages whose turn timestamp is within this epoch-second or relative window, such as 5m or 1d"
    )]
    pub since: Option<String>,
    #[arg(
        long,
        value_enum,
        help = "Keep only messages from this role after the recent turn scan"
    )]
    pub role: Option<MessageRole>,
    #[arg(
        long,
        default_value_t = 200,
        help = "Number of recent turns to scan before flattening and filtering messages"
    )]
    pub max_turns: u32,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct NewCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    #[arg(long)]
    pub cwd: PathBuf,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    #[arg(long = "service-tier")]
    pub service_tier: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    pub prompt: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub stream: bool,
    #[arg(long = "no-wait")]
    pub no_wait: bool,
}

#[derive(Debug, Args)]
pub struct SendCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    #[arg(long = "service-tier")]
    pub service_tier: Option<String>,
    pub prompt: String,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub stream: bool,
    #[arg(long = "no-wait")]
    pub no_wait: bool,
}

#[derive(Debug, Subcommand)]
pub enum SettingsSubcommand {
    Show(SettingsShowCommand),
    Set(SettingsSetCommand),
}

#[derive(Debug, Args)]
pub struct SettingsCommand {
    #[command(subcommand)]
    pub command: SettingsSubcommand,
}

#[derive(Debug, Args)]
pub struct SettingsShowCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SettingsSetCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub effort: Option<String>,
    #[arg(long = "service-tier", conflicts_with = "clear_service_tier")]
    pub service_tier: Option<String>,
    #[arg(long = "clear-service-tier")]
    pub clear_service_tier: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StatusCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SteerCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    pub turn_id: String,
    pub prompt: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct InterruptCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    pub turn_id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct NameCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    pub name: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ThreadOnlyCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ModelsCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct GoalCommand {
    #[command(subcommand)]
    pub command: GoalSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum GoalSubcommand {
    Get(GoalGetCommand),
    Set(GoalSetCommand),
    Clear(GoalClearCommand),
}

#[derive(Debug, Args)]
pub struct GoalGetCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct GoalSetCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    #[arg(long)]
    pub objective: Option<String>,
    #[arg(long = "token-budget")]
    pub token_budget: Option<i64>,
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct GoalClearCommand {
    #[command(flatten)]
    pub server: ServerOpt,
    pub thread_id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SortKey {
    Updated,
    Created,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ItemsView {
    Summary,
    Full,
    None,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MessageRole {
    User,
    Assistant,
}
