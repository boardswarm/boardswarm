use std::{
    cmp::Ordering,
    convert::Infallible,
    io::SeekFrom,
    os::unix::prelude::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use async_compression::futures::bufread::GzipDecoder;
use bmap_parser::Bmap;
use boardswarm_cli::{
    client::{Boardswarm, VolumeIoRW},
    device::DeviceVolume,
};
use boardswarm_protocol::ItemType;
use bytes::{Bytes, BytesMut};
use clap::{arg, Args, Parser, Subcommand};
use futures::{pin_mut, FutureExt, Stream, StreamExt, TryStreamExt};
use rockfile::boot::{
    RkBootEntry, RkBootEntryBytes, RkBootHeader, RkBootHeaderBytes, RkBootHeaderEntry,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader},
};

use boardswarm_cli::client::ItemEvent;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

fn find_bmap(img: &Path) -> Option<PathBuf> {
    fn append(path: PathBuf) -> PathBuf {
        let mut p = path.into_os_string();
        p.push(".bmap");
        p.into()
    }

    let mut bmap = img.to_path_buf();
    loop {
        bmap = append(bmap);
        if bmap.exists() {
            return Some(bmap);
        }
        // Drop .bmap
        bmap.set_extension("");
        bmap.extension()?;
        // Drop existing orignal extension part
        bmap.set_extension("");
    }
}

async fn write_bmap(io: VolumeIoRW, path: &Path) -> anyhow::Result<()> {
    let bmap_path = find_bmap(path).ok_or_else(|| anyhow!("Failed to find bmap"))?;
    println!("Using bmap file: {}", path.display());

    let mut bmap_file = tokio::fs::File::open(bmap_path).await?;
    let mut xml = String::new();
    bmap_file.read_to_string(&mut xml).await?;
    let bmap = Bmap::from_xml(&xml)?;

    let blocksize = io.blocksize().unwrap_or(4096) as usize * 128;
    let mut writer = boardswarm_cli::utils::BatchWriter::new(io, blocksize).compat_write();

    let file = tokio::fs::File::open(path).await?;
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("gz") => {
            let gz = GzipDecoder::new(BufReader::new(file).compat());
            let mut gz = bmap_parser::AsyncDiscarder::new(gz);
            bmap_parser::copy_async(&mut gz, &mut writer, &bmap).await?;
        }
        _ => {
            bmap_parser::copy_async(&mut file.compat(), &mut writer, &bmap).await?;
        }
    }

    Ok(())
}

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
    let stdin_fd = stdin.as_raw_fd();

    let mut stdin_termios = nix::sys::termios::tcgetattr(stdin_fd).unwrap();

    nix::sys::termios::cfmakeraw(&mut stdin_termios);
    nix::sys::termios::tcsetattr(stdin_fd, nix::sys::termios::SetArg::TCSANOW, &stdin_termios)
        .unwrap();

    futures::stream::unfold(stdin, |mut stdin| async move {
        let mut data = BytesMut::zeroed(64);
        let r = stdin.read(&mut data).await.ok()?;
        data.truncate(r);
        Some((data.into(), stdin))
    })
}

async fn rock_download_entry(
    header: RkBootHeaderEntry,
    target: &str,
    file: &mut File,
    volume: &mut DeviceVolume,
) -> anyhow::Result<()> {
    for i in 0..header.count {
        let mut entry: RkBootEntryBytes = [0; 57];

        file.seek(SeekFrom::Start(
            header.offset as u64 + (header.size * i) as u64,
        ))
        .await?;
        file.read_exact(&mut entry).await?;

        let entry = RkBootEntry::from_bytes(&entry);
        println!("{} Name: {}", i, String::from_utf16(entry.name.as_slice())?);

        let mut data = Vec::new();
        data.resize(entry.data_size as usize, 0);

        file.seek(SeekFrom::Start(entry.data_offset as u64)).await?;
        file.read_exact(&mut data).await?;

        let mut target = volume.open(target, Some(data.len() as u64)).await?;
        target.write_all(&data).await?;
        target.flush().await?;

        println!("Done!... waiting {}ms", entry.data_delay);
        if entry.data_delay > 0 {
            tokio::time::sleep(Duration::from_millis(entry.data_delay as u64)).await;
        }
    }

    Ok(())
}

async fn rock_download_boot(volume: &mut DeviceVolume, path: &Path) -> anyhow::Result<()> {
    let mut file = File::open(path).await?;
    let mut header: RkBootHeaderBytes = [0; 102];
    file.read_exact(&mut header).await?;
    let header =
        RkBootHeader::from_bytes(&header).ok_or_else(|| anyhow!("Failed to parse header"))?;

    rock_download_entry(header.entry_471, "471", &mut file, volume).await?;
    rock_download_entry(header.entry_472, "472", &mut file, volume).await?;
    Ok(())
}

