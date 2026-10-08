//! Command line front end.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tokio::sync::watch;

use ftpseg::cli::{Cli, Command, ConnectionArgs, GetArgs, LsArgs, StatArgs, TargetSpec};
use ftpseg::client::FtpClient;
use ftpseg::config::DownloadOptions;
use ftpseg::download;
use ftpseg::error::{Error, Result};
use ftpseg::fsutil::{human_bytes, human_rate};
use ftpseg::paths;

fn main() -> ExitCode {
    let cli = Cli::parse();

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Some(threads) = cli.connection.threads {
        builder.worker_threads(threads.max(1));
    }
    let runtime = match builder.build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Aborted) => {
            eprintln!(
                "interrupted — the partial download was kept; run the same command to resume"
            );
            ExitCode::from(130)
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    match &cli.command {
        Command::Get(args) => run_get(&cli.connection, args).await,
        Command::Ls(args) => run_ls(&cli.connection, args).await,
        Command::Stat(args) => run_stat(&cli.connection, args).await,
        Command::Gui(args) => {
            ftpseg::gui::serve(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), args.port)).await
        }
    }
}

fn local_destination(spec: &TargetSpec, output: &Option<PathBuf>) -> PathBuf {
    match output {
        Some(path) => {
            // `-o dir/` means "into this directory, keep the remote name".
            if path.is_dir() {
                path.join(spec.default_local_name())
            } else {
                path.clone()
            }
        }
        None => PathBuf::from(spec.default_local_name()),
    }
}

async fn run_get(connection: &ConnectionArgs, args: &GetArgs) -> Result<()> {
    let spec = TargetSpec::resolve(&args.target, connection)?;
    let connect = spec.connect_options(connection)?;
    let local = local_destination(&spec, &args.output);

    let mut options = DownloadOptions::new(spec.remote.clone(), local.clone());
    options.policy.connections = args.connections.max(1);
    options.policy.segment_size = args.segment_size;
    if let Some(min_segment) = args.min_segment {
        options.policy.min_segment = min_segment.max(1);
    }
    options.limit = args.limit;
    options.retries = args.retries;
    options.resume = !args.no_resume;
    options.force = args.force;
    options.keep_state = args.keep_state;
    options.quiet = args.quiet;
    options.dry_run = args.dry_run;

    if args.dry_run {
        return dry_run(&connect, &options).await;
    }

    // Ctrl-C stops the transfer cleanly: workers stop between chunks and the
    // resume file is written before we return.
    let (cancel_tx, cancel_rx) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancel_tx.send(true);
        }
    });

    let summary = download::download_file(Arc::new(connect), options, cancel_rx).await?;

    if summary.already_complete {
        println!(
            "{} is already complete ({}); use --force to download it again",
            local.display(),
            human_bytes(summary.bytes)
        );
        return Ok(());
    }

    println!(
        "saved {} — {} in {:.1}s ({})",
        local.display(),
        human_bytes(summary.bytes),
        summary.elapsed.as_secs_f64(),
        human_rate(summary.bytes_per_second())
    );
    if summary.resumed_from > 0 {
        println!("resumed from {}", human_bytes(summary.resumed_from));
    }
    println!(
        "{} segment(s) over {} connection(s)",
        summary.segments, summary.connections_used
    );
    Ok(())
}

async fn dry_run(
    connect: &ftpseg::client::ConnectOptions,
    options: &DownloadOptions,
) -> Result<()> {
    let info = download::probe(connect, &options.remote).await?;

    println!("remote      : {}", options.remote);
    println!("destination : {}", options.local.display());
    println!("charset     : {}", info.charset.describe());
    match info.size {
        Some(size) => println!("size        : {} ({size} bytes)", human_bytes(size)),
        None => println!("size        : unknown (server did not report one)"),
    }
    if let Some(mdtm) = &info.mdtm {
        println!("modified    : {mdtm}");
    }
    println!(
        "REST support: {}",
        if info.rest_supported {
            "yes"
        } else {
            "no — a single connection will be used"
        }
    );

    match (info.size, info.rest_supported) {
        (Some(size), true) if size > 0 => {
            println!(
                "plan        : {}",
                download::describe_plan(size, &options.policy)
            );
        }
        _ => println!("plan        : single connection (cannot be segmented)"),
    }
    Ok(())
}

