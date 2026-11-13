// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "artidezi",
    version = artidezi_contract::app_version(),
    about = "The maintenance agent that pushes the boring buttons"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parses and validates a configuration file, printing every problem found
    ValidateConfig {
        /// Path to the YAML configuration file
        path: PathBuf,
    },
    /// Inspect declarative browser accounts or reset a local password
    UiUser {
        #[command(subcommand)]
        command: UiUserCommand,
    },
    /// Hash a password from a file for an initial UI account declaration
    UiHashPassword {
        #[arg(long)]
        password_file: PathBuf,
    },
    /// Prints the JSON Schema of a contract document
    Schema {
        #[arg(value_enum)]
        document: SchemaDocument,
    },
    /// Runs the orchestrator: worker API, admin API and maintenance loop
    Serve {
        /// Path to the YAML configuration file
        #[arg(long)]
        config: PathBuf,
        /// Directory with one file per credential_ref
        #[arg(long)]
        secrets_dir: PathBuf,
        /// Address to listen on (put a TLS reverse proxy in front for remote workers)
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: std::net::SocketAddr,
        /// Seconds a lease stays valid without a heartbeat
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(30..=86400))]
        lease_ttl_seconds: u64,
        /// Seconds between forge maintenance passes (lease recovery runs separately every 5 seconds)
        #[arg(long, default_value_t = 30)]
        tick_seconds: u64,
        /// Enables named-user web UI login; declare an administrator in ui_access first
        #[arg(long, env = "ARTIDEZI_UI_ENABLED")]
        ui_enabled: bool,
        /// Allows the UI session cookie over plain HTTP, for loopback-only development
        #[arg(long)]
        ui_insecure_cookie: bool,
    },
    /// Runs the worker supervisor loop against an orchestrator
    Worker {
        /// Path to the worker YAML configuration file
        #[arg(long)]
        config: PathBuf,
        /// Process one attempt (or none) and exit, instead of looping
        #[arg(long)]
        once: bool,
    },
    /// Inspects one GitHub PR and its reported CI without scheduling work or writing to GitHub
    GithubInspect {
        /// Exact owner/repository (GitHub.com)
        #[arg(long)]
        repository: String,
        #[arg(long)]
        pull_request: u64,
        /// Optional explicit read-only token or App installation token; no ambient GitHub credentials
        #[arg(long)]
        token_file: Option<PathBuf>,
    },
    /// Verifies one configured GitHub App installation and repository scope, without scheduling work
    GithubCheck {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        secrets_dir: PathBuf,
        #[arg(long)]
        forge: String,
    },
    /// Talks to a running orchestrator's admin API
    Task {
        #[command(subcommand)]
        command: TaskCommand,
        /// Orchestrator base URL
        #[arg(long, env = "ARTIDEZI_URL", default_value = "http://127.0.0.1:8080")]
        url: String,
        /// File containing the admin token
        #[arg(long, env = "ARTIDEZI_ADMIN_TOKEN_FILE")]
        token_file: PathBuf,
    },
}

