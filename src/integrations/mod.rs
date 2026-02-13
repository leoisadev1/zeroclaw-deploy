pub mod registry;

use crate::config::Config;
use anyhow::Result;

/// Integration status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationStatus {
    /// Fully implemented and ready to use
    Available,
    /// Configured and active
    Active,
    /// Planned but not yet implemented
    ComingSoon,
}

/// Integration category
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationCategory {
    Chat,
    AiModel,
    Productivity,
    MusicAudio,
    SmartHome,
    ToolsAutomation,
    MediaCreative,
    Social,
    Platform,
}

impl IntegrationCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat Providers",
            Self::AiModel => "AI Models",
            Self::Productivity => "Productivity",
            Self::MusicAudio => "Music & Audio",
            Self::SmartHome => "Smart Home",
            Self::ToolsAutomation => "Tools & Automation",
            Self::MediaCreative => "Media & Creative",
            Self::Social => "Social",
            Self::Platform => "Platforms",
        }
    }

    pub fn all() -> &'static [Self] {
        &[
            Self::Chat,
            Self::AiModel,
            Self::Productivity,
            Self::MusicAudio,
            Self::SmartHome,
            Self::ToolsAutomation,
            Self::MediaCreative,
            Self::Social,
            Self::Platform,
        ]
    }
}

/// A registered integration
pub struct IntegrationEntry {
    pub name: &'static str,
    pub description: &'static str,
    pub category: IntegrationCategory,
    pub status_fn: fn(&Config) -> IntegrationStatus,
}

/// Handle the `integrations` CLI command
pub fn handle_command(command: super::IntegrationCommands, config: &Config) -> Result<()> {
    match command {
        super::IntegrationCommands::List { category } => {
            list_integrations(config, category.as_deref())
        }
        super::IntegrationCommands::Info { name } => show_integration_info(config, &name),
    }
}

#[allow(clippy::unnecessary_wraps)]
fn list_integrations(config: &Config, filter_category: Option<&str>) -> Result<()> {
    let entries = registry::all_integrations();

    let mut available = 0u32;
    let mut active = 0u32;
    let mut coming = 0u32;

    for &cat in IntegrationCategory::all() {
        // Filter by category if specified
        if let Some(filter) = filter_category {
            let filter_lower = filter.to_lowercase();
            let cat_lower = cat.label().to_lowercase();
            if !cat_lower.contains(&filter_lower) {
                continue;
            }
        }

        let cat_entries: Vec<&IntegrationEntry> =
            entries.iter().filter(|e| e.category == cat).collect();

        if cat_entries.is_empty() {
            continue;
        }

        println!("\n  ⟩ {}", console::style(cat.label()).white().bold());

        for entry in &cat_entries {
            let status = (entry.status_fn)(config);
            let (icon, label) = match status {
                IntegrationStatus::Active => {
                    active += 1;
                    ("✅", console::style("active").green())
                }
                IntegrationStatus::Available => {
                    available += 1;
                    ("⚪", console::style("available").dim())
                }
                IntegrationStatus::ComingSoon => {
                    coming += 1;
                    ("🔜", console::style("coming soon").dim())
                }
            };
            println!(
                "    {icon} {:<22} {:<30} {}",
                console::style(entry.name).white().bold(),
                entry.description,
                label
            );
        }
    }

    let total = available + active + coming;
    println!();
    println!("  {total} integrations: {active} active, {available} available, {coming} coming soon");
    println!();
    println!("  Configure: zeroclaw onboard");
    println!("  Details:   zeroclaw integrations info <name>");
    println!();

    Ok(())
}

fn show_integration_info(config: &Config, name: &str) -> Result<()> {
    let entries = registry::all_integrations();
    let name_lower = name.to_lowercase();

    let Some(entry) = entries.iter().find(|e| e.name.to_lowercase() == name_lower) else {
        anyhow::bail!(
            "Unknown integration: {name}. Run `zeroclaw integrations list` to see all."
        );
    };

    let status = (entry.status_fn)(config);
    let (icon, label) = match status {
        IntegrationStatus::Active => ("✅", "Active"),
        IntegrationStatus::Available => ("⚪", "Available"),
        IntegrationStatus::ComingSoon => ("🔜", "Coming Soon"),
    };

    println!();
    println!("  {} {} — {}", icon, console::style(entry.name).white().bold(), entry.description);
    println!("  Category: {}", entry.category.label());
    println!("  Status:   {label}");
    println!();

    // Show setup hints based on integration
    match entry.name {
        "Telegram" => {
            println!("  Setup:");
            println!("    1. Message @BotFather on Telegram");
            println!("    2. Create a bot and copy the token");
            println!("    3. Run: zeroclaw onboard");
            println!("    4. Start: zeroclaw channel start");
        }
        "Discord" => {
            println!("  Setup:");
            println!("    1. Go to https://discord.com/developers/applications");
            println!("    2. Create app → Bot → Copy token");
            println!("    3. Enable MESSAGE CONTENT intent");
            println!("    4. Run: zeroclaw onboard");
        }
        "Slack" => {
            println!("  Setup:");
            println!("    1. Go to https://api.slack.com/apps");
            println!("    2. Create app → Bot Token Scopes → Install");
            println!("    3. Run: zeroclaw onboard");
        }
        "OpenRouter" => {
            println!("  Setup:");
            println!("    1. Get API key at https://openrouter.ai/keys");
            println!("    2. Run: zeroclaw onboard");
            println!("    Access 200+ models with one key.");
        }
        "Ollama" => {
            println!("  Setup:");
            println!("    1. Install: brew install ollama");
            println!("    2. Pull a model: ollama pull llama3");
            println!("    3. Set provider to 'ollama' in config.toml");
        }
        "iMessage" => {
            println!("  Setup (macOS only):");
            println!("    Uses AppleScript bridge to send/receive iMessages.");
            println!("    Requires Full Disk Access in System Settings → Privacy.");
        }
        "GitHub" => {
            println!("  Setup:");
            println!("    1. Create a personal access token at https://github.com/settings/tokens");
            println!("    2. Add to config: [integrations.github] token = \"ghp_...\"");
        }
        "Browser" => {
            println!("  Built-in:");
            println!("    ZeroClaw can control Chrome/Chromium for web tasks.");
            println!("    Uses headless browser automation.");
        }
        "Cron" => {
            println!("  Built-in:");
            println!("    Schedule tasks in ~/.zeroclaw/workspace/cron/");
            println!("    Run: zeroclaw cron list");
        }
        "Webhooks" => {
            println!("  Built-in:");
            println!("    HTTP endpoint for external triggers.");
            println!("    Run: zeroclaw gateway");
        }
        _ => {
            if status == IntegrationStatus::ComingSoon {
                println!("  This integration is planned. Stay tuned!");
                println!("  Track progress: https://github.com/theonlyhennygod/zeroclaw");
            }
        }
    }

    println!();
    Ok(())
}
