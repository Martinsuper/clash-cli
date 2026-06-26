use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use regex::RegexSet;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_yaml::{Mapping, Value};
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    process::{Command as StdCommand, Stdio},
    time::Duration,
};
use tokio::{net::TcpStream, process::Command, time::sleep};
use tracing::{debug, info, warn};

const APP_QUALIFIER: &str = "io.github";
const APP_ORG: &str = "clash-cli";
const APP_NAME: &str = "clash-cli";
const DEFAULT_TEST_URL: &str = "http://cp.cloudflare.com/generate_204";

#[derive(Parser, Debug)]
#[command(name = "clash-cli")]
#[command(
    about = "A small Rust CLI wrapper around mihomo with subscription update and auto switching."
)]
#[command(subcommand_precedence_over_arg = true)]
struct Cli {
    /// First-run shortcut: pass a Clash/Mihomo subscription URL and start immediately.
    #[arg(value_name = "SUBSCRIPTION_URL")]
    quick_subscription: Option<String>,

    /// Add or replace subscription URLs in the config before running the command.
    #[arg(short = 's', long = "subscribe", global = true)]
    subscriptions: Vec<String>,

    /// Override the mihomo binary path in the config.
    #[arg(long, global = true)]
    mihomo_bin: Option<PathBuf>,

    /// Enable TUN mode in the config before running the command.
    #[arg(long, global = true)]
    tun: bool,

    /// Do not start mihomo; use an already running controller.
    #[arg(long, global = true)]
    no_core: bool,