#[derive(Debug, Args)]
struct ActuatorMode {
    mode: String,
}

#[derive(Debug, Subcommand)]
enum ActuatorCommand {
    /// Change actuator mode
    ChangeMode(ActuatorMode),
}

#[derive(Debug, Args)]
struct ConsoleConfigure {
    configuration: String,
}

#[derive(Debug, Subcommand)]
enum ConsoleCommand {
    /// Configure a console
    Configure(ConsoleConfigure),
    /// Tail the output of a device console
    Tail,
    /// Connect input and output to a device console
    Connect,
}

#[derive(Debug, Args)]
struct WriteArgs {
    #[clap(short, long)]
    offset: Option<u64>,
    target: String,
    file: PathBuf,
}

#[derive(Debug, Args)]
struct BmapWriteArgs {
    target: String,
    file: PathBuf,
}

#[derive(Debug, Subcommand)]
enum VolumeCommand {
    Info,
    /// Upload file to volume target
    Write(WriteArgs),
    /// Write a bmap file to volume target
    WriteBmap(BmapWriteArgs),
    /// Commit upload
    Commit,
}

#[derive(Clone, Debug)]
enum DeviceArg {
    Id(u64),
    Name(String),
}

impl DeviceArg {
    async fn device(
        &self,
        client: Boardswarm,
    ) -> Result<Option<boardswarm_cli::device::Device>, anyhow::Error> {
        let builder = boardswarm_cli::device::DeviceBuilder::from_client(client);
        match self {
            DeviceArg::Id(id) => Ok(Some(builder.by_id(*id).await?)),
            DeviceArg::Name(name) => Ok(builder.by_name(name).await?),
        }
    }
}

fn parse_device(device: &str) -> Result<DeviceArg, Infallible> {
    if let Ok(id) = device.parse() {
        Ok(DeviceArg::Id(id))
    } else {
        Ok(DeviceArg::Name(device.to_string()))
    }
}

#[derive(Clone, Debug, Args)]
struct DeviceConsoleArgs {
    #[clap(short, long)]
    console: Option<String>,
}

#[derive(Debug, Args)]
struct DeviceModeArgs {
    mode: String,
}

#[derive(Debug, Args)]
struct DeviceWriteArg {
    #[arg(short, long)]
    offset: Option<u64>,
    #[arg(short, long)]
    wait: bool,
    #[arg(short, long)]
    commit: bool,
    volume: String,
    target: String,
    file: PathBuf,
}

#[derive(Debug, Args)]
struct DeviceBmapWriteArg {
    #[arg(short, long)]
    wait: bool,
    #[arg(short, long)]
    commit: bool,
    volume: String,
    target: String,
    file: PathBuf,
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    /// Get info about a device
    Info {
        #[arg(short, long)]
        follow: bool,
    },
    Write(DeviceWriteArg),
    WriteBmap(DeviceBmapWriteArg),
    /// Change device mode
    Mode(DeviceModeArgs),
    // Turn the device off and on again
    Reset,
    /// Connect to the console
    Connect(DeviceConsoleArgs),
    /// Tail to the console
    Tail(DeviceConsoleArgs),
}

#[derive(Debug, Subcommand)]
enum RockCommand {
    DownloadBoot {
        #[arg(short, long)]
        wait: bool,
        #[clap(short, long)]
        volume: Option<String>,
        path: PathBuf,
    },
}

fn parse_item(item: &str) -> Result<ItemType, anyhow::Error> {
    let types = [
        ("actuators", ItemType::Actuator),
        ("consoles", ItemType::Console),
        ("devices", ItemType::Device),
        ("volumes", ItemType::Volume),
    ];
    for (n, t) in types {
        if n == item {
            return Ok(t);
        }
    }

    Err(anyhow::anyhow!(
        "Unknown item type; known types: {:?}",
        types.map(|(n, _)| n)
    ))
}

