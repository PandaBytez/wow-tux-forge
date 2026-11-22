use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct RepoSummary {
    pub full_name: String,
    pub stargazers_count: u64,
    pub description: Option<String>,
    pub pushed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    items: Vec<RepoSummary>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
    pub download_count: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone)]
pub struct ReleaseOption {
    pub tag_name: String,
    pub asset: ReleaseAsset,
}

fn resolve_github_token() -> Option<String> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let trimmed = token.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    static GH_CLI_TOKEN: OnceLock<Option<String>> = OnceLock::new();
    GH_CLI_TOKEN
        .get_or_init(|| {
            let out = std::process::Command::new("gh")
                .args(["auth", "token"])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!tok.is_empty()).then_some(tok)
        })
        .clone()
}

pub fn make_client() -> Result<Client, String> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static("wow-tux-forge/0.1"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    if let Some(token) = resolve_github_token()
        && let Ok(val) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(AUTHORIZATION, val);
    }

    Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(15))
        .timeout(None)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn parse_owner_repo(input: &str) -> Option<String> {
    let trimmed = input.trim().trim_end_matches('/');
    let path = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .unwrap_or(trimmed);

    if path.contains(' ') {
        return None;
    }
    let mut parts = path.split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?;
    let raw_repo = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let repo = raw_repo.strip_suffix(".git").unwrap_or(raw_repo);
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn run_search(client: &Client, raw_query: &str) -> Result<Vec<RepoSummary>, String> {
    let q = percent_encode(raw_query);
    let url = format!("https://api.github.com/search/repositories?q={q}&per_page=25");
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("Search failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("GitHub search returned {}", resp.status()));
    }

    let parsed: SearchResponse = resp.json().map_err(|e| format!("Invalid JSON: {e}"))?;
    Ok(parsed.items)
}

pub fn search_repos(client: &Client, query: &str) -> Result<Vec<RepoSummary>, String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    // Direct owner/repo or https://github.com/owner/repo lookup.
    if let Some(slug) = parse_owner_repo(trimmed) {
        let url = format!("https://api.github.com/repos/{slug}");
        let resp = client
            .get(&url)
            .send()
            .map_err(|e| format!("Request failed: {e}"))?;

        if resp.status().is_success() {
            let repo: RepoSummary = resp.json().map_err(|e| format!("Invalid JSON: {e}"))?;
            return Ok(vec![repo]);
        }
    }

    // Filter to Lua repos first (WoW addons are Lua) without requiring literal "wow addon" words.
    // Include forks (`fork:true`) since many maintained WoW addons are community/personal forks.
    let lua_results = run_search(client, &format!("{trimmed} language:Lua fork:true"))?;
    if !lua_results.is_empty() {
        return Ok(lua_results);
    }

    run_search(client, &format!("{trimmed} fork:true"))
}

#[derive(Debug, Deserialize)]
struct TagItem {
    name: String,
}

#[derive(Debug, Deserialize)]
struct RepoMeta {
    default_branch: String,
}

#[derive(Debug, Deserialize)]
struct CommitItem {
    sha: String,
}

fn default_branch_release(client: &Client, repo: &str) -> Result<Release, String> {
    let repo_name = repo.split('/').nth(1).unwrap_or(repo);
    let meta_url = format!("https://api.github.com/repos/{repo}");
    let meta_resp = client
        .get(&meta_url)
        .send()
        .map_err(|e| format!("Failed fetching repo info for {repo}: {e}"))?;
    if !meta_resp.status().is_success() {
        if meta_resp.status() == reqwest::StatusCode::FORBIDDEN {
            return Err("403 rate limited (set GITHUB_TOKEN or login with gh)".to_string());
        }
        return Err(format!("GitHub returned {} for {repo}", meta_resp.status()));
    }
    let meta: RepoMeta = meta_resp
        .json()
        .map_err(|e| format!("Invalid repo JSON: {e}"))?;

    let commits_url = format!(
        "https://api.github.com/repos/{repo}/commits?sha={}&per_page=1",
        percent_encode(&meta.default_branch)
    );
    let short_sha = client
        .get(&commits_url)
        .send()
        .ok()
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json::<Vec<CommitItem>>().ok())
        .and_then(|c| c.into_iter().next())
        .map(|c| c.sha.chars().take(7).collect::<String>())
        .unwrap_or_else(|| "head".to_string());

    Ok(Release {
        tag_name: format!("{}@{short_sha}", meta.default_branch),
        draft: false,
        assets: vec![ReleaseAsset {
            name: format!("{repo_name}-{}.zip", meta.default_branch),
            browser_download_url: format!(
                "https://api.github.com/repos/{repo}/zipball/{}",
                percent_encode(&meta.default_branch)
            ),
            size: 0,
            download_count: 0,
        }],
    })
}

