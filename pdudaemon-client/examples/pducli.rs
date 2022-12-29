use clap::{Args, Parser, Subcommand};

#[derive(Clone, Debug, Args)]
struct CommandArgs {
    hostname: String,
    port: u16,
}

#[derive(Debug, Subcommand)]
enum Command {
    On(CommandArgs),
    Off(CommandArgs),
    Reboot {
        #[command(flatten)]
        a: CommandArgs,
        #[arg(short, long)]
        delay: Option<u32>,
    },
}

#[derive(clap::Parser)]
struct Opts {
    #[arg(short, long, default_value = "http://localhost:16421")]
    url: String,
    #[command(subcommand)]
    command: Command,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opts::parse();

    let pdu = pdudaemon_client::PduDaemon::new(&opt.url)?;
    match opt.command {
        Command::On(a) => pdu.on(&a.hostname, a.port).await?,
        Command::Off(a) => pdu.off(&a.hostname, a.port).await?,
        Command::Reboot { a, delay } => pdu.reboot(&a.hostname, a.port, delay).await?,
    }

    Ok(())
}
