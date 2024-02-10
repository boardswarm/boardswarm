use std::{
    cmp::Ordering,
    convert::Infallible,
    io::SeekFrom,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{anyhow, bail, Context};
use async_compression::futures::bufread::GzipDecoder;
use bmap_parser::Bmap;
use boardswarm_client::{
    client::{Boardswarm, BoardswarmBuilder, VolumeIoRW},
    config,
    device::DeviceVolume,
    oidc::{OidcClientBuilder, StdoutAuth},
};
use boardswarm_protocol::ItemType;
use bytes::{Bytes, BytesMut};
use clap::{arg, Args, Parser, Subcommand};
use futures::{pin_mut, FutureExt, Stream, StreamExt, TryStreamExt};
use http::Uri;
use rockfile::boot::{
    RkBootEntry, RkBootEntryBytes, RkBootHeader, RkBootHeaderBytes, RkBootHeaderEntry,
};
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader},
};

use boardswarm_client::client::ItemEvent;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

mod ui;
mod ui_term;
mod utils;

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
    let mut writer = utils::BatchWriter::new(io, blocksize).compat_write();

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

    let mut stdin_termios = nix::sys::termios::tcgetattr(&stdin).unwrap();

    nix::sys::termios::cfmakeraw(&mut stdin_termios);
    nix::sys::termios::tcsetattr(&stdin, nix::sys::termios::SetArg::TCSANOW, &stdin_termios)
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

        let mut data = vec![0; entry.data_size as usize];
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
    /// Actuator specific mode in json format
    mode: String,
}

#[derive(Debug, Subcommand)]
enum ActuatorCommand {
    /// Change actuator mode
    ChangeMode(ActuatorMode),
}

#[derive(Debug, Args)]
struct ConsoleConfigure {
    /// Console specific configure in json format
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
    /// Offset in bytes to write to
    #[clap(short, long)]
    offset: Option<u64>,
    /// Target to write to
    target: String,
    /// File to write
    file: PathBuf,
}

#[derive(Debug, Args)]
struct BmapWriteArgs {
    /// Target to write the bmap file to
    target: String,
    /// Path to bmap file
    file: PathBuf,
}

#[derive(Debug, Subcommand)]
enum VolumeCommand {
    /// Retrieve volume information
    Info,
    /// Upload file to the volume
    Write(WriteArgs),
    /// Write a bmap file to the volum
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
    ) -> Result<Option<boardswarm_client::device::Device>, anyhow::Error> {
        let builder = boardswarm_client::device::DeviceBuilder::from_client(client);
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
    /// Console to open instead of the default
    #[clap(short, long)]
    console: Option<String>,
}

#[derive(Debug, Args)]
struct DeviceModeArgs {
    /// Mode to change the device to
    mode: String,
}

#[derive(Debug, Args)]
struct DeviceReadArg {
    /// Offset in bytes for reading to start
    #[arg(short, long)]
    offset: Option<u64>,
    /// Amount of bytes to be read
    #[arg(short, long)]
    #[arg(short, long)]
    length: Option<u64>,
    /// Wait for the volume and target to appear
    #[arg(short, long)]
    wait: bool,
    /// The volume to read from
    volume: String,
    /// The volume target to read from
    target: String,
    /// Path to the file to write the read data to
    file: PathBuf,
}

#[derive(Debug, Args)]
struct DeviceWriteArg {
    /// Write at the given offset rather then from the start
    #[arg(short, long)]
    offset: Option<u64>,
    /// Wait for the volume and target to appear
    #[arg(short, long)]
    wait: bool,
    /// Commit the volume after finishing the write
    #[arg(short, long)]
    commit: bool,
    /// The volume to write to
    volume: String,
    /// The volume target to write to
    target: String,
    /// Path to the file to write
    file: PathBuf,
}

#[derive(Debug, Args)]
struct DeviceBmapWriteArg {
    /// Wait for the volume and target to appear
    #[arg(short, long)]
    wait: bool,
    /// Commit the volume after finishing the write
    #[arg(short, long)]
    commit: bool,
    /// The volume to write to
    volume: String,
    /// The volume target to write to
    target: String,
    /// Path to the bmap file to write
    file: PathBuf,
}