    #[arg(short, long, global = true, env = "CLASH_CLI_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a sample config file.
    Init {
        #[arg(short, long)]
        subscription: Vec<String>,

        #[arg(long)]
        mihomo_bin: Option<PathBuf>,

        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Download subscriptions and generate the mihomo runtime config.
    Update,

    /// Start mihomo and keep checking/switching nodes.
    Run,

    /// Test nodes and switch the selector to the fastest reachable one.
    Switch,

    /// Check current connectivity; update subscriptions and switch if needed.
    Check,

    /// Print useful paths.
    Paths,

    /// Diagnose config, ports, controller, and target URL connectivity.
    Doctor {
        #[arg(default_value = DEFAULT_TEST_URL)]
        url: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct AppConfig {
    subscriptions: Vec<Subscription>,
    mihomo: MihomoConfig,
    controller: ControllerConfig,
    proxy: ProxyConfig,
    tun: TunConfig,
    dns: DnsConfig,
    rules: Vec<String>,
    rule_providers: Mapping,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            subscriptions: vec![],
            mihomo: MihomoConfig::default(),
            controller: ControllerConfig::default(),
            proxy: ProxyConfig::default(),
            tun: TunConfig::default(),
            dns: DnsConfig::default(),
            rules: vec!["MATCH,PROXY".to_string()],
            rule_providers: Mapping::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Subscription {
    name: String,
    url: String,
    user_agent: Option<String>,
    include: Vec<String>,
    exclude: Vec<String>,
}

impl Default for Subscription {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            url: String::new(),
            user_agent: None,
            include: vec![],
            exclude: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct MihomoConfig {
    bin: PathBuf,
    mixed_port: u16,
    allow_lan: bool,
    mode: String,
    log_level: String,
}

impl Default for MihomoConfig {
    fn default() -> Self {
        Self {
            bin: PathBuf::from("mihomo"),
            mixed_port: 7890,
            allow_lan: false,
            mode: "rule".to_string(),
            log_level: "info".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct ControllerConfig {
    host: String,
    port: u16,
    secret: String,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 9090,
            secret: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct ProxyConfig {
    selector: String,
    auto_group: String,
    test_url: String,
    timeout_ms: u64,
    interval_secs: u64,
    health_check_secs: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            selector: "PROXY".to_string(),
            auto_group: "AUTO".to_string(),
            test_url: DEFAULT_TEST_URL.to_string(),
            timeout_ms: 5000,
            interval_secs: 300,
            health_check_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct TunConfig {
    enable: bool,
    stack: String,
    auto_route: bool,
    auto_detect_interface: bool,
    dns_hijack: Vec<String>,
}

impl Default for TunConfig {
    fn default() -> Self {
        Self {
            enable: false,
            stack: "system".to_string(),
            auto_route: true,
            auto_detect_interface: true,
            dns_hijack: vec!["any:53".to_string()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct DnsConfig {
    enable: bool,
    #[serde(default = "default_dns_listen")]
    listen: String,
    enhanced_mode: String,
    fake_ip_range: String,
    nameserver: Vec<String>,
    fallback: Vec<String>,
}

fn default_dns_listen() -> String {
    "0.0.0.0:53".to_string()
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            enable: true,
            listen: default_dns_listen(),
            enhanced_mode: "fake-ip".to_string(),
            fake_ip_range: "198.18.0.1/16".to_string(),
            nameserver: vec!["223.5.5.5".to_string(), "119.29.29.29".to_string()],
            fallback: vec![
                "https://1.1.1.1/dns-query".to_string(),
                "https://dns.google/dns-query".to_string(),
            ],
        }
    }
}

#[derive(Debug)]
struct Paths {
    config: PathBuf,
    data_dir: PathBuf,
    runtime_config: PathBuf,
    cache_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ProxiesResponse {
    proxies: HashMap<String, ProxyInfo>,
}

#[derive(Debug, Deserialize)]
struct ProxyInfo {
    #[serde(default)]
    all: Vec<String>,
    #[serde(default)]
    now: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DelayResponse {
    delay: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("clash_cli=info".parse()?),
        )
        .init();

    let cli = Cli::parse();
    let paths = resolve_paths(cli.config.clone())?;
    let quick_setup = QuickSetup::from_cli(&cli);
    let no_core = cli.no_core;
    let refresh_on_start = quick_setup.has_changes();
    if quick_setup.has_changes() {
        apply_quick_setup(&paths, &quick_setup)?;
    }

    match cli.command {
        Some(Commands::Init {
            subscription,
            mihomo_bin,
            force,
        }) => init_config(&paths, subscription, mihomo_bin, force),
        Some(Commands::Update) => {
            let cfg = load_config(&paths)?;
            update_runtime_config(&paths, &cfg).await?;
            println!("{}", paths.runtime_config.display());
            Ok(())
        }
        Some(Commands::Run) | None => run(paths, no_core, refresh_on_start).await,
        Some(Commands::Switch) => {
            let cfg = load_config(&paths)?;
            let client = ApiClient::new(&cfg)?;
            let best = choose_and_switch(&client, &cfg).await?;
            println!("selected {} ({} ms)", best.name, best.delay);
            Ok(())
        }
        Some(Commands::Check) => check(paths).await,
        Some(Commands::Paths) => {
            println!("config: {}", paths.config.display());
            println!("data: {}", paths.data_dir.display());
            println!("runtime: {}", paths.runtime_config.display());
            println!("cache: {}", paths.cache_dir.display());
            Ok(())
        }
        Some(Commands::Doctor { url }) => doctor(paths, &url).await,
    }
}

#[derive(Debug)]
struct QuickSetup {
    subscriptions: Vec<String>,
    mihomo_bin: Option<PathBuf>,
    tun: bool,
}

impl QuickSetup {
    fn from_cli(cli: &Cli) -> Self {
        let mut subscriptions = Vec::new();
        if let Some(url) = &cli.quick_subscription {
            subscriptions.push(url.clone());
        }
        subscriptions.extend(cli.subscriptions.clone());
        Self {
            subscriptions,
            mihomo_bin: cli.mihomo_bin.clone(),
            tun: cli.tun,
        }
    }

    fn has_changes(&self) -> bool {
        !self.subscriptions.is_empty() || self.mihomo_bin.is_some() || self.tun
    }
}

fn resolve_paths(config: Option<PathBuf>) -> Result<Paths> {
    let dirs = ProjectDirs::from(APP_QUALIFIER, APP_ORG, APP_NAME)
        .ok_or_else(|| anyhow!("failed to resolve platform config directories"))?;
    let config = config.unwrap_or_else(|| dirs.config_dir().join("config.yaml"));
    let data_dir = dirs.data_dir().to_path_buf();
    let cache_dir = dirs.cache_dir().to_path_buf();
    let runtime_config = data_dir.join("runtime.yaml");
    Ok(Paths {
        config,
        data_dir,
        runtime_config,
        cache_dir,
    })
}

fn init_config(
    paths: &Paths,
    urls: Vec<String>,
    mihomo_bin: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    if paths.config.exists() && !force {
        bail!(
            "config already exists: {}. Pass --force to overwrite.",
            paths.config.display()
        );
    }

    if let Some(parent) = paths.config.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut cfg = AppConfig::default();
    cfg.subscriptions = urls
        .into_iter()
        .enumerate()
        .map(|(idx, url)| Subscription {
            name: format!("sub-{}", idx + 1),
            url,
            user_agent: Some("clash-cli/0.1".to_string()),
            include: vec![],
            exclude: vec![],
        })
        .collect();
    if let Some(bin) = mihomo_bin {
        cfg.mihomo.bin = bin;
    }

    let text = serde_yaml::to_string(&cfg)?;
    fs::write(&paths.config, text)?;
    println!("{}", paths.config.display());
    Ok(())
}

fn apply_quick_setup(paths: &Paths, setup: &QuickSetup) -> Result<()> {
    let mut cfg = if paths.config.exists() {
        let raw = fs::read_to_string(&paths.config)
            .with_context(|| format!("failed to read config: {}", paths.config.display()))?;
        serde_yaml::from_str::<AppConfig>(&raw)
            .with_context(|| format!("failed to parse config: {}", paths.config.display()))?
    } else {
        AppConfig::default()
    };

    if !setup.subscriptions.is_empty() {
        cfg.subscriptions = setup
            .subscriptions
            .iter()
            .enumerate()
            .map(|(idx, url)| Subscription {
                name: format!("sub-{}", idx + 1),
                url: url.clone(),
                user_agent: Some("clash-cli/0.1".to_string()),
                include: vec![],
                exclude: vec![],
            })
            .collect();
    }

    if let Some(bin) = &setup.mihomo_bin {
        cfg.mihomo.bin = bin.clone();
    }
    if setup.tun {
        cfg.tun.enable = true;
    }

    save_config(paths, &cfg)?;
    println!("config saved: {}", paths.config.display());
    Ok(())
}

fn save_config(paths: &Paths, cfg: &AppConfig) -> Result<()> {
    if let Some(parent) = paths.config.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&paths.config, serde_yaml::to_string(cfg)?)?;
    Ok(())
}

fn load_config(paths: &Paths) -> Result<AppConfig> {
    let raw = fs::read_to_string(&paths.config)
        .with_context(|| {
            format!(
                "failed to read config: {}. First run can be as simple as: clash-cli \"YOUR_SUBSCRIPTION_URL\"",
                paths.config.display()
            )
        })?;
    let cfg: AppConfig = serde_yaml::from_str(&raw)
        .with_context(|| format!("failed to parse config: {}", paths.config.display()))?;
    if cfg.subscriptions.is_empty() {
        bail!("no subscriptions configured in {}", paths.config.display());
    }
    Ok(cfg)
}

async fn run(paths: Paths, no_core: bool, refresh_on_start: bool) -> Result<()> {
    let cfg = load_config(&paths)?;
    ensure_runtime_config(&paths, &cfg, refresh_on_start).await?;

    let mut child = if no_core {
        None
    } else {
        ensure_can_start_core(&cfg)?;
        Some(start_mihomo(&paths, &cfg).await?)
    };

    wait_for_controller(&cfg).await?;
    check_once(&paths, &cfg).await?;

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("received Ctrl-C");
                if let Some(child) = child.as_mut() {
                    let _ = child.kill().await;
                }
                return Ok(());
            }
            _ = sleep(Duration::from_secs(cfg.proxy.health_check_secs.max(5))) => {
                if let Err(err) = check_once(&paths, &cfg).await {
                    warn!("health check failed: {err:#}");
                }
                if let Some(child) = child.as_mut()
                    && let Some(status) = child.try_wait()? {
                    bail!("mihomo exited: {status}");
                }
            }
        }
    }
}

fn ensure_can_start_core(cfg: &AppConfig) -> Result<()> {
    if cfg.tun.enable && !is_elevated() {
        bail!(
            "TUN mode usually requires administrator privileges. Try: sudo {} --tun",
            std::env::current_exe()
                .ok()
                .and_then(|path| path.into_os_string().into_string().ok())
                .unwrap_or_else(|| "clash-cli".to_string())
        );
    }
    Ok(())
}

#[cfg(unix)]
fn is_elevated() -> bool {
    StdCommand::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|uid| uid.trim().parse::<u32>().ok())
        == Some(0)
}

#[cfg(not(unix))]
fn is_elevated() -> bool {
    false
}

async fn ensure_runtime_config(paths: &Paths, cfg: &AppConfig, refresh: bool) -> Result<()> {
    if refresh || !paths.runtime_config.exists() {
        update_runtime_config(paths, cfg).await?;
    } else {
        info!(
            "using cached runtime config: {}",
            paths.runtime_config.display()
        );
    }
    Ok(())
}

async fn check(paths: Paths) -> Result<()> {
    let cfg = load_config(&paths)?;
    check_once(&paths, &cfg).await
}

async fn doctor(paths: Paths, url: &str) -> Result<()> {
    println!("clash-cli doctor");
    println!("target: {url}");
    println!("config: {}", paths.config.display());
    println!("runtime: {}", paths.runtime_config.display());

    let cfg = match load_config(&paths) {
        Ok(cfg) => {
            report_ok("config", "loaded");
            cfg
        }
        Err(err) => {
            report_fail("config", &err.to_string());
            return Err(err);
        }
    };

    if paths.runtime_config.exists() {
        report_ok("runtime", "exists");
    } else {
        report_fail("runtime", "missing; run `clash-cli update` first");
    }

    if cfg.mihomo.bin.components().count() > 1 {
        if cfg.mihomo.bin.exists() {
            report_ok("mihomo binary", &cfg.mihomo.bin.display().to_string());
        } else {
            report_fail(
                "mihomo binary",
                &format!("not found: {}", cfg.mihomo.bin.display()),
            );
        }
    } else {
        report_info(
            "mihomo binary",
            &format!("using PATH command `{}`", cfg.mihomo.bin.display()),
        );
    }

    let controller_addr = format!("{}:{}", cfg.controller.host, cfg.controller.port);
    if tcp_check(&controller_addr).await {
        report_ok("controller tcp", &controller_addr);
    } else {
        report_fail(
            "controller tcp",
            &format!("{controller_addr} is not listening"),
        );
    }

    let mixed_addr = format!("127.0.0.1:{}", cfg.mihomo.mixed_port);
    if tcp_check(&mixed_addr).await {
        report_ok("mixed proxy tcp", &mixed_addr);
    } else {
        report_fail("mixed proxy tcp", &format!("{mixed_addr} is not listening"));
    }

    let api = ApiClient::new(&cfg)?;
    match api.get_proxies().await {
        Ok(proxies) => {
            if let Some(selector) = proxies.proxies.get(&cfg.proxy.selector) {
                report_ok(
                    "controller api",
                    &format!(
                        "{} now={} candidates={}",
                        cfg.proxy.selector,
                        selector.now.as_deref().unwrap_or("<none>"),
                        selector.all.len()
                    ),
                );
            } else {
                report_fail(
                    "controller api",
                    &format!("selector `{}` not found", cfg.proxy.selector),
                );
            }
        }
        Err(err) => report_fail("controller api", &err.to_string()),
    }

    match http_probe(url, None).await {
        Ok(result) => report_ok("direct request", &result),
        Err(err) => report_fail("direct request", &err.to_string()),
    }

    match http_probe(url, Some(cfg.mihomo.mixed_port)).await {
        Ok(result) => report_ok("proxy request", &result),
        Err(err) => report_fail("proxy request", &err.to_string()),
    }

    if cfg.tun.enable {
        if tun_route_hint() {
            report_ok("tun route hint", "198.18.0.1 route/interface found");
        } else {
            report_fail(
                "tun route hint",
                "TUN is enabled in config, but 198.18.0.1 was not found",
            );
        }
    } else {
        report_info("tun", "disabled in config");
    }

    if let Some(proxy) = system_proxy_hint() {
        if proxy.enabled && proxy.port != Some(cfg.mihomo.mixed_port) {
            report_warn(
                "system proxy hint",
                &format!(
                    "Wi-Fi HTTP proxy points to {}:{}, but clash-cli mixed-port is {}",
                    proxy.server.as_deref().unwrap_or(""),
                    proxy.port.map(|port| port.to_string()).unwrap_or_default(),
                    cfg.mihomo.mixed_port
                ),
            );
        } else {
            report_info("system proxy hint", &proxy.summary());
        }
    }

    if let Some(processes) = mihomo_process_hint()
        && !processes.is_empty()
    {
        report_info("related processes", &processes.join(" | "));
    }

    Ok(())
}

async fn tcp_check(addr: &str) -> bool {
    tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr))
        .await
        .is_ok_and(|result| result.is_ok())
}

async fn http_probe(url: &str, proxy_port: Option<u16>) -> Result<String> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::limited(5));
    if let Some(port) = proxy_port {
        builder = builder.proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{port}"))?);
    } else {
        builder = builder.no_proxy();
    }

    let started = std::time::Instant::now();
    let response = builder.build()?.get(url).send().await?;
    Ok(format!(
        "HTTP {} in {} ms",
        response.status(),
        started.elapsed().as_millis()
    ))
}

fn report_ok(label: &str, detail: &str) {
    println!("[OK]   {label}: {detail}");
}

fn report_fail(label: &str, detail: &str) {
    println!("[FAIL] {label}: {detail}");
}

fn report_warn(label: &str, detail: &str) {
    println!("[WARN] {label}: {detail}");
}

fn report_info(label: &str, detail: &str) {
    println!("[INFO] {label}: {detail}");
}

fn tun_route_hint() -> bool {
    StdCommand::new("ifconfig")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|text| text.contains("198.18.0.1"))
}

#[derive(Debug)]
struct SystemProxyHint {
    enabled: bool,
    server: Option<String>,
    port: Option<u16>,
}

impl SystemProxyHint {
    fn summary(&self) -> String {
        format!(
            "Wi-Fi HTTP enabled={} server={} port={}",
            if self.enabled { "Yes" } else { "No" },
            self.server.as_deref().unwrap_or(""),
            self.port.map(|port| port.to_string()).unwrap_or_default()
        )
    }
}

fn system_proxy_hint() -> Option<SystemProxyHint> {
    let output = StdCommand::new("networksetup")
        .args(["-getwebproxy", "Wi-Fi"])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let enabled = text
        .lines()
        .find_map(|line| line.strip_prefix("Enabled: "))?;
    let server = text
        .lines()
        .find_map(|line| line.strip_prefix("Server: "))
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let port = text
        .lines()
        .find_map(|line| line.strip_prefix("Port: "))
        .and_then(|value| value.parse::<u16>().ok());
    Some(SystemProxyHint {
        enabled: enabled == "Yes",
        server,
        port,
    })
}

fn mihomo_process_hint() -> Option<Vec<String>> {
    let output = StdCommand::new("pgrep")
        .args(["-af", "verge-mihomo|mihomo|clash-cli|Clash Verge"])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    Some(
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(6)
            .map(ToOwned::to_owned)
            .collect(),
    )
}

async fn check_once(paths: &Paths, cfg: &AppConfig) -> Result<()> {
    let client = ApiClient::new(cfg)?;
    match choose_and_switch(&client, cfg).await {
        Ok(best) => {
            info!("proxy ok: {} ({} ms)", best.name, best.delay);
            Ok(())
        }
        Err(first_err) => {
            warn!("no reachable proxy before update: {first_err:#}");
            update_runtime_config(paths, cfg).await?;
            let _ = client.reload_config(&paths.runtime_config).await;
            let best = choose_and_switch(&client, cfg).await?;
            info!(
                "proxy recovered after subscription update: {} ({} ms)",
                best.name, best.delay
            );
            Ok(())
        }
    }
}

async fn start_mihomo(paths: &Paths, cfg: &AppConfig) -> Result<tokio::process::Child> {
    fs::create_dir_all(&paths.data_dir)?;
    info!("starting mihomo: {}", cfg.mihomo.bin.display());
    let child = Command::new(&cfg.mihomo.bin)
        .arg("-f")
        .arg(&paths.runtime_config)
        .arg("-d")
        .arg(&paths.data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "failed to start mihomo binary: {}",
                cfg.mihomo.bin.display()
            )
        })?;
    Ok(child)
}

async fn wait_for_controller(cfg: &AppConfig) -> Result<()> {
    let client = ApiClient::new(cfg)?;
    for _ in 0..40 {
        if client.get_proxies().await.is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(500)).await;
    }
    bail!("mihomo external-controller did not become ready");
}