#[derive(Subcommand)]
enum UiUserCommand {
    /// Reset an account password locally without changing its configured access
    ResetPassword {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        password_file: PathBuf,
    },
    /// List IDs, names and access without exposing password hashes
    List {
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Creates a manual task
    Create {
        #[arg(long)]
        project: String,
        /// Free-form instructions for the agent (untrusted text)
        #[arg(long)]
        instructions: String,
        #[arg(long)]
        agent_profile: Option<String>,
        /// merge_request | merge_request_with_automerge | direct_push_fast_forward
        #[arg(long)]
        delivery_mode: Option<String>,
        #[arg(long, default_value = "cli")]
        created_by: String,
    },
    /// Lists tasks, optionally filtered by state
    List {
        #[arg(long)]
        state: Option<String>,
    },
    /// Shows a task with its attempts, merge requests and events
    Show { task_id: String },
    /// Requests another attempt with human feedback
    Revise {
        task_id: String,
        #[arg(long)]
        feedback: String,
        #[arg(long, default_value = "cli")]
        actor: String,
    },
    /// Closes a task without delivery
    Close {
        task_id: String,
        #[arg(long, default_value = "cli")]
        actor: String,
    },
    /// Runs one maintenance pass now
    Tick,
    /// Previews configured artifact retention; --apply deletes the reported eligible files
    CleanupArtifacts {
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum SchemaDocument {
    Job,
    Result,
    Attempt,
    Config,
}

/// Worker configuration file. Kept separate from the orchestrator configuration: a worker knows
/// only its orchestrator, its token and its local paths.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerFile {
    orchestrator_url: String,
    /// Permits sending the token over plain HTTP to a non-loopback host.
    #[serde(default)]
    allow_insecure_orchestrator_url: bool,
    /// File containing the worker token.
    token_file: PathBuf,
    worker_pool: String,
    worker_id: String,
    /// Tags projects can require. They route work and are not a trust boundary.
    #[serde(default)]
    tags: Vec<String>,
    workspaces_root: PathBuf,
    #[serde(default)]
    credentials_root: Option<PathBuf>,
    #[serde(default = "default_poll_seconds")]
    poll_seconds: u64,
    #[serde(default = "default_max_log_bytes")]
    max_log_bytes: u64,
    /// Removes what the worker's user created in `/tmp` during an attempt, once it ends. Only for a
    /// `/tmp` that no other process of the same user uses.
    #[serde(default)]
    sweep_system_tmp: bool,
    #[serde(default)]
    codex_binary: Option<PathBuf>,
    #[serde(default)]
    pi_binary: Option<PathBuf>,
    #[serde(default)]
    fake_script: Option<PathBuf>,
}

fn default_poll_seconds() -> u64 {
    15
}

fn default_max_log_bytes() -> u64 {
    16 * 1024 * 1024
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::ValidateConfig { path } => {
            let config = artidezi_config::load(&path)?;
            println!(
                "configuration is valid: instance {:?}, {} project(s), {} agent profile(s)",
                config.instance.name,
                config.projects.len(),
                config.agent_profiles.len()
            );
        }
        Command::UiUser { command } => ui_user(command)?,
        Command::UiHashPassword { password_file } => {
            let password = std::fs::read_to_string(&password_file)
                .with_context(|| format!("reading {}", password_file.display()))?;
            println!(
                "{}",
                artidezi_server::ui::UiConfig::hash_password(
                    password.trim_end_matches(['\r', '\n'])
                )
                .map_err(anyhow::Error::msg)?
            );
        }
        Command::Schema { document } => {
            let schema = match document {
                SchemaDocument::Job => artidezi_contract::schema::job_schema(),
                SchemaDocument::Result => artidezi_contract::schema::result_schema(),
                SchemaDocument::Attempt => artidezi_contract::schema::attempt_schema(),
                SchemaDocument::Config => schemars::schema_for!(artidezi_config::Config),
            };
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
        Command::Serve {
            config,
            secrets_dir,
            listen,
            lease_ttl_seconds,
            tick_seconds,
            ui_enabled,
            ui_insecure_cookie,
        } => serve(
            config,
            secrets_dir,
            listen,
            lease_ttl_seconds,
            tick_seconds,
            UiServeOptions {
                enabled: ui_enabled,
                insecure_cookie: ui_insecure_cookie,
            },
        )?,
        Command::Worker { config, once } => worker(config, once)?,
        Command::GithubInspect {
            repository,
            pull_request,
            token_file,
        } => {
            let token = token_file
                .map(|path| -> anyhow::Result<String> {
                    let token = std::fs::read_to_string(&path)
                        .context("reading GitHub inspection credential")?;
                    let token = token.trim();
                    anyhow::ensure!(
                        !token.is_empty() && !token.chars().any(char::is_whitespace),
                        "GitHub credential must be a single nonempty token"
                    );
                    Ok(token.to_owned())
                })
                .transpose()?;
            let report = artidezi_forge::github::GithubReader::new(token)
                .inspect(&repository, pull_request)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::GithubCheck {
            config,
            secrets_dir,
            forge,
        } => {
            let config = artidezi_config::load(&config)?;
            let repositories = artidezi_orchestrator::forges::verify_github_binding(
                &config,
                &artidezi_orchestrator::secrets::SecretsDir::new(secrets_dir),
                &forge,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "forge": forge, "repositories": repositories, "repository_access": "verified_configured_scope",
                    "tasks_created": 0, "repository_writes": false
                }))?
            );
        }
        Command::Task {
            command,
            url,
            token_file,
        } => task(command, &url, &token_file)?,
    }
    Ok(())
}

struct UiServeOptions {
    enabled: bool,
    insecure_cookie: bool,
}

