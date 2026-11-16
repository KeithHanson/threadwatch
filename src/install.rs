use anyhow::{Context, Result, bail, ensure};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

const MARKER: &str = "# Managed by Threadwatch";

fn command(program: &str, args: &[&str]) -> Result<()> {
    ensure!(
        Command::new(program).args(args).status()?.success(),
        "{program} failed; inspect the service status"
    );
    Ok(())
}

fn uid(user: Option<&str>) -> Result<u32> {
    let mut cmd = Command::new("id");
    cmd.arg("-u");
    if let Some(user) = user {
        cmd.arg(user);
    }
    let result = cmd.output().context("Could not resolve service account")?;
    ensure!(result.status.success(), "Service account does not exist");
    Ok(std::str::from_utf8(&result.stdout)?.trim().parse()?)
}

fn quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}

pub fn install(environment: &Path, user: &str) -> Result<()> {
    ensure!(uid(None)? == 0, "Installing system services requires sudo");
    ensure!(
        !user.is_empty()
            && user
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
        "Invalid service user"
    );
    let service_uid = uid(Some(user))?;
    ensure!(
        service_uid != 0,
        "Use an unprivileged service account, not root"
    );
    ensure!(
        environment.is_absolute(),
        "Environment file path must be absolute"
    );
    let env = fs::canonicalize(environment).context("Environment file is unavailable")?;
    let metadata = fs::metadata(&env)?;
    ensure!(
        metadata.is_file() && metadata.mode() & 0o077 == 0 && metadata.uid() == service_uid,
        "Environment file must be private and owned by the service user"
    );
    let executable = std::env::current_exe()?;
    let exe = executable
        .to_str()
        .context("Executable path must be UTF-8")?;
    let env_text = env.to_str().context("Environment path must be UTF-8")?;
    ensure!(
        !exe.chars().chain(env_text.chars()).any(char::is_control),
        "Service paths must not contain control characters"
    );
    let workdir = env
        .parent()
        .context("Environment file has no parent")?
        .to_str()
        .context("Invalid environment directory")?;
    let (name, action, description) = ("threadwatch.service", "serve", "server");
    let target = Path::new("/etc/systemd/system").join(name);
    if target.exists() && !fs::read_to_string(&target)?.starts_with(MARKER) {
        bail!("Refusing to replace a service not managed by Threadwatch");
    }
    let unit = format!(
        "{MARKER}\n[Unit]\nDescription=Threadwatch {description}\nWants=network-online.target\nAfter=network-online.target\n\n[Service]\nType=simple\nUser={user}\nWorkingDirectory={}\nExecStart={} --env-file {} {action}\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=30\nUMask=0077\nNoNewPrivileges=true\nPrivateTmp=true\n\n[Install]\nWantedBy=multi-user.target\n",
        workdir.replace('%', "%%"),
        quote(exe),
        quote(env_text)
    );
    let staged = target.with_extension(format!("stage-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&staged)?;
    file.write_all(unit.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o644))?;
    fs::rename(&staged, &target)?;
    command(
        "systemd-analyze",
        &["verify", target.to_str().context("Invalid unit path")?],
    )?;
    command("systemctl", &["daemon-reload"])?;
    command("systemctl", &["enable", name])?;
    command("systemctl", &["restart", name])?;
    println!("Installed and enabled {name}");
    Ok(())
}