async fn update_runtime_config(paths: &Paths, cfg: &AppConfig) -> Result<()> {
    fs::create_dir_all(&paths.data_dir)?;
    fs::create_dir_all(&paths.cache_dir)?;

    let http = Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("clash-cli/0.1")
        .build()?;

    let mut proxies = Vec::new();
    let mut proxy_names = BTreeSet::new();

    for sub in &cfg.subscriptions {
        info!("updating subscription {}", sub.name);
        let body = download_subscription(&http, sub).await?;
        fs::write(
            paths
                .cache_dir
                .join(format!("{}.yaml", sanitize_name(&sub.name))),
            &body,
        )?;
        let sub_config = parse_subscription(&body)?;
        let sub_proxies = read_proxy_items(&sub_config)?;
        let include = compile_patterns(&sub.include)?;
        let exclude = compile_patterns(&sub.exclude)?;

        for proxy in sub_proxies {
            let name = proxy_name(&proxy)?;
            if !matches_filter(&name, include.as_ref(), exclude.as_ref()) {
                continue;
            }
            if proxy_names.insert(name) {
                proxies.push(proxy);
            }
        }
    }

    if proxies.is_empty() {
        bail!("subscriptions did not provide any proxy after filtering");
    }

    let runtime = build_runtime_config(cfg, proxies, proxy_names.into_iter().collect())?;
    fs::write(&paths.runtime_config, serde_yaml::to_string(&runtime)?)?;
    info!("runtime config written: {}", paths.runtime_config.display());
    Ok(())
}

