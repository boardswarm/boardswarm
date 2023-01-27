mod client;
mod ui;
mod ui_term;

use std::path::PathBuf;

use bytes::{Bytes, BytesMut};
use clap::{Args, Parser, Subcommand};
use futures::{pin_mut, stream, FutureExt, Stream, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn copy_output_to_stdout<O>(output: O) -> anyhow::Result<()>
where
    O: Stream<Item = Bytes>,
{
    pin_mut!(output);
    let mut stdout = tokio::io::stdout();
    while let Some(data) = output.next().await {
        stdout.write_all(&data).await?;
        stdout.flush().await?;
    }
    Ok(())
}

fn input_stream() -> impl Stream<Item = Bytes> {
    let stdin = tokio::io::stdin();
    futures::stream::unfold(stdin, |mut stdin| async move {
        let mut data = BytesMut::zeroed(64);
        let r = stdin.read(&mut data).await.ok()?;
        data.truncate(r);
        Some((data.into(), stdin))
    })
}

#[derive(Debug, Args)]
struct ActuatorMode {
    actuator: String,
    mode: String,
}

#[derive(Debug, Subcommand)]
enum ActuatorCommand {
    /// List actuators known to the server
    List,
    /// Change actuator mode
    ChangeMode(ActuatorMode),
}

#[derive(Debug, Args)]
struct ConsoleArgs {
    console: String,
}

#[derive(Debug, Args)]
struct ConsoleConfigure {
    console: String,
    configuration: String,
}

#[derive(Debug, Subcommand)]
enum ConsoleCommand {
    /// List devices known to the server
    List,
    /// Configure a console
    Configure(ConsoleConfigure),
    /// Tail the output of a device console
    Tail(ConsoleArgs),
    /// Connect input and output to a device console
    Connect(ConsoleArgs),
}

#[derive(Debug, Args)]
struct UploadArgs {
    uploader: String,
    target: String,
    file: PathBuf,
}

#[derive(Debug, Subcommand)]
enum UploadCommand {
    /// List uploaders known to the server
    List,
    /// Upload file to uploader target
    Upload(UploadArgs),
    /// Commit upload
    Commit { uploader: String },
}

#[derive(Debug, Args)]
struct DeviceConsoleArgs {
    #[clap(short, long)]
    console: Option<String>,
    device: String,
}

#[derive(Debug, Args)]
struct DeviceModeArgs {
    device: String,
    mode: String,
}

#[derive(Debug, Args)]
struct DeviceUploadArgs {
    device: String,
    uploader: String,
    target: String,
    file: PathBuf,
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    /// List devices known to the server
    List,
    /// Tail the output of a device console
    Tail(DeviceConsoleArgs),
    /// Connect input and output to a device console
    Connect(DeviceConsoleArgs),
    /// Change device mode
    Mode(DeviceModeArgs),
    /// Upload file to uploader target
    Upload(DeviceUploadArgs),
    /// Commit upload
    Commit { device: String, uploader: String },
}

#[derive(Debug, Subcommand)]
enum Command {
    Actuators {
        #[command(subcommand)]
        command: ActuatorCommand,
    },
    Consoles {
        #[command(subcommand)]
        command: ConsoleCommand,
    },
    Uploaders {
        #[command(subcommand)]
        command: UploadCommand,
    },
    Devices {
        #[command(subcommand)]
        command: DeviceCommand,
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
        Command::Actuators { command } => {
            let mut actuators = client::Actuators::connect(opt.uri).await?;
            match command {
                ActuatorCommand::List => {
                    println!("Actuators:");
                    for c in actuators.list().await? {
                        println!("* {}", c);
                    }
                }
                ActuatorCommand::ChangeMode(c) => {
                    let p = serde_json::from_str(&c.mode)?;
                    actuators.change_mode(c.actuator, p).await?;
                }
            }

            Ok(())
        }
        Command::Consoles { command } => {
            let mut consoles = client::Consoles::connect(opt.uri).await?;
            match command {
                ConsoleCommand::List => {
                    println!("Consoles:");
                    for c in consoles.list().await? {
                        println!("* {}", c);
                    }
                }
                ConsoleCommand::Configure(c) => {
                    let p = serde_json::from_str(&c.configuration)?;
                    consoles.configure(c.console, p).await?;
                }
                ConsoleCommand::Tail(c) => {
                    let output = consoles.stream_output(c.console).await?;
                    copy_output_to_stdout(output).await?;
                }
                ConsoleCommand::Connect(c) => {
                    let out =
                        copy_output_to_stdout(consoles.stream_output(c.console.clone()).await?);
                    let in_ = consoles.stream_input(c.console, input_stream());
                    futures::select! {
                        in_ = in_.fuse() => in_?,
                        out = out.fuse() => out?,
                    }
                }
            }

            Ok(())
        }
        Command::Uploaders { command } => {
            let mut uploaders = client::Uploaders::connect(opt.uri).await?;
            match command {
                UploadCommand::List => {
                    println!("Uploaders:");
                    for u in uploaders.list().await? {
                        println!(" {}", u);
                    }
                }
                UploadCommand::Upload(upload) => {
                    let f = tokio::fs::File::open(upload.file).await?;
                    let m = f.metadata().await?;
                    let data = stream::unfold(f, |mut f| async move {
                        let mut data = BytesMut::zeroed(4096);
                        let r = f.read(&mut data).await.unwrap();
                        if r == 0 {
                            None
                        } else {
                            data.truncate(r);
                            Some((data.freeze(), f))
                        }
                    });

                    uploaders
                        .upload(upload.uploader, upload.target, data, m.len())
                        .await?;
                }
                UploadCommand::Commit { uploader } => {
                    uploaders.commit(uploader).await?;
                }
            }
            Ok(())
        }
        Command::Devices { command } => {
            let mut devices = client::Devices::connect(opt.uri).await?;
            match command {
                DeviceCommand::List => {
                    println!("Devices:");
                    for d in devices.list().await? {
                        println!("* {}", d);
                    }
                }
                DeviceCommand::Tail(d) => {
                    let output = devices.stream_output(d.device, d.console).await?;
                    copy_output_to_stdout(output).await?;
                }
                DeviceCommand::Connect(d) => {
                    let out = copy_output_to_stdout(
                        devices
                            .stream_output(d.device.clone(), d.console.clone())
                            .await?,
                    );
                    let in_ = devices.stream_input(d.device, d.console, input_stream());

                    futures::select! {
                        in_ = in_.fuse() => in_?,
                        out = out.fuse() => out?,
                    }
                }
                DeviceCommand::Mode(d) => devices.change_mode(d.device, d.mode).await?,
                DeviceCommand::Upload(upload) => {
                    let f = tokio::fs::File::open(upload.file).await?;
                    let m = f.metadata().await?;
                    let data = stream::unfold(f, |mut f| async move {
                        let mut data = BytesMut::zeroed(4096);
                        let r = f.read(&mut data).await.unwrap();
                        if r == 0 {
                            None
                        } else {
                            data.truncate(r);
                            Some((data.freeze(), f))
                        }
                    });

                    devices
                        .upload(upload.device, upload.uploader, upload.target, data, m.len())
                        .await?;
                }
                DeviceCommand::Commit { device, uploader } => {
                    devices.commit(device, uploader).await?;
                }
            }
            Ok(())
        }
        Command::Ui(ui) => ui::run_ui(opt.uri, ui.device, ui.console).await,
    }
}