async fn run_ls(connection: &ConnectionArgs, args: &LsArgs) -> Result<()> {
    let spec = TargetSpec::resolve(&args.target, connection)?;
    let connect = spec.connect_options(connection)?;
    let mut client = FtpClient::connect(&connect).await?;

    // Listing a file path is a common mistake; fall back to listing its parent
    // and keeping just that entry.
    let entries = match client.list(Some(&spec.remote)).await {
        Ok(entries) => entries,
        Err(first_error) => {
            let (parent, name) = paths::split_remote(&spec.remote);
            if name.is_empty() {
                return Err(first_error);
            }
            let entries = client.list(parent.as_deref()).await?;
            let filtered: Vec<_> = entries
                .into_iter()
                .filter(|entry| entry.name == name)
                .collect();
            if filtered.is_empty() {
                return Err(first_error);
            }
            filtered
        }
    };

    let mut entries = entries;
    entries.sort_by_key(|a| a.name.to_lowercase());

    if args.long {
        println!("{:<4} {:>12}  {:<16} name", "type", "size", "modified");
        for entry in &entries {
            println!(
                "{:<4} {:>12}  {:<16} {}",
                if entry.is_dir { "dir" } else { "file" },
                entry
                    .size
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                entry.modify.as_deref().unwrap_or("-"),
                entry.name
            );
        }
        println!("\n{} entries", entries.len());
    } else {
        for entry in &entries {
            if entry.is_dir {
                println!("{}/", entry.name);
            } else {
                println!("{}", entry.name);
            }
        }
    }

    client.quit().await;
    Ok(())
}

async fn run_stat(connection: &ConnectionArgs, args: &StatArgs) -> Result<()> {
    let spec = TargetSpec::resolve(&args.target, connection)?;
    let connect = spec.connect_options(connection)?;
    let info = download::probe(&connect, &spec.remote).await?;

    println!("host        : {}:{}", connect.host, connect.port);
    println!(
        "transport   : {}",
        match connect.tls {
            ftpseg::client::TlsMode::None => "plain FTP",
            ftpseg::client::TlsMode::Explicit => "explicit FTPS (AUTH TLS)",
            ftpseg::client::TlsMode::Implicit => "implicit FTPS",
        }
    );
    println!("user        : {}", connect.user);
    println!("path        : {}", spec.remote);
    println!("charset     : {}", info.charset.describe());
    match info.size {
        Some(size) => println!("size        : {} ({size} bytes)", human_bytes(size)),
        None => println!("size        : unknown"),
    }
    println!(
        "modified    : {}",
        info.mdtm.as_deref().unwrap_or("unknown")
    );
    println!(
        "kind        : {}",
        if info.is_dir { "directory" } else { "file" }
    );
    println!("REST STREAM : {}", yes_no(info.rest_supported));
    println!("SIZE        : {}", yes_no(info.features.size));
    println!("MDTM        : {}", yes_no(info.features.mdtm));
    println!("MLSD        : {}", yes_no(info.features.mlsd));
    println!("UTF8        : {}", yes_no(info.features.utf8));
    println!("EPSV        : {}", yes_no(info.features.epsv));
    println!("AUTH TLS    : {}", yes_no(info.features.auth_tls));

    if !info.features.raw.is_empty() {
        println!("features    :");
        for feature in &info.features.raw {
            println!("  {feature}");
        }
    }

    if let (Some(size), true) = (info.size, info.rest_supported) {
        let policy = ftpseg::config::SegmentPolicy::default();
        println!("default plan: {}", download::describe_plan(size, &policy));
    }

    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}