async fn download_subscription(http: &Client, sub: &Subscription) -> Result<String> {
    let mut req = http.get(&sub.url);
    if let Some(ua) = &sub.user_agent {
        req = req.header(reqwest::header::USER_AGENT, ua);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("failed to download {}", sub.name))?;
    if resp.status() != StatusCode::OK {
        bail!("subscription {} returned HTTP {}", sub.name, resp.status());
    }
    Ok(resp.text().await?)
}

fn parse_subscription(body: &str) -> Result<Value> {
    if let Ok(value) = serde_yaml::from_str::<Value>(body)
        && value.get("proxies").is_some()
    {
        return Ok(value);
    }

    let compact = body.lines().map(str::trim).collect::<String>();
    if let Ok(bytes) = general_purpose::STANDARD.decode(compact)
        && let Ok(decoded) = String::from_utf8(bytes)
        && let Ok(value) = serde_yaml::from_str::<Value>(&decoded)
        && value.get("proxies").is_some()
    {
        return Ok(value);
    }

    if let Some(value) = parse_uri_subscription(body)? {
        return Ok(value);
    }

    let compact = body.lines().map(str::trim).collect::<String>();
    if let Ok(decoded) = decode_base64_to_string(&compact)
        && let Some(value) = parse_uri_subscription(&decoded)?
    {
        return Ok(value);
    }

    bail!("subscription is not a Clash/Mihomo YAML document or supported URI list.")
}