#[derive(Debug, Subcommand)]
enum Command {
    Login {},
    Actuator {
        actuator: u64,
        #[command(subcommand)]
        command: ActuatorCommand,
    },
    Console {
        console: u64,
        #[command(subcommand)]
        command: ConsoleCommand,
    },
    Volume {
        volume: u64,
        #[command(subcommand)]
        command: VolumeCommand,
    },
    Device {
        #[arg(value_parser = parse_device)]
        device: DeviceArg,
        #[command(subcommand)]
        command: DeviceCommand,
    },
    // Commands specific to rockchip devices
    Rock {
        #[arg(value_parser = parse_device)]
        device: DeviceArg,
        #[command(subcommand)]
        command: RockCommand,
    },
    List {
        #[arg(value_parser = parse_item)]
        type_: ItemType,
    },
    Monitor {
        #[arg(value_parser = parse_item)]
        type_: ItemType,
    },
    Properties {
        #[arg(value_parser = parse_item)]
        type_: ItemType,
        item: u64,
    },
    Ui {
        #[arg(value_parser = parse_device)]
        device: DeviceArg,
        #[command(flatten)]
        console: DeviceConsoleArgs,
    },
}

#[derive(clap::Parser)]
struct Opts {
    #[clap(short, long)]
    token: Option<PathBuf>,
    #[clap(short, long, default_value = "http://localhost:6653")]
    uri: tonic::transport::Uri,
    #[command(subcommand)]
    command: Command,
}