#[derive(Debug, Subcommand)]
enum DeviceCommand {
    /// Get info about a device
    Info {
        /// Monitor changes to the device information
        #[arg(short, long)]
        follow: bool,
    },
    /// Read data from a device volume
    Read(DeviceReadArg),
    /// Write data to a device volume
    Write(DeviceWriteArg),
    /// Write a bmap file to a device volume
    WriteBmap(DeviceBmapWriteArg),
    /// Change device mode
    Mode(DeviceModeArgs),
    /// Turn the device off and on again
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

#[derive(Debug, clap::Parser)]
struct ConfigureArg {
    /// Instance to configure is new
    #[clap(long)]
    new: bool,
    /// Instance should be set as the default going forward
    #[clap(long)]
    default: bool,
    /// Configure the instance's url
    #[clap(short, long)]
    uri: Option<Uri>,
    /// Configure the JWT auth token
    #[clap(short, long)]
    token: Option<String>,
    /// Read new JWT auth token from the given file
    #[clap(long)]
    token_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Configure boardswarm instances
    Configure(ConfigureArg),
    /// Retrieve the login information from the remote boardswarm server
    LoginInfo,
    /// Actuator specific commands
    Actuator {
        actuator: u64,
        #[command(subcommand)]
        command: ActuatorCommand,
    },
    /// Console specific commands
    Console {
        console: u64,
        #[command(subcommand)]
        command: ConsoleCommand,
    },
    /// Volumes specific commands
    Volume {
        volume: u64,
        #[command(subcommand)]
        command: VolumeCommand,
    },
    /// Device specific commands
    Device {
        #[arg(value_parser = parse_device)]
        device: DeviceArg,
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// Commands specific to rockchip devices
    Rock {
        #[arg(value_parser = parse_device)]
        device: DeviceArg,
        #[command(subcommand)]
        command: RockCommand,
    },
    /// List all items of a given type
    List {
        #[arg(value_parser = parse_item)]
        type_: ItemType,
    },
    /// Monitor registered items of a given type
    Monitor {
        #[arg(value_parser = parse_item)]
        type_: ItemType,
    },
    /// Show item properties
    Properties {
        #[arg(value_parser = parse_item)]
        type_: ItemType,
        item: u64,
    },
    /// Open the UI for a given device
    Ui {
        #[arg(value_parser = parse_device)]
        device: DeviceArg,
        #[command(flatten)]
        console: DeviceConsoleArgs,
    },
}

async fn run_configure(
    mut config: config::Config,
    instance: Option<String>,
    opts: &ConfigureArg,
) -> anyhow::Result<()> {
    if opts.new && instance.is_none() {
        bail!("New instances require an instance argument");
    }

    // UGGGG
    let config_path = config.path().to_owned();

    let current_server = match instance {
        Some(ref i) => config.find_server_mut(i),
        None => config.default_server_mut(),
    };

    match (current_server.is_some(), opts.new) {
        (true, true) => bail!("Instance already exists"),
        (false, false) => bail!("Instance not found, use --new to create a new one"),
        _ => (),
    }

    let uri = match &current_server {
        Some(s) => opts.uri.as_ref().unwrap_or(&s.uri),
        None => opts
            .uri
            .as_ref()
            .ok_or_else(|| anyhow!("New server needs a url"))?,
    };

    let instance = instance.unwrap_or_else(|| current_server.as_ref().unwrap().name.clone());

    let auth = if let Some(token) = &opts.token {
        Some(config::Auth::Token(token.clone()))
    } else if let Some(ref token_path) = opts.token_file {
        let file = tokio::fs::File::open(token_path).await?;
        let mut reader = BufReader::new(file);
        let mut token = String::new();
        let read = reader.read_line(&mut token).await?;
        // Arbitrary minimal size to make sure might actually be a token without going all the way
        // and parsing it. Mostly to avoid accidentally using an empty file
        if read < 16 {
            bail!("Token file doesn't look like a JWT token (too small)");
        }
        let token = token.trim_end();
        Some(config::Auth::Token(token.to_string()))
    } else if current_server
        .as_ref()
        .map_or(false, |s| matches!(s.auth, config::Auth::Token(_)))
    {
        None
    } else {
        // OIDC
        let mut boardswarm = BoardswarmBuilder::new(uri.clone()).connect().await?;
        let info = boardswarm.login_info().await?;

        // TODO allow user select the !first one
        let login = info.first().unwrap();
        println!("Starting login with {}", login.description);
        match &login.method {
            boardswarm_client::client::AuthMethod::Oidc { url, client_id } => {
                let mut token_file = instance.replace([std::path::MAIN_SEPARATOR, '.'], "_");
                token_file.push_str(".token");
                let token_cache = config_path.with_file_name(token_file);

                let mut builder = OidcClientBuilder::new(url.parse()?, client_id);
                builder.token_cache(token_cache.clone());
                builder.login_provider(StdoutAuth());

                let mut oidc = builder.build();
                oidc.auth().await?;
                Some(config::Auth::Oidc {
                    uri: url.parse().unwrap(),
                    client_id: client_id.clone(),
                    token_cache,
                })
            }
        }
    };

    if let Some(s) = current_server {
        if let Some(auth) = auth {
            s.auth = auth;
        }
        if let Some(uri) = &opts.uri {
            s.uri = uri.clone()
        }
        if opts.default {
            let name = s.name.clone();
            config.set_default(&name);
        }
    } else {
        let new = config::Server {
            name: instance.clone(),
            uri: opts.uri.clone().ok_or_else(|| anyhow!("Missing uri"))?,
            auth: auth.ok_or_else(|| anyhow!("Missing authentication configuration"))?,
        };
        config.add_server(new);
        if opts.default {
            config.set_default(&instance);
        }
    }

    config.write().await?;

    Ok(())
}

#[derive(clap::Parser)]
struct Opts {
    #[clap(short, long)]
    config: Option<PathBuf>,
    /// instance name
    #[clap(short, long)]
    instance: Option<String>,
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
    tracing_subscriber::fmt::init();