fn parse_uri_subscription(body: &str) -> Result<Option<Value>> {
    let mut proxies = Vec::new();
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.starts_with("ss://") {
            proxies.push(parse_ss_uri(line)?);
        } else if line.starts_with("vmess://") {
            proxies.push(parse_vmess_uri(line)?);
        }
    }

    if proxies.is_empty() {
        return Ok(None);
    }

    let mut root = Mapping::new();
    insert(&mut root, "proxies", Value::Sequence(proxies));
    Ok(Some(Value::Mapping(root)))
}

fn parse_ss_uri(uri: &str) -> Result<Value> {
    let raw = uri
        .strip_prefix("ss://")
        .ok_or_else(|| anyhow!("invalid ss uri"))?;
    let (main, fragment) = split_once(raw, '#');
    let name = decode_url_component(fragment).unwrap_or_else(|| "ss".to_string());
    let (main, _query) = split_once(main, '?');

    let decoded_main = if main.contains('@') {
        main.to_string()
    } else {
        decode_base64_to_string(main)?
    };

    let (user_info, server_info) = decoded_main
        .rsplit_once('@')
        .ok_or_else(|| anyhow!("invalid ss uri: missing server"))?;
    let (cipher, password) = user_info
        .split_once(':')
        .ok_or_else(|| anyhow!("invalid ss uri: missing cipher"))?;
    let (server, port) = split_host_port(server_info)?;

    let mut proxy = Mapping::new();
    insert(&mut proxy, "name", name);
    insert(&mut proxy, "type", "ss");
    insert(&mut proxy, "server", server);
    insert(&mut proxy, "port", port);
    insert(
        &mut proxy,
        "cipher",
        decode_url_component(cipher).unwrap_or_else(|| cipher.to_string()),
    );
    insert(
        &mut proxy,
        "password",
        decode_url_component(password).unwrap_or_else(|| password.to_string()),
    );
    insert(&mut proxy, "udp", true);
    Ok(Value::Mapping(proxy))
}