fn print_item(i: boardswarm_protocol::Item) {
    print!("{} {}", i.id, i.name);
    if let Some(instance) = i.instance {
        println!(" on {instance}");
    } else {
        println!();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opt = Opts::parse();

    println!("Connecting to: {}", opt.uri);
    let mut build = boardswarm_cli::client::BoardswarmBuilder::new(opt.uri);
    if let Some(token) = opt.token {
        let token = tokio::fs::read_to_string(token)
            .await
            .context("Failed to read token")?;
        build.auth_static(token.trim_end())
    }
    let mut boardswarm = build.connect().await?;

    match opt.command {
        Command::Login {} => {
            println!("Info: {:#?}", boardswarm.login_info().await?);
            Ok(())
        }
        Command::List { type_ } => {
            println!("{:?}s: ", type_);
            for i in boardswarm.list(type_).await? {
                print_item(i);
            }
            Ok(())
        }
        Command::Monitor { type_ } => {
            println!("{:?}s: ", type_);
            let events = boardswarm.monitor(type_).await?;
            pin_mut!(events);
            while let Some(event) = events.next().await {
                let event = event?;
                match event {
                    ItemEvent::Added(items) => {
                        for i in items {
                            print_item(i)
                        }
                    }
                    ItemEvent::Removed(removed) => println!("Removed: {}", removed),
                }
            }
            Ok(())
        }
        Command::Properties { type_, item } => {
            let properties = boardswarm.properties(type_, item).await?;
            for (k, v) in properties {
                println!(r#""{}" => "{}""#, k, v);
            }
            Ok(())
        }
        Command::Actuator { actuator, command } => {
            match command {
                ActuatorCommand::ChangeMode(c) => {
                    let p = serde_json::from_str(&c.mode)?;
                    boardswarm.actuator_change_mode(actuator, p).await?;
                }
            }

            Ok(())
        }
        Command::Console { console, command } => {
            match command {
                ConsoleCommand::Configure(c) => {
                    let p = serde_json::from_str(&c.configuration)?;
                    boardswarm.console_configure(console, p).await?;
                }
                ConsoleCommand::Tail => {
                    let output = boardswarm.console_stream_output(console).await?;
                    copy_output_to_stdout(output).await?;
                }
                ConsoleCommand::Connect => {
                    let out =
                        copy_output_to_stdout(boardswarm.console_stream_output(console).await?);
                    let in_ = boardswarm.console_stream_input(console, input_stream());
                    futures::select! {
                        in_ = in_.fuse() => in_?,
                        out = out.fuse() => out?,
                    }
                }
            }

            Ok(())
        }
        Command::Volume { volume, command } => {
            match command {
                VolumeCommand::Info => {
                    let info = boardswarm.volume_info(volume).await?;
                    println!("{:#?}", info);
                }
                VolumeCommand::Write(write) => {
                    let mut f = tokio::fs::File::open(write.file).await?;
                    let m = f.metadata().await?;
                    let mut rw = boardswarm
                        .volume_io_readwrite(volume, write.target, Some(m.len()))
                        .await?;
                    if let Some(offset) = write.offset {
                        rw.seek(SeekFrom::Start(offset)).await?;
                    }
                    tokio::io::copy(&mut f, &mut rw).await?;
                    f.flush().await?;
                    drop(rw);
                }
                VolumeCommand::WriteBmap(write) => {
                    let rw = boardswarm
                        .volume_io_readwrite(volume, write.target, None)
                        .await?;
                    write_bmap(rw, &write.file).await?;
                }
                VolumeCommand::Commit => {
                    boardswarm.volume_commit(volume).await?;
                }
            }
            Ok(())
        }
        Command::Device { device, command } => {
            let device = device.device(boardswarm.clone()).await?;
            let device = device.ok_or_else(|| anyhow::anyhow!("Device not found"))?;
            match command {
                DeviceCommand::Write(DeviceWriteArg {
                    offset,
                    wait,
                    commit,
                    volume,
                    target,
                    file,
                }) => {
                    let mut f = tokio::fs::File::open(file).await?;
                    let mut volume = device
                        .volume_by_name(&volume)
                        .ok_or_else(|| anyhow!("Volume not available for device"))?;
                    if !volume.available() {
                        if wait {
                            println!("Waiting for volume..");
                            volume.wait().await;
                        } else {
                            bail!("volume not available");
                        }
                    }

                    let m = f.metadata().await?;
                    let mut rw = volume.open(target, Some(m.len())).await?;
                    if let Some(offset) = offset {
                        rw.seek(SeekFrom::Start(offset)).await?;
                    }
                    tokio::io::copy(&mut f, &mut rw).await?;
                    f.flush().await?;
                    drop(rw);

                    if commit {
                        volume.commit().await?;
                    }
                }
                DeviceCommand::WriteBmap(DeviceBmapWriteArg {
                    wait,
                    commit,
                    volume,
                    target,
                    file,
                }) => {
                    let mut volume = device
                        .volume_by_name(&volume)
                        .ok_or_else(|| anyhow!("Volume not available for device"))?;
                    if !volume.available() {
                        if wait {
                            println!("Waiting for volume..");
                            volume.wait().await;
                        } else {
                            bail!("volume not available");
                        }
                    }

                    let rw = volume.open(target, None).await?;
                    write_bmap(rw, &file).await?;

                    if commit {
                        volume.commit().await?;
                    }
                }
                DeviceCommand::Info { follow } => {
                    let mut d = boardswarm.device_info(device.id()).await?;
                    while let Some(device) = d.try_next().await? {
                        println!("{:#?}", device);
                        if !follow {
                            break;
                        }
                    }
                }
                DeviceCommand::Mode(d) => {
                    device.change_mode(d.mode).await?;
                }
                DeviceCommand::Reset {} => {
                    println!("Turning off");
                    device.change_mode("off").await?;
                    println!("Turning on");
                    device.change_mode("on").await?;
                }
                DeviceCommand::Connect(d) => {
                    let mut console = if let Some(c) = &d.console {
                        device
                            .console_by_name(c)
                            .ok_or_else(|| anyhow::anyhow!("Console not found"))?
                    } else {
                        device
                            .console()
                            .ok_or_else(|| anyhow::anyhow!("Console not found"))?
                    };
                    let out = copy_output_to_stdout(console.stream_output().await?);
                    let in_ = console.stream_input(input_stream());
                    futures::select! {
                        in_ = in_.fuse() => in_?,
                        out = out.fuse() => out?,
                    }
                }
                DeviceCommand::Tail(d) => {
                    let mut console = if let Some(c) = &d.console {
                        device
                            .console_by_name(c)
                            .ok_or_else(|| anyhow::anyhow!("Console not found"))?
                    } else {
                        device
                            .console()
                            .ok_or_else(|| anyhow::anyhow!("Console not found"))?
                    };
                    let output = console.stream_output().await?;
                    copy_output_to_stdout(output).await?;
                }
            }
            Ok(())
        }
        Command::Rock { device, command } => {
            let device = device
                .device(boardswarm)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Device not found"))?;
            match command {
                RockCommand::DownloadBoot { wait, volume, path } => {
                    let mut volume = if let Some(volume) = volume {
                        device
                            .volume_by_name(&volume)
                            .ok_or_else(|| anyhow::anyhow!("Volume not found"))?
                    } else {
                        let mut volumes = device.volumes();
                        match volumes.len().cmp(&1) {
                            Ordering::Equal => volumes.pop().unwrap(),
                            Ordering::Greater => bail!("More then one volume, please specify one"),
                            Ordering::Less => bail!("No volumes for this device"),
                        }
                    };
                    if !volume.available() {
                        if wait {
                            println!("Waiting for volume..");
                            volume.wait().await;
                        } else {
                            bail!("volume not available");
                        }
                    }
                    rock_download_boot(&mut volume, &path).await?;
                }
            }
            Ok(())
        }
        Command::Ui { device, console } => {
            let device = device
                .device(boardswarm)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Device not found"))?;
            boardswarm_cli::ui::run_ui(device, console.console).await
        }
    }
}
