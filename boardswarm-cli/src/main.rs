mod client;
mod ui;
mod ui_term;

use bytes::BytesMut;
use clap::{Args, Parser, Subcommand};
use futures::{pin_mut, FutureExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn copy_output_to_stdout(
    devices: &mut client::Devices,
    device: String,
    console: Option<String>,
) -> anyhow::Result<()> {
    let output = devices.stream_output(device, console).await?;
    pin_mut!(output);
    let mut stdout = tokio::io::stdout();
    while let Some(data) = output.next().await {
        stdout.write_all(&data).await?;
    }
    Ok(())
}

async fn copy_stdin_to_input(
    devices: &mut client::Devices,
    device: String,
    console: Option<String>,
) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let input = futures::stream::unfold(stdin, |mut stdin| async move {
        let mut data = BytesMut::zeroed(64);
        let r = stdin.read(&mut data).await.ok()?;
        data.truncate(r);
        Some((data.into(), stdin))
    });

    devices.stream_input(device, console, input).await?;
    Ok(())
}

#[derive(Debug, Args)]
struct DeviceConsoleArgs {
    #[clap(short, long)]
    console: Option<String>,
    device: String,
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    /// List devices known to the server
    List,
    /// Tail the output of a device console
    Tail(DeviceConsoleArgs),
    /// Connect input and output to a device console
    Connect(DeviceConsoleArgs),
}

#[derive(Debug, Subcommand)]
enum Command {
    Devices {
        #[command(subcommand)]
        device_command: DeviceCommand,
    },
    Ui(DeviceConsoleArgs),
}

#[derive(clap::Parser)]
struct Opts {
    #[clap(short, long, default_value = "http://localhost:50051")]
    uri: String,
    #[command(subcommand)]
    command: Command,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opts::parse();

    match opt.command {
        Command::Ui(ui) => ui::run_ui(opt.uri, ui.device, ui.console).await,
        Command::Devices { device_command } => {
            let mut devices = client::Devices::connect(opt.uri).await?;
            match device_command {
                DeviceCommand::List => {
                    println!("Devices:");
                    for d in devices.list_devices().await? {
                        println!("* {}", d);
                    }
                }
                DeviceCommand::Tail(d) => {
                    copy_output_to_stdout(&mut devices, d.device, d.console).await?;
                }
                DeviceCommand::Connect(d) => {
                    let mut devices_in = devices.clone();
                    let out =
                        copy_output_to_stdout(&mut devices, d.device.clone(), d.console.clone());
                    let in_ = copy_stdin_to_input(&mut devices_in, d.device, d.console);
                    futures::select! {
                        in_ = in_.fuse() => in_?,
                        out = out.fuse() => out?,
                    }
                }
            }
            Ok(())
        }
    }
}