    let opt = Opts::parse();

    let config_path = opt.config.clone().unwrap_or_else(|| {
        let mut c = dirs::config_dir().expect("Config directory not found");
        c.push("boardswarm");
        c.push("config.yaml");
        c
    });

    let config = match boardswarm_client::config::Config::from_file(&config_path).await {
        Ok(config) => Some(config),
        Err(config::Error::IO(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("Failed to load config"),
    };

    let server = if let Some(ref c) = config {
        if let Some(ref name) = opt.instance {
            c.find_server(name)
        } else {
            c.default_server()
        }
    } else {
        None
    };

    // Pre-connection handling
    let mut boardswarm = match opt.command {
        Command::Configure(ref configure) => {
            let config = config
                .clone()
                .unwrap_or_else(|| config::Config::new(config_path));

            return run_configure(config, opt.instance, configure).await;
        }
        Command::LoginInfo { .. } => {
            if let Some(server) = server {
                server.to_boardswarm_builder().connect().await?
            } else if let Some(ref instance) = opt.instance {
                let Ok(uri) = instance.parse() else {
                    return Err(anyhow!("{instance} not a uri and not in configuration"));
                };
                BoardswarmBuilder::new(uri).connect().await?
            } else {
                return Err(anyhow!(
                    "default Server should be configured or instance passed"
                ));
            }
        }
        _ => {
            if let Some(server) = server {
                let mut builder: BoardswarmBuilder = server.to_boardswarm_builder();
                builder.login_provider(StdoutAuth());
                builder.connect().await?
            } else if config.is_none() {
                return Err(anyhow!(
                    "Configuration file not found; Run configure first?"
                ));
            } else if let Some(ref instance) = opt.instance {
                return Err(anyhow!("{instance} not found in configuration"));
            } else {
                return Err(anyhow!("No default instance configured"));
            }
        }
    };

    match opt.command {
        Command::Configure { .. } => {
            unreachable!()
        }
        Command::LoginInfo { .. } => {
            println!("Info: {:#?}", boardswarm.login_info().await?);
            Ok(())
        }
        Command::List { type_ } => {
            let items = boardswarm.list(type_).await?;
            println!("{:?}s: ", type_);
            for i in items {
                print_item(i);
            }
            Ok(())
        }
        Command::Monitor { type_ } => {
            let events = boardswarm.monitor(type_).await?;
            println!("{:?}s: ", type_);
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
                DeviceCommand::Read(DeviceReadArg {
                    offset,
                    length,
                    wait,
                    volume,
                    target,
                    file,
                }) => {
                    let mut f = tokio::fs::File::create(file).await?;
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
                    let mut r = volume.open(target, None).await?;
                    if let Some(offset) = offset {
                        r.seek(SeekFrom::Start(offset)).await?;
                    }

                    // For most volumes reading 8K at a time will be rather slow, so use a 1
                    // megabyte reader
                    let mut r = BufReader::with_capacity(1024 * 1024, r);
                    if let Some(length) = length {
                        let mut r = r.take(length);
                        tokio::io::copy_buf(&mut r, &mut f).await?;
                    } else {
                        tokio::io::copy_buf(&mut r, &mut f).await?;
                    }
                    f.flush().await?;
                }
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
            ui::run_ui(device, console.console).await
        }
    }
}
