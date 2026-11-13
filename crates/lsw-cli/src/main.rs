//! `lsw`: command-line front end for the LSW host service.
//!
//! M0 commands:
//!
//! * `lsw doctor` – host capability report and blocking problems.
//! * `lsw create` – download and verify media, write the answer file and the
//!   VM configuration.
//! * `lsw install` – boot the installer, follow progress, power off when done.
//! * `lsw run` – boot the installed VM in the foreground.
//! * `lsw ctl` – talk to a running VM (status, pause, resume, screenshot…).

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use lsw_daemon::config::{Firmware, GpuTier, InstallMedia, Network, Profile, VmConfig};
use lsw_daemon::firmware;
use lsw_daemon::host::HostReport;
use lsw_daemon::paths::VmPaths;
use lsw_daemon::qemu::{DiskIo, LaunchOptions};
use lsw_daemon::qmp::Qmp;
use lsw_daemon::vm::{self, Vm};
use lsw_installer::{download, media, unattend};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "lsw", version, about = "Linux Subsystem for Windows")]
struct Cli {
    /// VM name.
    #[arg(long, global = true, default_value = "windows")]
    name: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Source {
    /// Windows 11 multi-edition ISO from Microsoft (installs Pro).
    Consumer,
    /// Windows 11 Enterprise Evaluation (90 days, for testing).
    Evaluation,
    /// An ISO you already have (`--iso`).
    Iso,
}

#[derive(Clone, Copy, ValueEnum)]
enum TierArg {
    Auto,
    A,
    B,
    C,
    D,
}

#[derive(Clone, Copy, ValueEnum)]
enum NetArg {
    Auto,
    Passt,
    User,
    None,
}

#[derive(Clone, Copy, ValueEnum)]
enum ProfileArg {
    Productivity,
    Development,
    Games,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check whether this host can run LSW.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Prepare a new Windows VM (downloads media, writes the answer file).
    Create {
        #[arg(long, value_enum, default_value_t = ProfileArg::Productivity)]
        profile: ProfileArg,
        #[arg(long, value_enum, default_value_t = Source::Consumer)]
        source: Source,
        /// Path to a Windows 11 ISO (with `--source iso`).
        #[arg(long)]
        iso: Option<PathBuf>,
        /// Image name in the ISO for `--source iso` (default "Windows 11 Pro").
        #[arg(long, default_value = "Windows 11 Pro")]
        edition: String,
        /// Download language as named by Microsoft (`English`, `English International`, …).
        #[arg(long, default_value = "English")]
        language: String,
        /// Windows account name (default: your Linux user name).
        #[arg(long)]
        user: Option<String>,
        #[arg(long, value_enum, default_value_t = TierArg::Auto)]
        tier: TierArg,
        #[arg(long, value_enum, default_value_t = NetArg::Auto)]
        network: NetArg,
        /// Override memory in MiB.
        #[arg(long)]
        memory: Option<u32>,
        /// Override vCPU count.
        #[arg(long)]
        cpus: Option<u16>,
        /// Override disk size in GiB.
        #[arg(long)]
        disk: Option<u64>,
    },
    /// Run the unattended installation and wait for it to finish.
    Install {
        /// Give up after this many minutes.
        #[arg(long, default_value_t = 240)]
        timeout_min: u64,
        /// Save a screenshot of the finished desktop here.
        #[arg(long)]
        screenshot: Option<PathBuf>,
        #[command(flatten)]
        launch: LaunchArgs,
    },
    /// Boot the installed VM in the foreground (Ctrl+C shuts it down).
    Run {
        #[command(flatten)]
        launch: LaunchArgs,
    },
    /// Control a running VM.
    Ctl {
        #[command(subcommand)]
        action: CtlAction,
    },
}

#[derive(clap::Args, Clone)]
struct LaunchArgs {
    /// Use the portable thread-pool disk backend instead of io_uring.
    #[arg(long)]
    disk_threads: bool,
    /// Disable QEMU's seccomp sandbox (debugging only).
    #[arg(long)]
    no_sandbox: bool,
    /// Expose VNC on this Unix socket (debugging).
    #[arg(long)]
    vnc: Option<PathBuf>,
}

#[derive(Subcommand)]
enum CtlAction {
    Status,
    Pause,
    Resume,
    /// Save a PNG screenshot.
    Screenshot {
        path: PathBuf,
    },
    /// ACPI power button.
    Powerdown,
    /// Stop QEMU immediately.
    Quit,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("LSW_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let paths = VmPaths::resolve(&cli.name)?;
    match cli.cmd {
        Cmd::Doctor { json } => doctor(json),
        Cmd::Create {
            profile,
            source,
            iso,
            edition,
            language,
            user,
            tier,
            network,
            memory,
            cpus,
            disk,
        } => {
            create(
                &cli.name,
                &paths,
                CreateArgs {
                    profile,
                    source,
                    iso,
                    edition,
                    language,
                    user,
                    tier,
                    network,
                    memory,
                    cpus,
                    disk,
                },
            )
            .await
        }
        Cmd::Install {
            timeout_min,
            screenshot,
            launch,
        } => install(&paths, timeout_min, screenshot, &launch).await,
        Cmd::Run { launch } => run(&paths, &launch).await,
        Cmd::Ctl { action } => ctl(&paths, action).await,
    }
}

fn doctor(json: bool) -> anyhow::Result<()> {
    let report = HostReport::probe();
    let problems = report.problems();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({ "host": report, "problems": problems })
            )?
        );
    } else {
        println!(
            "CPU:          {:?}, virtualization {}",
            report.cpu_vendor,
            yes(report.virt_extensions)
        );
        println!("Logical CPUs: {}", report.logical_cpus);
        println!("Memory:       {} MiB", report.mem_total_mib);
        println!(
            "KVM:          present {}, accessible {}",
            yes(report.kvm_present),
            yes(report.kvm_accessible)
        );
        println!("IOMMU groups: {}", report.iommu_groups);
        println!("vhost-vsock:  {}", yes(report.vhost_vsock));
        for g in &report.gpus {
            println!(
                "GPU:          {} {:04x}:{:04x} driver={} boot_vga={} vfs={} render={}",
                g.pci_addr,
                g.vendor,
                g.device,
                g.driver.as_deref().unwrap_or("-"),
                g.boot_vga,
                g.sriov_total_vfs,
                g.render_node.as_deref().unwrap_or("-")
            );
        }
        println!("GPU tiers:    {:?}", report.tiers);
        let b = &report.binaries;
        for (name, p) in [
            ("qemu", &b.qemu),
            ("swtpm", &b.swtpm),
            ("passt", &b.passt),
            ("virtiofsd", &b.virtiofsd),
        ] {
            println!(
                "{name:<13} {}",
                p.as_ref()
                    .map_or("not found".into(), |p| p.display().to_string())
            );
        }
        if problems.is_empty() {
            println!("\nThis host can run LSW.");
        } else {
            println!("\nProblems:");
            for p in &problems {
                println!("  - {}\n    fix: {}", p.what, p.fix);
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        std::process::exit(2)
    }
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

struct CreateArgs {
    profile: ProfileArg,
    source: Source,
    iso: Option<PathBuf>,
    edition: String,
    language: String,
    user: Option<String>,
    tier: TierArg,
    network: NetArg,
    memory: Option<u32>,
    cpus: Option<u16>,
    disk: Option<u64>,
}

async fn create(name: &str, paths: &VmPaths, a: CreateArgs) -> anyhow::Result<()> {
    if paths.data.join(VmConfig::FILE_NAME).exists() {
        bail!("VM {name:?} already exists in {}", paths.data.display());
    }
    let host = HostReport::probe();
    paths.create()?;
    let cache = paths
        .data
        .parent()
        .and_then(Path::parent)
        .context("data dir layout")?
        .join("cache");
    std::fs::create_dir_all(&cache)?;

    let profile = match a.profile {
        ProfileArg::Productivity => Profile::Productivity,
        ProfileArg::Development => Profile::Development,
        ProfileArg::Games => Profile::Games,
    };
    let (mem, cpus, disk) = profile.capped(host.mem_total_mib, host.logical_cpus.max(2));
    let tier = match a.tier {
        TierArg::Auto => host.recommended_tier(profile),
        TierArg::A => GpuTier::A,
        TierArg::B => GpuTier::B,
        TierArg::C => GpuTier::C,
        TierArg::D => GpuTier::D,
    };
    if !host.tiers.contains(&tier) {
        bail!(
            "GPU tier {tier:?} is not available on this host (available: {:?})",
            host.tiers
        );
    }
    let network = match a.network {
        NetArg::Auto if host.binaries.passt.is_some() => Network::Passt,
        NetArg::Auto | NetArg::User => Network::User,
        NetArg::Passt => Network::Passt,
        NetArg::None => Network::None,
    };

    // Tier A's test-signed driver needs Secure Boot off (alpha decision).
    let test_signing = tier == GpuTier::A;
    let fw = firmware::find(Path::new("/"), !test_signing)
        .or_else(|| firmware::find(Path::new("/"), false))
        .context(
            "no OVMF firmware found; install the 'ovmf' (Ubuntu) or 'edk2-ovmf' (Arch) package",
        )?;
    let vars = paths.data.join("OVMF_VARS.fd");
    firmware::instantiate_vars(&fw, &vars)?;

    let client = download::client()?;
    let virtio_iso = cache.join(format!("virtio-win-{}.iso", download::VIRTIO_WIN_VERSION));
    if !virtio_iso.exists() {
        eprintln!(
            "Downloading virtio-win {} drivers…",
            download::VIRTIO_WIN_VERSION
        );
        download::download(
            &client,
            download::VIRTIO_WIN_URL,
            &virtio_iso,
            Some(download::VIRTIO_WIN_SHA256),
            progress_printer(),
        )
        .await?;
        eprintln!();
    }

    let (windows_iso, image_name, image_index) = match a.source {
        Source::Iso => {
            let iso = a.iso.context("--source iso needs --iso PATH")?;
            anyhow::ensure!(iso.is_file(), "{} does not exist", iso.display());
            (iso, a.edition.clone(), None)
        }
        Source::Consumer => {
            eprintln!(
                "Resolving Windows 11 ({}) download from Microsoft…",
                a.language
            );
            let r = download::resolve_consumer(&client, &a.language).await?;
            let dest = cache.join(&r.file_name);
            fetch_cached(&client, &r, &dest).await?;
            (dest, "Windows 11 Pro".to_owned(), None)
        }
        Source::Evaluation => {
            eprintln!("Resolving Windows 11 Enterprise Evaluation download…");
            let r = download::resolve_evaluation(&client, "en-US").await?;
            let dest = cache.join(&r.file_name);
            fetch_cached(&client, &r, &dest).await?;
            (dest, "Windows 11 Enterprise Evaluation".to_owned(), Some(1))
        }
    };

    let username = a.user.unwrap_or_else(default_user);
    let password = random_password();
    let creds = paths.data.join("credentials.json");
    write_private(
        &creds,
        &serde_json::to_vec_pretty(
            &serde_json::json!({ "username": username, "password": password }),
        )?,
    )?;

    let locale = match a.language.as_str() {
        "English International" => "en-GB",
        _ => "en-US",
    };
    let xml = unattend::render(&unattend::UnattendOptions {
        username: username.clone(),
        full_name: username.clone(),
        password,
        locale: locale.into(),
        time_zone: "UTC".into(),
        computer_name: "LSW-WINDOWS".into(),
        image_name,
        image_index,
        product_key: None,
    })?;
    let setup_iso = paths.data.join("setup.iso");
    media::build_setup_media(
        &setup_iso,
        &xml,
        &media::FirstBootConfig { test_signing },
        None,
    )?;
    restrict_file(&setup_iso)?;

    let cfg = VmConfig {
        name: name.to_owned(),
        memory_mib: a.memory.unwrap_or(mem),
        vcpus: a.cpus.unwrap_or(cpus),
        disk_path: paths.data.join("disk.raw"),
        disk_size_gib: a.disk.unwrap_or(disk),
        firmware: Firmware {
            code: fw.code,
            vars,
            secure_boot: fw.ms_keys_enrolled,
        },
        tpm_state_dir: paths.data.join("tpm"),
        network,
        gpu_tier: tier,
        vsock_cid: host.vhost_vsock.then_some(3),
        install: Some(InstallMedia {
            windows_iso,
            virtio_win_iso: virtio_iso,
            setup_iso,
        }),
    };
    cfg.save(&paths.data)?;
    println!("Created VM {name:?} in {}", paths.data.display());
    println!(
        "  {} MiB, {} vCPUs, {} GiB disk, GPU tier {:?}, network {:?}, Secure Boot {}",
        cfg.memory_mib,
        cfg.vcpus,
        cfg.disk_size_gib,
        cfg.gpu_tier,
        cfg.network,
        yes(cfg.firmware.secure_boot)
    );
    println!(
        "  Windows account: {username} (password in {})",
        creds.display()
    );
    println!("Next: lsw install");
    Ok(())
}

async fn fetch_cached(
    client: &download::Client,
    r: &download::ResolvedIso,
    dest: &Path,
) -> anyhow::Result<()> {
    if dest.exists() {
        if let Some(expected) = &r.sha256 {
            eprintln!("Verifying cached {}…", dest.display());
            let actual = download::sha256_file(dest).await?;
            anyhow::ensure!(
                actual.eq_ignore_ascii_case(expected),
                "cached ISO has the wrong checksum; delete {}",
                dest.display()
            );
        }
        return Ok(());
    }
    eprintln!("Downloading {}…", r.file_name);
    let sha = download::download(
        client,
        &r.url,
        dest,
        r.sha256.as_deref(),
        progress_printer(),
    )
    .await?;
    eprintln!(
        "\nSHA-256 {sha}{}",
        if r.sha256.is_some() {
            " (matches Microsoft's published hash)"
        } else {
            ""
        }
    );
    Ok(())
}

fn progress_printer() -> impl FnMut(u64, Option<u64>) {
    let mut last = std::time::Instant::now() - Duration::from_secs(1);
    move |done, total| {
        if last.elapsed() < Duration::from_millis(500) {
            return;
        }
        last = std::time::Instant::now();
        match total {
            Some(t) if t > 0 => eprint!(
                "\r  {:>6.1} / {:.1} MiB ({:.0}%)",
                mib(done),
                mib(t),
                done as f64 * 100.0 / t as f64
            ),
            _ => eprint!("\r  {:>6.1} MiB", mib(done)),
        }
    }
}

fn mib(b: u64) -> f64 {
    b as f64 / (1024.0 * 1024.0)
}

fn default_user() -> String {
    let raw = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let clean: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(20)
        .collect();
    match clean.to_ascii_lowercase().as_str() {
        "" | "administrator" | "guest" => "lswuser".into(),
        _ => clean,
    }
}

/// 24 random characters from two v4 UUIDs (the OS CSPRNG via getrandom).
fn random_password() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("Lsw-{}{}", &a[..10], &b[..10])
}

fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    std::fs::write(path, data)?;
    restrict_file(path)
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file(_: &Path) -> anyhow::Result<()> {
    Ok(())
}

fn launch_options(cfg: &VmConfig, host: &HostReport, l: &LaunchArgs) -> LaunchOptions {
    LaunchOptions {
        display: match &l.vnc {
            Some(p) => lsw_daemon::qemu::Display::VncUnix(p.clone()),
            None => lsw_daemon::qemu::Display::Headless,
        },
        disk_io: if l.disk_threads {
            DiskIo::Threads
        } else {
            DiskIo::IoUring
        },
        vsock: host.vhost_vsock && cfg.vsock_cid.is_some(),
        sandbox: !l.no_sandbox,
    }
}

async fn install(
    paths: &VmPaths,
    timeout_min: u64,
    screenshot: Option<PathBuf>,
    l: &LaunchArgs,
) -> anyhow::Result<()> {
    let mut cfg = VmConfig::load(&paths.data)?;
    let install = cfg
        .install
        .clone()
        .context("this VM is already installed")?;
    let host = HostReport::probe();
    let opts = launch_options(&cfg, &host, l);
    let _ = std::fs::remove_file(paths.serial_log());
    let started = std::time::Instant::now();
    let mut running = Vm::new(cfg.clone(), paths.clone())
        .start(&host.binaries, &opts)
        .await?;
    eprintln!("VM started; getting past the \"Press any key to boot from CD\" prompt…");
    running
        .press_key_repeatedly("ret", Duration::from_millis(500), Duration::from_secs(20))
        .await;
    eprintln!("Windows Setup is running unattended. Progress (from the guest's serial console):");
    let marker = unattend::INSTALL_DONE_MARKER;
    let serial = paths.serial_log();
    let wait = vm::wait_for_serial_marker(
        &serial,
        marker,
        Duration::from_secs(timeout_min * 60),
        |line| {
            if !line.trim().is_empty() {
                eprintln!(
                    "  [{:>5.1} min] {line}",
                    started.elapsed().as_secs_f64() / 60.0
                );
            }
        },
    );
    tokio::select! {
        r = wait => { r?; }
        status = running.wait() => {
            bail!("QEMU exited during installation ({:?}); see {}\n{}", status?, paths.qemu_log().display(), vm::tail(&paths.qemu_log(), 30));
        }
    }
    eprintln!(
        "Installation finished in {:.1} minutes.",
        started.elapsed().as_secs_f64() / 60.0
    );
    // Give the desktop a moment to settle for the screenshot.
    tokio::time::sleep(Duration::from_secs(15)).await;
    let shot = screenshot.unwrap_or_else(|| paths.data.join("installed.png"));
    running
        .qmp
        .screendump_png(&shot)
        .await
        .context("screenshot")?;
    eprintln!("Screenshot: {}", shot.display());
    let status = running.shutdown(Duration::from_secs(180)).await?;
    eprintln!("VM powered off ({status}).");

    cfg.install = None;
    cfg.save(&paths.data)?;
    // The setup image contains the account password.
    let _ = std::fs::remove_file(&install.setup_iso);
    println!("Windows is installed. Start it with: lsw run");
    Ok(())
}

async fn run(paths: &VmPaths, l: &LaunchArgs) -> anyhow::Result<()> {
    let cfg = VmConfig::load(&paths.data)?;
    if cfg.install.is_some() {
        bail!("Windows is not installed yet; run `lsw install` first");
    }
    let host = HostReport::probe();
    let opts = launch_options(&cfg, &host, l);
    let mut running = Vm::new(cfg, paths.clone())
        .start(&host.binaries, &opts)
        .await?;
    println!("VM running. Control it with `lsw ctl …`; Ctrl+C shuts Windows down.");
    tokio::select! {
        status = running.wait() => println!("QEMU exited: {}", status?),
        _ = tokio::signal::ctrl_c() => {
            println!("Shutting Windows down…");
            let status = running.shutdown(Duration::from_secs(120)).await?;
            println!("VM powered off ({status}).");
        }
    }
    Ok(())
}

async fn ctl(paths: &VmPaths, action: CtlAction) -> anyhow::Result<()> {
    let mut qmp = Qmp::connect_with_retry(&paths.ctl_socket(), Duration::from_secs(2))
        .await
        .context("VM is not running (no control socket)")?;
    match action {
        CtlAction::Status => println!("{}", qmp.status().await?),
        CtlAction::Pause => {
            qmp.execute("stop", None).await?;
            println!("paused");
        }
        CtlAction::Resume => {
            qmp.execute("cont", None).await?;
            println!("running");
        }
        CtlAction::Screenshot { path } => {
            let abs = std::path::absolute(&path)?;
            qmp.screendump_png(&abs).await?;
            println!("{}", abs.display());
        }
        CtlAction::Powerdown => {
            qmp.execute("system_powerdown", None).await?;
            println!("power button pressed");
        }
        CtlAction::Quit => {
            let _ = qmp.execute("quit", None).await;
            println!("quit");
        }
    }
    Ok(())
}