fn fetch_tags_list(client: &Client, repo: &str, per_page: usize) -> Vec<Release> {
    let repo_name = repo.split('/').nth(1).unwrap_or(repo);
    let tags_url = format!("https://api.github.com/repos/{repo}/tags?per_page={per_page}");
    let Ok(resp) = client.get(&tags_url).send() else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(tags) = resp.json::<Vec<TagItem>>() else {
        return Vec::new();
    };
    tags.into_iter()
        .map(|tag| Release {
            tag_name: tag.name.clone(),
            draft: false,
            assets: vec![ReleaseAsset {
                name: format!("{repo_name}-{}.zip", tag.name),
                browser_download_url: format!(
                    "https://api.github.com/repos/{repo}/zipball/{}",
                    percent_encode(&tag.name)
                ),
                size: 0,
                download_count: 0,
            }],
        })
        .collect()
}

fn fallback_archive_release(client: &Client, repo: &str) -> Result<Release, String> {
    // Release -> tag -> main/master order.
    if let Some(tag_rel) = fetch_tags_list(client, repo, 1).into_iter().next() {
        return Ok(tag_rel);
    }
    default_branch_release(client, repo)
}

fn fetch_releases_list(client: &Client, repo: &str) -> Result<Vec<Release>, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=50");
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("Release list failed for {repo}: {e}"))?;

    if !resp.status().is_success() {
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            return Err("403 rate limited (set GITHUB_TOKEN or login with gh)".to_string());
        }
        return Err(format!("GitHub returned {} for {repo}", resp.status()));
    }

    let mut releases: Vec<Release> = resp.json().map_err(|e| format!("Invalid JSON: {e}"))?;
    for r in &mut releases {
        r.assets
            .retain(|a| a.name.to_ascii_lowercase().ends_with(".zip"));
    }
    releases.retain(|r| !r.draft && !r.assets.is_empty());
    Ok(releases)
}

pub fn group_install_options(releases: Vec<Release>) -> Vec<ReleaseOption> {
    releases
        .into_iter()
        .flat_map(|r| {
            let tag = r.tag_name;
            r.assets.into_iter().map(move |asset| ReleaseOption {
                tag_name: tag.clone(),
                asset,
            })
        })
        .collect()
}

pub fn fetch_install_options(client: &Client, repo: &str) -> Result<Vec<ReleaseOption>, String> {
    if let Ok(releases) = fetch_releases_list(client, repo)
        && !releases.is_empty()
    {
        let options = group_install_options(releases);
        if !options.is_empty() {
            return Ok(options);
        }
    }

    // No GitHub Releases: offer both tags and default branch (main/master) in the picker.
    let mut options = Vec::new();
    for rel in fetch_tags_list(client, repo, 10) {
        for asset in rel.assets {
            options.push(ReleaseOption {
                tag_name: rel.tag_name.clone(),
                asset,
            });
        }
    }
    if let Ok(branch_rel) = default_branch_release(client, repo) {
        for asset in branch_rel.assets {
            options.push(ReleaseOption {
                tag_name: branch_rel.tag_name.clone(),
                asset,
            });
        }
    }
    if !options.is_empty() {
        return Ok(options);
    }

    let rel = latest_release(client, repo)?;
    Ok(rel
        .assets
        .into_iter()
        .map(|asset| ReleaseOption {
            tag_name: rel.tag_name.clone(),
            asset,
        })
        .collect())
}