fn parse_vmess_uri(uri: &str) -> Result<Value> {
    let raw = uri
        .strip_prefix("vmess://")
        .ok_or_else(|| anyhow!("invalid vmess uri"))?;
    let decoded = decode_base64_to_string(raw)?;
    let json: serde_json::Value = serde_json::from_str(&decoded)?;

    let name = json_string(&json, "ps").unwrap_or_else(|| "vmess".to_string());
    let server = json_string(&json, "add").ok_or_else(|| anyhow!("vmess missing add"))?;
    let port = json_string(&json, "port")
        .ok_or_else(|| anyhow!("vmess missing port"))?
        .parse::<u16>()?;
    let uuid = json_string(&json, "id").ok_or_else(|| anyhow!("vmess missing id"))?;
    let alter_id = json_string(&json, "aid")
        .and_then(|aid| aid.parse::<u16>().ok())
        .unwrap_or(0);
    let cipher = json_string(&json, "scy").unwrap_or_else(|| "auto".to_string());
    let network = json_string(&json, "net").unwrap_or_default();
    let tls = json_string(&json, "tls")
        .map(|tls| !tls.is_empty() && tls != "none")
        .unwrap_or(false);

    let mut proxy = Mapping::new();
    insert(&mut proxy, "name", name);
    insert(&mut proxy, "type", "vmess");
    insert(&mut proxy, "server", server);
    insert(&mut proxy, "port", port);
    insert(&mut proxy, "uuid", uuid);
    insert(&mut proxy, "alterId", alter_id);
    insert(&mut proxy, "cipher", cipher);
    insert(&mut proxy, "udp", true);

    if tls {
        insert(&mut proxy, "tls", true);
        if let Some(sni) = json_string(&json, "sni").filter(|sni| !sni.is_empty()) {
            insert(&mut proxy, "servername", sni);
        }
    }

    if !network.is_empty() && network != "tcp" {
        insert(&mut proxy, "network", network.as_str());
    }

    if network == "ws" {
        let mut ws_opts = Mapping::new();
        if let Some(path) = json_string(&json, "path").filter(|path| !path.is_empty()) {
            insert(&mut ws_opts, "path", path);
        }
        if let Some(host) = json_string(&json, "host").filter(|host| !host.is_empty()) {
            let mut headers = Mapping::new();
            insert(&mut headers, "Host", host);
            insert(&mut ws_opts, "headers", Value::Mapping(headers));
        }
        if !ws_opts.is_empty() {
            insert(&mut proxy, "ws-opts", Value::Mapping(ws_opts));
        }
    }

    Ok(Value::Mapping(proxy))
}

fn decode_base64_to_string(input: &str) -> Result<String> {
    let normalized = input.trim().replace('-', "+").replace('_', "/");
    let padded = match normalized.len() % 4 {
        0 => normalized,
        rem => format!("{}{}", normalized, "=".repeat(4 - rem)),
    };
    let bytes = general_purpose::STANDARD.decode(padded)?;
    Ok(String::from_utf8(bytes)?)
}

fn decode_url_component(input: &str) -> Option<String> {
    urlencoding::decode(input)
        .ok()
        .map(|value| value.into_owned())
}

fn split_once<'a>(value: &'a str, delimiter: char) -> (&'a str, &'a str) {
    value.split_once(delimiter).unwrap_or((value, ""))
}

fn split_host_port(value: &str) -> Result<(String, u16)> {
    let (host, port) = value
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("missing host port"))?;
    Ok((
        host.trim_matches(['[', ']']).to_string(),
        port.parse::<u16>()?,
    ))
}

