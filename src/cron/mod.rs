use crate::config::Config;
use anyhow::Result;

pub fn handle_command(command: super::CronCommands, _config: Config) -> Result<()> {
    match command {
        super::CronCommands::List => {
            println!("No scheduled tasks yet.");
            println!("\nUsage:");
            println!("  zeroclaw cron add '0 9 * * *' 'agent -m \"Good morning!\"'");
            Ok(())
        }
        super::CronCommands::Add {
            expression,
            command,
        } => {
            println!("Cron scheduling coming soon!");
            println!("  Expression: {expression}");
            println!("  Command: {command}");
            Ok(())
        }
        super::CronCommands::Remove { id } => {
            anyhow::bail!("Remove task '{id}' not yet implemented");
        }
    }
}
