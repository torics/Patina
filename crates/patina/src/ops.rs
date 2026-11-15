//! `patina export` and `patina version`.
use anyhow::{Context, Result, bail};
use patina_core::Paths;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// Where `patina version --check` asks for the latest release, unless `PATINA_RELEASE_URL` says otherwise. A
/// build can name another feed with `PATINA_RELEASE_API` at compile time.
const RELEASE_API: &str = match option_env!("PATINA_RELEASE_API") {
    Some(url) => url,
    None => "https://api.github.com/repos/torics/Patina/releases/latest",
};

/// Write one run as Markdown or JSON, to `output` or to stdout. `id` may be the whole run id or its last
/// characters, as the UI and export file names show it, as long as only one run matches.
pub async fn export(paths: &Paths, id: &str, format: &str, output: Option<&Path>) -> Result<()> {
    if !paths.database_file().exists() {
        bail!("no Patina data in {}; pass --data-dir", paths.data_dir.display());
    }
    let cfg = match std::fs::read_to_string(paths.config_file()) {
        Ok(text) => patina_core::Config::parse(&text)?,
        Err(_) => patina_core::Config::default(),
    };
    let db = patina_store::Db::open(&paths.database_file())?;
    let key = patina_store::secrets::master_key(&paths.data_dir)?;
    let secrets = patina_store::secrets::Secrets::new(db.clone(), key);
    let id = resolve_run(&db, id).await?;
    let st = patina_server::state::AppState::new(paths.clone(), cfg, db, secrets, patina_server::genai_factory());
    let exported = patina_server::files::export_run(&st, &id, format)
        .await
        .map_err(|e| anyhow::anyhow!(e.message))?;
    match output {
        Some(path) => {
            std::fs::write(path, &exported.body).with_context(|| format!("cannot write {}", path.display()))?;
            eprintln!("Wrote run {id} to {}", path.display());
        }
        None => std::io::stdout().lock().write_all(exported.body.as_bytes())?,
    }
    Ok(())
}

async fn resolve_run(db: &patina_store::Db, id: &str) -> Result<String> {
    if patina_store::runs::get_run(db, id).await?.is_some() {
        return Ok(id.to_string());
    }
    if id.len() < 4 {
        bail!("no run {id:?}; give at least four characters of its id");
    }
    let runs = patina_store::runs::list_runs(db, i64::MAX).await?;
    let found: Vec<&str> = runs.iter().map(|r| r.id.as_str()).filter(|r| r.ends_with(id)).collect();
    match found.as_slice() {
        [one] => Ok(one.to_string()),
        [] => bail!("no run {id:?}"),
        many => bail!("{} runs end with {id:?}; give more of the id", many.len()),
    }
}

/// The version and what this binary was built for.
pub fn version_text() -> String {
    format!(
        "patina {}\ncommit {}\ntarget {}-{}",
        env!("CARGO_PKG_VERSION"),
        option_env!("PATINA_COMMIT").unwrap_or("unknown"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Ask the release feed for the latest version and say whether this one is behind. This is the only call
/// Patina makes to a place the owner did not configure, and only when the owner runs this command.
pub async fn check_release() -> Result<String> {
    let url = std::env::var("PATINA_RELEASE_URL").unwrap_or_else(|_| RELEASE_API.to_string());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(concat!("patina/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("cannot reach the release feed {url}"))?;
    if !resp.status().is_success() {
        bail!(
            "the release feed {url} answered {}; a private repository needs a signed-in check on its releases page",
            resp.status()
        );
    }
    let body: serde_json::Value = resp.json().await.context("the release feed did not answer with JSON")?;
    let tag = body["tag_name"].as_str().context("the release feed named no tag")?;
    let page = body["html_url"].as_str().unwrap_or("");
    let ours = env!("CARGO_PKG_VERSION");
    Ok(match (parse(tag), parse(ours)) {
        (Some(latest), Some(mine)) if latest > mine => format!("A newer release is available: {tag} (this is {ours}). {page}"),
        (Some(_), Some(_)) => format!("This is the latest release ({ours})."),
        _ => format!("The latest release is {tag}; this is {ours}."),
    })
}

/// `v1.2.3` or `1.2.3`, with any pre-release part ignored.
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
        parts.next().flatten().unwrap_or(0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert_eq!(parse("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse("0.10.0-rc.1"), Some((0, 10, 0)));
        assert_eq!(parse("2"), Some((2, 0, 0)));
        assert_eq!(parse("nightly"), None);
        assert!(parse("v0.10.0") > parse("0.9.9"));
    }
}