pub fn latest_release_for_key(
    client: &Client,
    state_key: &str,
    current_tag: &str,
) -> Result<Release, String> {
    if let Some((repo, prefix)) = state_key.split_once('#') {
        let releases = fetch_releases_list(client, repo)?;
        let prefix_slash = format!("{prefix}/");
        return releases
            .into_iter()
            .find(|r| r.tag_name.starts_with(&prefix_slash))
            .ok_or_else(|| format!("No release found for {state_key}"));
    }
    if current_tag.contains('@') {
        return default_branch_release(client, state_key);
    }
    latest_release(client, state_key)
}

pub fn latest_release(client: &Client, repo: &str) -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| format!("Release check failed for {repo}: {e}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return fallback_archive_release(client, repo);
    }
    if !resp.status().is_success() {
        if resp.status() == reqwest::StatusCode::FORBIDDEN {
            return Err("403 rate limited (set GITHUB_TOKEN or login with gh)".to_string());
        }
        return Err(format!("GitHub returned {} for {repo}", resp.status()));
    }

    let mut release: Release = resp.json().map_err(|e| format!("Invalid JSON: {e}"))?;
    release
        .assets
        .retain(|a| a.name.to_ascii_lowercase().ends_with(".zip"));

    if release.assets.is_empty() {
        return fallback_archive_release(client, repo);
    }
    Ok(release)
}

pub fn download_to_file(client: &Client, url: &str, dest: &std::path::Path) -> Result<(), String> {
    let mut last_err = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        let mut resp = match client
            .get(url)
            .send()
        {
            Ok(r) => r,
            Err(e) => {
                last_err = format!("Download failed: {e}");
                continue;
            }
        };

        if !resp.status().is_success() {
            return Err(format!("Download returned {}", resp.status()));
        }

        let mut out = std::fs::File::create(dest)
            .map_err(|e| format!("Failed creating temp file {}: {e}", dest.display()))?;
        match std::io::copy(&mut resp, &mut out) {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = format!("Failed reading download body: {e}");
                let _ = std::fs::remove_file(dest);
            }
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::{Release, ReleaseAsset, group_install_options, parse_owner_repo};

    fn asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_string(),
            browser_download_url: format!("https://example.com/{name}"),
            size: 100,
            download_count: 1,
        }
    }

    #[test]
    fn parses_slug_and_urls() {
        assert_eq!(
            parse_owner_repo("https://github.com/Wuild/GatherLite").as_deref(),
            Some("Wuild/GatherLite")
        );
        assert_eq!(
            parse_owner_repo("Wuild/GatherLite.git").as_deref(),
            Some("Wuild/GatherLite")
        );
        assert_eq!(parse_owner_repo("GatherLite"), None);
    }

    #[test]
    fn groups_prefixed_and_standard_releases() {
        let prefixed = vec![
            Release {
                tag_name: "spoken/v3.11.0".to_string(),
                draft: false,
                assets: vec![asset("spoken.zip")],
            },
            Release {
                tag_name: "spoken/v3.10.0".to_string(),
                draft: false,
                assets: vec![asset("spoken-old.zip")],
            },
            Release {
                tag_name: "quests/v2.5.0".to_string(),
                draft: false,
                assets: vec![asset("quests.zip")],
            },
        ];
        let opts = group_install_options(prefixed);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].tag_name, "spoken/v3.11.0");
        assert_eq!(opts[1].tag_name, "spoken/v3.10.0");
        assert_eq!(opts[2].tag_name, "quests/v2.5.0");

        let standard = vec![
            Release {
                tag_name: "v2.0".to_string(),
                draft: false,
                assets: vec![asset("a.zip"), asset("b.zip")],
            },
            Release {
                tag_name: "v1.0".to_string(),
                draft: false,
                assets: vec![asset("old.zip")],
            },
        ];
        let opts2 = group_install_options(standard);
        assert_eq!(opts2.len(), 3);
        assert_eq!(opts2[0].tag_name, "v2.0");
        assert_eq!(opts2[2].tag_name, "v1.0");
    }
}