fn serve(
    config_path: PathBuf,
    secrets_dir: PathBuf,
    listen: std::net::SocketAddr,
    lease_ttl_seconds: u64,
    tick_seconds: u64,
    ui_options: UiServeOptions,
) -> anyhow::Result<()> {
    use artidezi_orchestrator::Orchestrator;
    use artidezi_orchestrator::forges::build_forges;
    use artidezi_orchestrator::secrets::SecretsDir;

    let config = artidezi_config::load(&config_path)?;
    let secrets = SecretsDir::new(&secrets_dir);
    let data_dir = PathBuf::from(&config.instance.data_dir);
    let forges = build_forges(&config, &secrets, &data_dir.join("fake-forges"))?;
    let mut tokens = artidezi_server::Tokens {
        workers: Vec::new(),
        admin: secrets
            .get(&config.instance.admin_token_ref)
            .context("admin token")?,
    };
    for (pool_name, pool) in &config.worker_pools {
        for (id, worker) in &pool.workers {
            let token = secrets
                .get(&worker.token_ref)
                .with_context(|| format!("worker {pool_name}/{id} token"))?;
            anyhow::ensure!(
                token != tokens.admin && tokens.workers.iter().all(|other| other.token != token),
                "worker {pool_name}/{id} shares its token with the admin API or another worker"
            );
            tokens.workers.push(artidezi_server::WorkerToken {
                worker_pool: pool_name.clone(),
                worker_id: id.clone(),
                token,
            });
        }
    }
    let UiServeOptions {
        enabled,
        insecure_cookie,
    } = ui_options;
    if insecure_cookie && !enabled {
        anyhow::bail!("--ui-insecure-cookie requires --ui-enabled");
    }
    let ui = enabled.then(|| artidezi_server::ui::UiConfig::new(!insecure_cookie));
    let mut orchestrator = Orchestrator::open(config, &data_dir, forges)?;
    if ui.is_some() {
        use artidezi_config::model::UiProjectGrant;
        use artidezi_store::db::{UiAccountDeclaration, UiGrant};
        let declared = orchestrator
            .config
            .ui_access
            .as_ref()
            .map_or_else(Vec::new, |ui| {
                ui.accounts
                    .iter()
                    .map(|(id, account)| UiAccountDeclaration {
                        id: id.clone(),
                        display_name: account.display_name.clone(),
                        initial_password_hash: account.initial_password_hash.clone(),
                        disabled: account.disabled,
                        instance_admin: account.instance_admin,
                        grants: account
                            .grants
                            .iter()
                            .map(|(project, grant)| {
                                (
                                    project.clone(),
                                    match grant {
                                        UiProjectGrant::Read => UiGrant::Read,
                                        UiProjectGrant::Write => UiGrant::Write,
                                    },
                                )
                            })
                            .collect(),
                    })
                    .collect()
            });
        anyhow::ensure!(
            orchestrator.config.ui_access.is_some(),
            "--ui-enabled requires a ui_access account declaration"
        );
        orchestrator.db.reconcile_ui_accounts(&declared)?;
        let users = orchestrator.db.list_ui_users()?;
        anyhow::ensure!(
            users
                .iter()
                .any(|user| user.instance_admin && !user.disabled),
            "--ui-enabled requires an enabled instance administrator in ui_access"
        );
        for user in users.iter().filter(|user| !user.disabled) {
            artidezi_server::ui::UiConfig::validate_user(user, &orchestrator.config)
                .map_err(anyhow::Error::msg)?;
        }
    }
    let server_config = artidezi_server::ServerConfig {
        listen,
        lease_ttl: Duration::from_secs(lease_ttl_seconds),
        tick_interval: Duration::from_secs(tick_seconds),
        ui,
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(artidezi_server::serve(orchestrator, tokens, server_config))?;
    Ok(())
}

fn ui_user(command: UiUserCommand) -> anyhow::Result<()> {
    use artidezi_store::Db;
    use artidezi_store::db::UiGrant;

    let path = match &command {
        UiUserCommand::ResetPassword { config, .. } | UiUserCommand::List { config } => config,
    };
    let config = artidezi_config::load(path)?;
    let data_dir = PathBuf::from(&config.instance.data_dir);
    let db_path = data_dir.join("artidezi.sqlite");
    anyhow::ensure!(
        db_path.exists(),
        "UI database does not exist; start the UI with ui_access first"
    );
    let mut db = Db::open(&db_path)?;
    match command {
        UiUserCommand::ResetPassword {
            id, password_file, ..
        } => {
            let mut user = db
                .get_ui_user(&id)?
                .with_context(|| format!("UI account {id} does not exist"))?;
            let password = std::fs::read_to_string(&password_file)
                .with_context(|| format!("reading {}", password_file.display()))?;
            user.password_hash = artidezi_server::ui::UiConfig::hash_password(
                password.trim_end_matches(['\r', '\n']),
            )
            .map_err(anyhow::Error::msg)?;
            user.password_change_required = true;
            anyhow::ensure!(
                db.update_ui_user(&user)?,
                "UI account changed concurrently; retry"
            );
            println!("Reset UI password for {id}; existing sessions were revoked");
        }
        UiUserCommand::List { .. } => {
            for user in db.list_ui_users()? {
                let grants = user
                    .grants
                    .iter()
                    .map(|(project, level)| {
                        format!(
                            "{project}={}",
                            match level {
                                UiGrant::Read => "read",
                                UiGrant::Write => "write",
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{}\t{}\tadmin={}\tdisabled={}\tinitial_password={}\tmanaged_by_config={}\t{}",
                    user.id,
                    user.display_name,
                    user.instance_admin,
                    user.disabled,
                    user.password_change_required,
                    user.managed_by_config,
                    grants
                );
            }
        }
    }
    Ok(())
}

fn worker(config_path: PathBuf, once: bool) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let file: WorkerFile =
        serde_saphyr::from_str_with_options(&text, artidezi_config::yaml_options())?;
    if let Some(tag) = file
        .tags
        .iter()
        .find(|tag| !artidezi_contract::validate::is_plain_id(tag))
    {
        anyhow::bail!("invalid tag {tag:?}");
    }
    let token = std::fs::read_to_string(&file.token_file)
        .with_context(|| format!("reading {}", file.token_file.display()))?
        .trim()
        .to_owned();
    let daemon = artidezi_worker::Daemon::new(artidezi_worker::DaemonConfig {
        orchestrator_url: file.orchestrator_url,
        allow_insecure_orchestrator_url: file.allow_insecure_orchestrator_url,
        token,
        worker: artidezi_worker::WorkerConfig {
            worker_pool: file.worker_pool,
            worker_id: file.worker_id,
            workspaces_root: file.workspaces_root,
            credentials_root: file.credentials_root,
            max_log_bytes: file.max_log_bytes,
            system_tmp: file.sweep_system_tmp.then(|| PathBuf::from("/tmp")),
        },
        poll_interval: Duration::from_secs(file.poll_seconds),
        tags: file.tags,
        codex_binary: file.codex_binary,
        pi_binary: file.pi_binary,
        fake_script: file.fake_script,
    })?;
    if once {
        let had_work = daemon.run_once()?;
        println!(
            "{}",
            if had_work {
                "processed one attempt"
            } else {
                "no work"
            }
        );
        Ok(())
    } else {
        Ok(daemon.run()?)
    }
}

fn task(command: TaskCommand, url: &str, token_file: &PathBuf) -> anyhow::Result<()> {
    let token = std::fs::read_to_string(token_file)
        .with_context(|| format!("reading {}", token_file.display()))?
        .trim()
        .to_owned();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let call = |method: &str,
                path: &str,
                body: Option<serde_json::Value>|
     -> anyhow::Result<serde_json::Value> {
        let full = format!("{}{path}", url.trim_end_matches('/'));
        let auth = format!("Bearer {token}");
        let mut response = match (method, body) {
            ("GET", _) => agent.get(&full).header("Authorization", &auth).call()?,
            (_, Some(b)) => agent
                .post(&full)
                .header("Authorization", &auth)
                .send_json(b)?,
            (_, None) => agent
                .post(&full)
                .header("Authorization", &auth)
                .send_empty()?,
        };
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string()?;
        let value: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        anyhow::ensure!((200..300).contains(&status), "HTTP {status}: {value}");
        Ok(value)
    };
    let value = match command {
        TaskCommand::Create {
            project,
            instructions,
            agent_profile,
            delivery_mode,
            created_by,
        } => call(
            "POST",
            "/admin/v1/tasks",
            Some(serde_json::json!({
                "project": project,
                "instructions": instructions,
                "agent_profile": agent_profile,
                "delivery_mode": delivery_mode,
                "created_by": created_by,
            })),
        )?,
        TaskCommand::List { state } => {
            let path = match state {
                Some(s) => format!("/admin/v1/tasks?state={s}"),
                None => "/admin/v1/tasks".into(),
            };
            call("GET", &path, None)?
        }
        TaskCommand::Show { task_id } => call("GET", &format!("/admin/v1/tasks/{task_id}"), None)?,
        TaskCommand::Revise {
            task_id,
            feedback,
            actor,
        } => call(
            "POST",
            &format!("/admin/v1/tasks/{task_id}/revisions"),
            Some(serde_json::json!({"feedback": feedback, "actor": actor})),
        )?,
        TaskCommand::Close { task_id, actor } => call(
            "POST",
            &format!("/admin/v1/tasks/{task_id}/close"),
            Some(serde_json::json!({"actor": actor})),
        )?,
        TaskCommand::Tick => call("POST", "/admin/v1/tick", None)?,
        TaskCommand::CleanupArtifacts { apply } => call(
            "POST",
            "/admin/v1/artifacts/cleanup",
            Some(serde_json::json!({"apply": apply})),
        )?,
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