fn json_string(json: &serde_json::Value, key: &str) -> Option<String> {
    json.get(key).and_then(|value| match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn read_proxy_items(config: &Value) -> Result<Vec<Value>> {
    let proxies = config
        .get("proxies")
        .and_then(Value::as_sequence)
        .ok_or_else(|| anyhow!("subscription missing proxies array"))?;
    Ok(proxies.clone())
}

fn build_runtime_config(cfg: &AppConfig, proxies: Vec<Value>, names: Vec<String>) -> Result<Value> {
    let mut root = Mapping::new();
    insert(&mut root, "mixed-port", cfg.mihomo.mixed_port);
    insert(&mut root, "allow-lan", cfg.mihomo.allow_lan);
    insert(&mut root, "mode", cfg.mihomo.mode.clone());
    insert(&mut root, "log-level", cfg.mihomo.log_level.clone());
    insert(
        &mut root,
        "external-controller",
        format!("{}:{}", cfg.controller.host, cfg.controller.port),
    );
    if !cfg.controller.secret.is_empty() {
        insert(&mut root, "secret", cfg.controller.secret.clone());
    }
    insert(&mut root, "proxies", Value::Sequence(proxies));
    insert(
        &mut root,
        "proxy-groups",
        Value::Sequence(build_proxy_groups(cfg, &names)),
    );
    insert(
        &mut root,
        "rules",
        Value::Sequence(cfg.rules.iter().cloned().map(Value::String).collect()),
    );

    if !cfg.rule_providers.is_empty() {
        insert(
            &mut root,
            "rule-providers",
            Value::Mapping(cfg.rule_providers.clone()),
        );
    }

    insert(&mut root, "tun", build_tun(cfg));
    insert(&mut root, "dns", build_dns(cfg));
    Ok(Value::Mapping(root))
}

fn build_proxy_groups(cfg: &AppConfig, names: &[String]) -> Vec<Value> {
    let mut groups = Vec::new();
    let proxy_name_values: Vec<Value> = names.iter().cloned().map(Value::String).collect();

    let mut auto = Mapping::new();
    insert(&mut auto, "name", cfg.proxy.auto_group.clone());
    insert(&mut auto, "type", "url-test");
    insert(&mut auto, "url", cfg.proxy.test_url.clone());
    insert(&mut auto, "interval", cfg.proxy.interval_secs);
    insert(
        &mut auto,
        "proxies",
        Value::Sequence(proxy_name_values.clone()),
    );
    groups.push(Value::Mapping(auto));

    let mut selector_proxies = vec![Value::String(cfg.proxy.auto_group.clone())];
    selector_proxies.extend(proxy_name_values);

    let mut selector = Mapping::new();
    insert(&mut selector, "name", cfg.proxy.selector.clone());
    insert(&mut selector, "type", "select");
    insert(&mut selector, "proxies", Value::Sequence(selector_proxies));
    groups.push(Value::Mapping(selector));

    groups
}

fn build_tun(cfg: &AppConfig) -> Value {
    let mut tun = Mapping::new();
    insert(&mut tun, "enable", cfg.tun.enable);
    insert(&mut tun, "stack", cfg.tun.stack.clone());
    insert(&mut tun, "auto-route", cfg.tun.auto_route);
    insert(
        &mut tun,
        "auto-detect-interface",
        cfg.tun.auto_detect_interface,
    );
    insert(
        &mut tun,
        "dns-hijack",
        Value::Sequence(
            cfg.tun
                .dns_hijack
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    Value::Mapping(tun)
}

fn build_dns(cfg: &AppConfig) -> Value {
    let mut dns = Mapping::new();
    insert(&mut dns, "enable", cfg.dns.enable);
    insert(&mut dns, "listen", cfg.dns.listen.as_str());
    insert(&mut dns, "enhanced-mode", cfg.dns.enhanced_mode.clone());
    insert(&mut dns, "fake-ip-range", cfg.dns.fake_ip_range.clone());
    insert(
        &mut dns,
        "nameserver",
        Value::Sequence(
            cfg.dns
                .nameserver
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    insert(
        &mut dns,
        "fallback",
        Value::Sequence(
            cfg.dns
                .fallback
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    Value::Mapping(dns)
}

fn insert<T: Serialize>(map: &mut Mapping, key: &str, value: T) {
    map.insert(
        Value::String(key.to_string()),
        serde_yaml::to_value(value).expect("serializable value"),
    );
}

fn proxy_name(proxy: &Value) -> Result<String> {
    proxy
        .get("name")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("proxy item missing name"))
}

fn compile_patterns(patterns: &[String]) -> Result<Option<RegexSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    Ok(Some(RegexSet::new(patterns)?))
}

fn matches_filter(name: &str, include: Option<&RegexSet>, exclude: Option<&RegexSet>) -> bool {
    if let Some(include) = include
        && !include.is_match(name)
    {
        return false;
    }
    if let Some(exclude) = exclude
        && exclude.is_match(name)
    {
        return false;
    }
    true
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug)]
struct BestProxy {
    name: String,
    delay: u64,
}

async fn choose_and_switch(client: &ApiClient, cfg: &AppConfig) -> Result<BestProxy> {
    let proxies = client.get_proxies().await?;
    let selector = proxies
        .proxies
        .get(&cfg.proxy.selector)
        .ok_or_else(|| anyhow!("selector group not found: {}", cfg.proxy.selector))?;

    let candidates: Vec<String> = selector
        .all
        .iter()
        .filter(|name| {
            *name != &cfg.proxy.auto_group && name.as_str() != "DIRECT" && name.as_str() != "REJECT"
        })
        .cloned()
        .collect();

    if candidates.is_empty() {
        bail!(
            "selector {} does not contain node candidates",
            cfg.proxy.selector
        );
    }

    let mut best: Option<BestProxy> = None;
    for name in candidates {
        match client
            .delay(&name, &cfg.proxy.test_url, cfg.proxy.timeout_ms)
            .await
        {
            Ok(delay) if delay < cfg.proxy.timeout_ms => {
                debug!("{} delay {} ms", name, delay);
                if best.as_ref().is_none_or(|current| delay < current.delay) {
                    best = Some(BestProxy { name, delay });
                }
            }
            Ok(delay) => debug!("{} timeout-ish delay {} ms", name, delay),
            Err(err) => debug!("{} delay test failed: {err:#}", name),
        }
    }

    let best = best.ok_or_else(|| anyhow!("no reachable proxy found"))?;
    if selector.now.as_deref() != Some(best.name.as_str()) {
        client.select(&cfg.proxy.selector, &best.name).await?;
    }
    Ok(best)
}

struct ApiClient {
    base: String,
    secret: String,
    http: Client,
}

impl ApiClient {
    fn new(cfg: &AppConfig) -> Result<Self> {
        Ok(Self {
            base: format!("http://{}:{}", cfg.controller.host, cfg.controller.port),
            secret: cfg.controller.secret.clone(),
            http: Client::builder().timeout(Duration::from_secs(15)).build()?,
        })
    }

    async fn get_proxies(&self) -> Result<ProxiesResponse> {
        let resp = self
            .auth(self.http.get(format!("{}/proxies", self.base)))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("GET /proxies failed: HTTP {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    async fn delay(&self, name: &str, test_url: &str, timeout_ms: u64) -> Result<u64> {
        let url = format!(
            "{}/proxies/{}/delay?timeout={}&url={}",
            self.base,
            urlencoding::encode(name),
            timeout_ms,
            urlencoding::encode(test_url)
        );
        let resp = self.auth(self.http.get(url)).send().await?;
        if !resp.status().is_success() {
            bail!("delay API failed for {name}: HTTP {}", resp.status());
        }
        let delay: DelayResponse = resp.json().await?;
        Ok(delay.delay)
    }

    async fn select(&self, selector: &str, name: &str) -> Result<()> {
        let url = format!("{}/proxies/{}", self.base, urlencoding::encode(selector));
        let resp = self
            .auth(self.http.put(url))
            .json(&json!({ "name": name }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("select API failed: HTTP {}", resp.status());
        }
        info!("switched {} -> {}", selector, name);
        Ok(())
    }

    async fn reload_config(&self, runtime_config: &Path) -> Result<()> {
        let resp = self
            .auth(self.http.put(format!("{}/configs?force=true", self.base)))
            .json(&json!({ "path": runtime_config }))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("reload config failed: HTTP {}", resp.status());
        }
        Ok(())
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.secret.is_empty() {
            req
        } else {
            req.bearer_auth(&self.secret)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_subscription() -> &'static str {
        r#"
proxies:
  - name: HK-01
    type: ss
    server: example.com
    port: 443
    cipher: aes-128-gcm
    password: pass
  - name: 官网信息
    type: ss
    server: example.org
    port: 443
    cipher: aes-128-gcm
    password: pass
"#
    }

    fn sample_vmess_uri() -> String {
        let raw = serde_json::json!({
            "v": "2",
            "ps": "VM-01",
            "add": "vm.example.com",
            "port": "443",
            "id": "11111111-1111-1111-1111-111111111111",
            "aid": "0",
            "scy": "auto",
            "net": "ws",
            "type": "none",
            "host": "cdn.example.com",
            "path": "/ws",
            "tls": "tls",
            "sni": "sni.example.com"
        })
        .to_string();
        format!("vmess://{}", general_purpose::STANDARD.encode(raw))
    }

    #[test]
    fn parses_clash_yaml_subscription() {
        let value = parse_subscription(sample_subscription()).expect("subscription should parse");
        let proxies = read_proxy_items(&value).expect("proxies should exist");
        assert_eq!(proxies.len(), 2);
        assert_eq!(proxy_name(&proxies[0]).unwrap(), "HK-01");
    }

    #[test]
    fn filters_proxy_names_with_include_and_exclude() {
        let include = compile_patterns(&["HK|US".to_string()]).unwrap();
        let exclude = compile_patterns(&["官网|过期".to_string()]).unwrap();
        assert!(matches_filter("HK-01", include.as_ref(), exclude.as_ref()));
        assert!(!matches_filter("JP-01", include.as_ref(), exclude.as_ref()));
        assert!(!matches_filter(
            "HK-官网",
            include.as_ref(),
            exclude.as_ref()
        ));
    }

    #[test]
    fn builds_runtime_config_with_tun_dns_groups_and_rules() {
        let mut cfg = AppConfig::default();
        cfg.tun.enable = true;
        cfg.rules = vec![
            "DOMAIN-SUFFIX,github.com,PROXY".to_string(),
            "MATCH,DIRECT".to_string(),
        ];

        let value = parse_subscription(sample_subscription()).unwrap();
        let proxies = read_proxy_items(&value).unwrap();
        let names = proxies
            .iter()
            .map(proxy_name)
            .collect::<Result<Vec<_>>>()
            .unwrap();
        let runtime = build_runtime_config(&cfg, proxies, names).unwrap();

        assert_eq!(runtime["mixed-port"].as_i64(), Some(7890));
        assert_eq!(runtime["tun"]["enable"].as_bool(), Some(true));
        assert_eq!(runtime["dns"]["enhanced-mode"].as_str(), Some("fake-ip"));
        assert_eq!(runtime["proxy-groups"][0]["name"].as_str(), Some("AUTO"));
        assert_eq!(runtime["proxy-groups"][1]["name"].as_str(), Some("PROXY"));
        assert_eq!(runtime["rules"].as_sequence().unwrap().len(), 2);
    }

    #[test]
    fn parses_base64_uri_subscription() {
        let ss_main = general_purpose::STANDARD.encode("aes-128-gcm:pass@ss.example.com:8388");
        let lines = format!("ss://{}#SS-01\n{}", ss_main, sample_vmess_uri());
        let body = general_purpose::STANDARD.encode(lines);

        let value = parse_subscription(&body).expect("uri subscription should parse");
        let proxies = read_proxy_items(&value).expect("proxies should exist");

        assert_eq!(proxies.len(), 2);
        assert_eq!(proxies[0]["type"].as_str(), Some("ss"));
        assert_eq!(proxies[0]["name"].as_str(), Some("SS-01"));
        assert_eq!(proxies[1]["type"].as_str(), Some("vmess"));
        assert_eq!(
            proxies[1]["ws-opts"]["headers"]["Host"].as_str(),
            Some("cdn.example.com")
        );
    }
}
