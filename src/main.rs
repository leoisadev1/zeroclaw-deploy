#![warn(clippy::all, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::unnecessary_literal_bound,
    clippy::module_name_repetitions,
    clippy::struct_field_names,
    dead_code
)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod agent;
mod channels;
mod config;
mod cron;
mod gateway;
mod heartbeat;
mod memory;
mod observability;
mod onboard;
mod providers;
mod runtime;
mod security;
mod integrations;
mod skills;
mod tools;

use config::Config;

/// `ZeroClaw` - Zero overhead. Zero compromise. 100% Rust.
#[derive(Parser, Debug)]
#[command(name = "zeroclaw")]
#[command(author = "theonlyhennygod")]
#[command(version = "0.1.0")]
#[command(about = "The fastest, smallest AI assistant.", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize your workspace and configuration
    Onboard,

    /// Start the AI agent loop
    Agent {
        /// Single message mode (don't enter interactive mode)
        #[arg(short, long)]
        message: Option<String>,

        /// Provider to use (openrouter, anthropic, openai)
        #[arg(short, long)]
        provider: Option<String>,

        /// Model to use
        #[arg(short, long)]
        model: Option<String>,

        /// Temperature (0.0 - 2.0)
        #[arg(short, long, default_value = "0.7")]
        temperature: f64,
    },

    /// Start the gateway server (webhooks, websockets)
    Gateway {
        /// Port to listen on
        #[arg(short, long, default_value = "8080")]
        port: u16,

        /// Host to bind to
        #[arg(short, long, default_value = "127.0.0.1")]
        host: String,
    },

    /// Show system status
    Status {
        /// Show detailed status
        #[arg(short, long)]
        verbose: bool,
    },

    /// Configure and manage scheduled tasks
    Cron {
        #[command(subcommand)]
        cron_command: CronCommands,
    },

    /// Manage channels (telegram, discord, slack)
    Channel {
        #[command(subcommand)]
        channel_command: ChannelCommands,
    },

    /// Tool utilities
    Tools {
        #[command(subcommand)]
        tool_command: ToolCommands,
    },

    /// Browse 50+ integrations
    Integrations {
        #[command(subcommand)]
        integration_command: IntegrationCommands,
    },

    /// Manage skills (user-defined capabilities)
    Skills {
        #[command(subcommand)]
        skill_command: SkillCommands,
    },
}

#[derive(Subcommand, Debug)]
enum CronCommands {
    /// List all scheduled tasks
    List,
    /// Add a new scheduled task
    Add {
        /// Cron expression
        expression: String,
        /// Command to run
        command: String,
    },
    /// Remove a scheduled task
    Remove {
        /// Task ID
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ChannelCommands {
    /// List configured channels
    List,
    /// Start all configured channels (Telegram, Discord, Slack)
    Start,
    /// Add a new channel
    Add {
        /// Channel type
        channel_type: String,
        /// Configuration JSON
        config: String,
    },
    /// Remove a channel
    Remove {
        /// Channel name
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum SkillCommands {
    /// List installed skills
    List,
    /// Install a skill from a GitHub URL or local path
    Install {
        /// GitHub URL or local path
        source: String,
    },
    /// Remove an installed skill
    Remove {
        /// Skill name
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum IntegrationCommands {
    /// List all integrations and their status
    List {
        /// Filter by category (e.g. "chat", "ai", "productivity")
        #[arg(short, long)]
        category: Option<String>,
    },
    /// Show details about a specific integration
    Info {
        /// Integration name
        name: String,
    },
}

#[derive(Subcommand, Debug)]
enum ToolCommands {
    /// List available tools
    List,
    /// Test a tool
    Test {
        /// Tool name
        tool: String,
        /// Tool arguments (JSON)
        args: String,
    },
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    // Onboard runs the interactive wizard — no existing config needed
    if matches!(cli.command, Commands::Onboard) {
        let config = onboard::run_wizard()?;
        // Auto-start channels if user said yes during wizard
        if std::env::var("ZEROCLAW_AUTOSTART_CHANNELS").as_deref() == Ok("1") {
            channels::start_channels(config).await?;
        }
        return Ok(());
    }

    // All other commands need config loaded first
    let config = Config::load_or_init()?;

    match cli.command {
        Commands::Onboard => unreachable!(),

        Commands::Agent {
            message,
            provider,
            model,
            temperature,
        } => agent::run(config, message, provider, model, temperature).await,

        Commands::Gateway { port, host } => {
            info!("🚀 Starting ZeroClaw Gateway on {host}:{port}");
            info!("POST http://{host}:{port}/webhook  — send JSON messages");
            info!("GET  http://{host}:{port}/health    — health check");
            gateway::run_gateway(&host, port, config).await
        }

        Commands::Status { verbose } => {
            println!("🦀 ZeroClaw Status");
            println!();
            println!("Version:     {}", env!("CARGO_PKG_VERSION"));
            println!("Workspace:   {}", config.workspace_dir.display());
            println!("Config:      {}", config.config_path.display());
            println!();
            println!(
                "🤖 Provider:      {}",
                config.default_provider.as_deref().unwrap_or("openrouter")
            );
            println!(
                "   Model:         {}",
                config.default_model.as_deref().unwrap_or("(default)")
            );
            println!("📊 Observability:  {}", config.observability.backend);
            println!("🛡️  Autonomy:      {:?}", config.autonomy.level);
            println!("⚙️  Runtime:       {}", config.runtime.kind);
            println!(
                "💓 Heartbeat:      {}",
                if config.heartbeat.enabled {
                    format!("every {}min", config.heartbeat.interval_minutes)
                } else {
                    "disabled".into()
                }
            );
            println!(
                "🧠 Memory:         {} (auto-save: {})",
                config.memory.backend,
                if config.memory.auto_save { "on" } else { "off" }
            );

            if verbose {
                println!();
                println!("Security:");
                println!("  Workspace only:    {}", config.autonomy.workspace_only);
                println!(
                    "  Allowed commands:  {}",
                    config.autonomy.allowed_commands.join(", ")
                );
                println!(
                    "  Max actions/hour:  {}",
                    config.autonomy.max_actions_per_hour
                );
                println!(
                    "  Max cost/day:      ${:.2}",
                    f64::from(config.autonomy.max_cost_per_day_cents) / 100.0
                );
                println!();
                println!("Channels:");
                println!("  CLI:      ✅ always");
                for (name, configured) in [
                    ("Telegram", config.channels_config.telegram.is_some()),
                    ("Discord", config.channels_config.discord.is_some()),
                    ("Slack", config.channels_config.slack.is_some()),
                    ("Webhook", config.channels_config.webhook.is_some()),
                ] {
                    println!(
                        "  {name:9} {}",
                        if configured { "✅ configured" } else { "❌ not configured" }
                    );
                }
            }

            Ok(())
        }

        Commands::Cron { cron_command } => cron::handle_command(cron_command, config),

        Commands::Channel { channel_command } => match channel_command {
            ChannelCommands::Start => channels::start_channels(config).await,
            other => channels::handle_command(other, &config),
        },

        Commands::Tools { tool_command } => tools::handle_command(tool_command, config).await,

        Commands::Integrations {
            integration_command,
        } => integrations::handle_command(integration_command, &config),

        Commands::Skills { skill_command } => {
            skills::handle_command(skill_command, &config.workspace_dir)
        }
    }
}
