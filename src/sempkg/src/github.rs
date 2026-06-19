/// GitHub source resolution, archive download, and release-asset detection.
///
/// This module handles:
///   - Parsing GitHub URL / shorthand specs into a [`GitHubSource`]
///   - Resolving a ref (tag / branch / SHA) to a full commit SHA via the GitHub API
///   - Checking whether an existing `.sembundle` release asset is available
///   - Downloading and extracting the repo tarball to a temp directory
///   - Simple language detection for the `BuildOptions.language` field
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A parsed GitHub source reference (not yet resolved to a commit SHA).
#[derive(Debug, Clone)]
pub struct GitHubSource {
    /// GitHub host (e.g. `github.com`, `github.company.com`).
    pub host: String,
    pub owner: String,
    pub repo: String,
    /// Tag / branch / SHA as supplied. `None` means "resolve default branch".
    pub git_ref: Option<String>,
    /// Optional repo-relative subdirectory for monorepo scoping (from `#subdir` suffix).
    pub subdir: Option<String>,
}

/// A fully resolved GitHub reference, ready for download / manifest population.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    /// GitHub host (e.g. `github.com`, `github.company.com`).
    pub host: String,
    pub owner: String,
    pub repo: String,
    /// Concrete tag/branch/sha used for the archive URL.
    pub git_ref: String,
    /// Full 40-character lowercase commit SHA.
    pub commit_sha: String,
    /// True when `git_ref` is a tag (affects release-asset lookup).
    pub is_tag: bool,
    /// Sanitized repo name obeying sembundle name rules.
    pub package_name: String,
    /// Version string: ref with a leading `v` stripped, or first 12 chars of SHA.
    pub version: String,
    /// `https://github.com/{owner}/{repo}`
    pub source_repo_url: String,
}

/// A `.sembundle` release asset found on a GitHub release.
#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub bundle_url: String,
    pub sig_url: Option<String>,
}

// ---------------------------------------------------------------------------
// HTTP client with optional auth
// ---------------------------------------------------------------------------

/// A thin wrapper around `reqwest::blocking::Client` that injects
/// `Authorization: token <token>` when a token is configured.
pub struct GhClient {
    inner: reqwest::blocking::Client,
    token: Option<String>,
}

impl GhClient {
    pub fn new(token: Option<&str>) -> Result<Self> {
        let inner = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(300))
            .user_agent("sempkg/0.1 (https://github.com/willem445/codegraph-hub)")
            .build()
            .context("Failed to build HTTP client")?;
        Ok(Self {
            inner,
            token: token.map(str::to_owned),
        })
    }

    pub fn get(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        let mut rb = self.inner.get(url);
        if let Some(tok) = &self.token {
            // GitHub Enterprise commonly expects PAT auth as `token <PAT>`.
            rb = rb.header("Authorization", format!("token {tok}"));
        }
        rb
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Try to parse `spec` as a GitHub source.
///
/// Returns `None` when the spec is clearly not a GitHub reference (e.g. a bare
/// `name@version` without slashes) so the caller can fall back to the existing
/// registry/URL path.
///
/// Accepted forms:
/// - `owner/repo`
/// - `owner/repo@ref`
/// - `owner/repo@ref#subdir`
/// - `https://github.com/owner/repo`
/// - `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo@ref`
/// - `https://github.com/owner/repo/tree/<ref>`
/// - `https://github.com/owner/repo/releases/tag/<tag>`
pub fn parse_source(spec: &str) -> Option<GitHubSource> {
    let s = spec.trim();

    if s.starts_with("https://") || s.starts_with("http://") {
        parse_github_url(s)
    } else if looks_like_owner_repo(s) {
        parse_shorthand(s)
    } else {
        None
    }
}

fn parse_github_url(url: &str) -> Option<GitHubSource> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    if !is_github_host(&host) {
        return None;
    }

    let mut rest = parsed.path().trim_start_matches('/');
    if rest.is_empty() {
        return None;
    }

    // Keep compatibility with source forms that append `@ref` before a fragment.
    let rendered = parsed.to_string();
    let at_ref_from_url = rendered
        .find('@')
        .and_then(|i| rendered[i + 1..].split(['#', '?']).next())
        .filter(|s| !s.is_empty());

    // `@ref` suffix may be appended before any fragment
    let (path_wo_ref, at_ref_in_path) = split_at_ref(rest);
    rest = path_wo_ref;
    let at_ref = at_ref_in_path.or(at_ref_from_url);

    // Fragment `#subdir`
    let fragment = parsed.fragment();
    let subdir = fragment.map(str::to_owned);

    // Strip trailing `.git`
    let rest = rest.strip_suffix(".git").unwrap_or(rest);

    // Split into path segments (max 4: owner / repo / kind / ref)
    let segments: Vec<&str> = rest.splitn(4, '/').collect();
    if segments.len() < 2 {
        return None;
    }

    let owner = validate_ident(segments[0])?;
    let repo = validate_ident(segments[1])?;

    let git_ref: Option<String> = if let Some(r) = at_ref {
        Some(r.to_owned())
    } else if segments.len() >= 4 {
        match segments[2] {
            "tree" => Some(segments[3].to_owned()),
            "releases" if segments[3].starts_with("tag/") => {
                Some(segments[3]["tag/".len()..].to_owned())
            }
            _ => None,
        }
    } else {
        None
    };

    Some(GitHubSource {
        host,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        git_ref,
        subdir,
    })
}

fn parse_shorthand(spec: &str) -> Option<GitHubSource> {
    let (spec, fragment) = split_fragment(spec);
    let subdir = fragment.map(str::to_owned);

    let (owner_repo, at_ref) = split_at_ref(spec);
    let (owner, repo) = owner_repo.split_once('/')?;

    let owner = validate_ident(owner)?;
    let repo = validate_ident(repo.strip_suffix(".git").unwrap_or(repo))?;

    Some(GitHubSource {
        host: "github.com".to_owned(),
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        git_ref: at_ref.map(str::to_owned),
        subdir,
    })
}

/// Return true if `s` looks like `owner/repo[...]`.
fn looks_like_owner_repo(s: &str) -> bool {
    if s.contains("://") {
        return false;
    }
    let slash = match s.find('/') {
        Some(i) => i,
        None => return false,
    };
    let owner = &s[..slash];
    !owner.is_empty()
        && owner
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

fn split_at_ref(s: &str) -> (&str, Option<&str>) {
    match s.find('@') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    }
}

fn split_fragment(s: &str) -> (&str, Option<&str>) {
    match s.find('#') {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    }
}

/// Validate a GitHub owner/repo identifier — reject path traversal.
fn validate_ident(s: &str) -> Option<&str> {
    if s.is_empty() || s == "." || s == ".." {
        return None;
    }
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        Some(s)
    } else {
        None
    }
}

fn is_github_host(host: &str) -> bool {
    host == "github.com" || host.starts_with("github.") || host.contains(".github.")
}

fn api_base(host: &str) -> String {
    if host == "github.com" {
        "https://api.github.com".to_owned()
    } else {
        format!("https://{host}/api/v3")
    }
}

fn web_base(host: &str) -> String {
    format!("https://{host}")
}

// ---------------------------------------------------------------------------
// Resolution (GitHub API)
// ---------------------------------------------------------------------------

/// Resolve a [`GitHubSource`] to a [`ResolvedSource`] with a full commit SHA.
pub fn resolve(src: &GitHubSource, token: Option<&str>) -> Result<ResolvedSource> {
    let client = GhClient::new(token)?;
    let api = api_base(&src.host);
    let base = format!("{api}/repos/{}/{}", src.owner, src.repo);

    let git_ref: String = match &src.git_ref {
        Some(r) => r.clone(),
        None => {
            let repo_info: RepoInfo = api_get(&client, &base)?;
            repo_info.default_branch
        }
    };

    let commit_sha = resolve_ref_to_sha(&client, &api, &src.host, &src.owner, &src.repo, &git_ref)?;
    let is_tag = probe_is_tag(&client, &api, &src.owner, &src.repo, &git_ref);
    let package_name = sanitize_bundle_name(&src.repo);

    let version = if git_ref.len() >= 40 && git_ref.chars().all(|c| c.is_ascii_hexdigit()) {
        commit_sha[..12].to_owned()
    } else {
        git_ref.trim_start_matches('v').to_owned()
    };

    Ok(ResolvedSource {
        host: src.host.clone(),
        owner: src.owner.clone(),
        repo: src.repo.clone(),
        git_ref,
        commit_sha,
        is_tag,
        package_name,
        version,
        source_repo_url: format!("{}/{}/{}", web_base(&src.host), src.owner, src.repo),
    })
}

fn resolve_ref_to_sha(
    client: &GhClient,
    api_base: &str,
    host: &str,
    owner: &str,
    repo: &str,
    git_ref: &str,
) -> Result<String> {
    let url = format!("{api_base}/repos/{owner}/{repo}/commits/{git_ref}");

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github.sha")
        .send()
        .with_context(|| format!("Failed to contact GitHub API at {url}"))?;

    match resp.status().as_u16() {
        403 => {
            let remaining = resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("?");
            bail!(
                "GitHub API rate limit hit (remaining: {remaining}). \
                 Set the GITHUB_TOKEN environment variable to raise the limit."
            );
        }
        404 => bail!(
            "Repository or ref not found: {host}/{owner}/{repo} @ {git_ref}. \
             Check that the repo is public and the tag/branch/SHA exists."
        ),
        s if s >= 400 => bail!("GitHub API error {s}: {url}"),
        _ => {}
    }

    let body = resp.text().context("Failed to read SHA response")?;
    let sha = body.trim().to_lowercase();
    if sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(sha);
    }

    // Fallback: full JSON response
    let commit: CommitResponse =
        serde_json::from_str(&sha).context("Could not parse commit SHA from GitHub response")?;
    Ok(commit.sha.to_lowercase())
}

fn probe_is_tag(client: &GhClient, api_base: &str, owner: &str, repo: &str, git_ref: &str) -> bool {
    let url = format!("{api_base}/repos/{owner}/{repo}/git/refs/tags/{git_ref}");
    client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Release-asset fast path
// ---------------------------------------------------------------------------

/// Check whether `resolved`'s tag has a `.sembundle` release asset.
/// Returns `Ok(None)` when there is no release or no matching asset (→ build path).
pub fn find_release_bundle_asset(
    resolved: &ResolvedSource,
    token: Option<&str>,
) -> Result<Option<ReleaseAsset>> {
    if !resolved.is_tag {
        return Ok(None);
    }

    let client = GhClient::new(token)?;
    let url = format!(
        "{}/repos/{}/{}/releases/tags/{}",
        api_base(&resolved.host),
        resolved.owner,
        resolved.repo,
        resolved.git_ref
    );

    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .with_context(|| format!("Failed to check releases at {url}"))?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let release: ReleaseResponse = resp
        .json()
        .context("Failed to parse GitHub release response")?;

    let bundle_name = format!("{}-{}.sembundle", resolved.package_name, resolved.version);

    let bundle_url = release
        .assets
        .iter()
        .find(|a| a.name == bundle_name)
        .or_else(|| {
            release
                .assets
                .iter()
                .find(|a| a.name.ends_with(".sembundle"))
        })
        .map(|a| a.browser_download_url.clone());

    let bundle_url = match bundle_url {
        Some(u) => u,
        None => return Ok(None),
    };

    let sig_url = release
        .assets
        .iter()
        .find(|a| a.name.ends_with(".sembundle.sig"))
        .map(|a| a.browser_download_url.clone());

    Ok(Some(ReleaseAsset {
        bundle_url,
        sig_url,
    }))
}

// ---------------------------------------------------------------------------
// Archive URL + download / extract
// ---------------------------------------------------------------------------

/// Build the tarball download URL for a resolved source.
pub fn archive_tarball_url(resolved: &ResolvedSource) -> String {
    let web = web_base(&resolved.host);
    if resolved.is_tag {
        format!(
            "{}/{}/{}/archive/refs/tags/{}.tar.gz",
            web, resolved.owner, resolved.repo, resolved.git_ref
        )
    } else if resolved.git_ref.len() >= 40
        && resolved.git_ref.chars().all(|c| c.is_ascii_hexdigit())
    {
        format!(
            "{}/{}/{}/archive/{}.tar.gz",
            web, resolved.owner, resolved.repo, resolved.git_ref
        )
    } else {
        format!(
            "{}/{}/{}/archive/refs/heads/{}.tar.gz",
            web, resolved.owner, resolved.repo, resolved.git_ref
        )
    }
}

/// Download and extract the GitHub repo tarball to `dest`.
///
/// Strips the top-level `<repo>-<ref>/` directory. Tar-Slip safe.
/// Returns `dest`.
pub fn download_and_extract_tarball(
    url: &str,
    token: Option<&str>,
    dest: &Path,
) -> Result<PathBuf> {
    let client = GhClient::new(token)?;

    eprintln!("[sempkg] Downloading source archive from GitHub ...");

    let resp = client
        .get(url)
        .send()
        .with_context(|| format!("Failed to download archive from {url}"))?;

    if !resp.status().is_success() {
        bail!(
            "Failed to download GitHub archive: HTTP {} from {url}",
            resp.status()
        );
    }

    let bytes = resp
        .bytes()
        .context("Failed to read archive response body")?;

    eprintln!(
        "[sempkg] Extracting archive ({} KiB) ...",
        bytes.len() / 1024
    );

    let cursor = std::io::Cursor::new(bytes);
    let gz = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(gz);

    for entry in archive
        .entries()
        .context("Failed to read archive entries")?
    {
        let mut entry = entry.context("Bad archive entry")?;
        let raw_path = entry.path().context("Bad entry path")?.to_path_buf();

        // Tar-Slip guard
        if raw_path.is_absolute() {
            bail!("Archive contains absolute path: {}", raw_path.display());
        }
        for component in raw_path.components() {
            use std::path::Component;
            if matches!(component, Component::ParentDir) {
                bail!("Archive contains path traversal: {}", raw_path.display());
            }
        }

        // Skip symlinks
        if entry.header().entry_type().is_symlink() {
            continue;
        }

        // Strip the first path component (the GitHub-generated `repo-ref/` prefix)
        let stripped: PathBuf = raw_path.components().skip(1).collect();
        if stripped == PathBuf::from("") {
            continue; // top-level dir entry itself
        }

        let out_path = dest.join(&stripped);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Cannot create dir {}", parent.display()))?;
        }

        if entry.header().entry_type().is_file() {
            entry
                .unpack(&out_path)
                .with_context(|| format!("Failed to extract {}", stripped.display()))?;
        }
    }

    Ok(dest.to_path_buf())
}

// ---------------------------------------------------------------------------
// Full git clone (for repos whose GitHub archive omits docs)
// ---------------------------------------------------------------------------

/// Shallow-clone the repository at `resolved.git_ref` into `dest` and return
/// the cloned root directory.
///
/// Uses `git clone --depth 1 --branch <ref>` so it is fast but still fetches
/// every file in the working tree, including documentation that GitHub strips
/// from auto-generated tar.gz archives via `.gitattributes export-ignore`.
///
/// Requires `git` to be installed and on PATH.
pub fn git_clone_at_ref(resolved: &ResolvedSource, dest: &Path) -> Result<PathBuf> {
    let git = which::which("git").map_err(|_| {
        anyhow::anyhow!(
            "`git` not found on PATH. Install git or omit --full to use the tar.gz archive."
        )
    })?;

    let clone_dir = dest.join(&resolved.repo);
    std::fs::create_dir_all(&clone_dir)
        .with_context(|| format!("Cannot create clone dir {}", clone_dir.display()))?;

    eprintln!(
        "[sempkg] Cloning {}/{} @ {} (shallow) ...",
        resolved.owner, resolved.repo, resolved.git_ref
    );

    let clone_url = format!(
        "{}/{}/{}.git",
        web_base(&resolved.host),
        resolved.owner,
        resolved.repo
    );

    // `--branch` works for both tags and branches; for a raw SHA we fall back
    // to cloning the default branch and then checking out the SHA.
    let is_raw_sha =
        resolved.git_ref.len() == 40 && resolved.git_ref.chars().all(|c| c.is_ascii_hexdigit());

    if is_raw_sha {
        // Clone without --branch, then checkout the specific commit.
        // Needs --no-single-branch so the commit is reachable.
        run_git(
            &git,
            &[
                "clone",
                "--depth",
                "1",
                "--no-single-branch",
                &clone_url,
                clone_dir.to_str().unwrap_or("."),
            ],
        )?;
        run_git(&git, &["checkout", &resolved.git_ref])?;
    } else {
        run_git(
            &git,
            &[
                "clone",
                "--depth",
                "1",
                "--branch",
                &resolved.git_ref,
                "--single-branch",
                &clone_url,
                clone_dir.to_str().unwrap_or("."),
            ],
        )?;
    }

    eprintln!("[sempkg] Clone complete.");
    Ok(clone_dir)
}

fn run_git(git: &std::path::Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(git)
        .args(args)
        .status()
        .with_context(|| format!("Failed to run git with args: {args:?}"))?;

    if !status.success() {
        bail!("`git {}` exited with status {}", args.join(" "), status);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Language detection
// ---------------------------------------------------------------------------

const LANG_EXTS: &[(&str, &str)] = &[
    ("py", "python"),
    ("rs", "rust"),
    ("cpp", "cpp"),
    ("cc", "cpp"),
    ("cxx", "cpp"),
    ("c", "c"),
    ("h", "cpp"),
    ("hpp", "cpp"),
    ("ts", "typescript"),
    ("tsx", "typescript"),
    ("js", "javascript"),
    ("jsx", "javascript"),
    ("go", "go"),
    ("java", "java"),
    ("cs", "csharp"),
    ("rb", "ruby"),
    ("swift", "swift"),
    ("kt", "kotlin"),
    ("scala", "scala"),
    ("hs", "haskell"),
    ("ex", "elixir"),
    ("exs", "elixir"),
    ("php", "php"),
    ("lua", "lua"),
    ("r", "r"),
    ("jl", "julia"),
    ("zig", "zig"),
    ("nim", "nim"),
];

/// Heuristic language detection by counting source file extensions (max depth 4).
pub fn detect_language(root: &Path) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();

    let walker = walkdir::WalkDir::new(root)
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file());

    for entry in walker {
        if let Some(ext) = entry.path().extension().and_then(|s| s.to_str()) {
            let ext_lower = ext.to_ascii_lowercase();
            for (e, lang) in LANG_EXTS {
                if *e == ext_lower.as_str() {
                    *counts.entry(lang).or_insert(0) += 1;
                    break;
                }
            }
        }
    }

    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(lang, _)| lang.to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

// ---------------------------------------------------------------------------
// Token helper
// ---------------------------------------------------------------------------

/// Read GitHub token from environment (`GITHUB_TOKEN` preferred, then `GH_TOKEN`).
pub fn github_token() -> Option<String> {
    github_token_for_host("github.com")
}

/// Read a GitHub token for a specific host.
///
/// Resolution order (first match wins):
/// 1) `GITHUB_TOKEN_<HOST>`
/// 2) `GH_TOKEN_<HOST>`
/// 3) For non-github.com hosts: `GITHUB_ENTERPRISE_TOKEN`, `GH_ENTERPRISE_TOKEN`
/// 4) `GITHUB_TOKEN`, `GH_TOKEN`
///
/// Where `<HOST>` uppercases and replaces non-alphanumeric chars with `_`.
/// Example: `github.company.com` -> `GITHUB_TOKEN_GITHUB_COMPANY_COM`.
pub fn github_token_for_host(host: &str) -> Option<String> {
    let suffix = host_env_suffix(host);

    let host_scoped = [
        format!("GITHUB_TOKEN_{suffix}"),
        format!("GH_TOKEN_{suffix}"),
    ];

    for key in host_scoped {
        if let Ok(v) = std::env::var(&key) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }

    if !host.eq_ignore_ascii_case("github.com") {
        for key in ["GITHUB_ENTERPRISE_TOKEN", "GH_ENTERPRISE_TOKEN"] {
            if let Ok(v) = std::env::var(key) {
                if !v.trim().is_empty() {
                    return Some(v);
                }
            }
        }
    }

    for key in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }

    None
}

fn host_env_suffix(host: &str) -> String {
    host.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Sanitize bundle name
// ---------------------------------------------------------------------------

/// Sanitize an arbitrary repo name into a valid sembundle package name.
pub fn sanitize_bundle_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    out = out.trim_matches('-').to_owned();

    while out.contains("--") {
        out = out.replace("--", "-");
    }

    if out.len() < 2 {
        out.push_str("00");
    }

    out
}

// ---------------------------------------------------------------------------
// GitHub API response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RepoInfo {
    default_branch: String,
}

#[derive(Deserialize)]
struct CommitResponse {
    sha: String,
}

#[derive(Deserialize)]
struct ReleaseResponse {
    assets: Vec<ReleaseAssetEntry>,
}

#[derive(Deserialize)]
struct ReleaseAssetEntry {
    name: String,
    browser_download_url: String,
}

fn api_get<T: serde::de::DeserializeOwned>(client: &GhClient, url: &str) -> Result<T> {
    let resp = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .with_context(|| format!("GitHub API request failed: {url}"))?;

    match resp.status().as_u16() {
        403 => {
            let remaining = resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("?");
            bail!(
                "GitHub API rate limit hit (remaining: {remaining}). \
                 Set GITHUB_TOKEN to raise the limit."
            );
        }
        404 => bail!("GitHub API 404: {url} — check the repo/ref exists and is public."),
        s if s >= 400 => bail!("GitHub API error {s}: {url}"),
        _ => {}
    }

    resp.json::<T>()
        .with_context(|| format!("Failed to parse GitHub API response from {url}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_shorthand_with_ref() {
        let src = parse_source("pandas-dev/pandas@v2.2.2").unwrap();
        assert_eq!(src.host, "github.com");
        assert_eq!(src.owner, "pandas-dev");
        assert_eq!(src.repo, "pandas");
        assert_eq!(src.git_ref.as_deref(), Some("v2.2.2"));
        assert!(src.subdir.is_none());
    }

    #[test]
    fn test_host_env_suffix() {
        assert_eq!(host_env_suffix("github.com"), "GITHUB_COM");
        assert_eq!(host_env_suffix("github.company.com"), "GITHUB_COMPANY_COM");
    }

    #[test]
    fn test_parse_shorthand_no_ref() {
        let src = parse_source("pandas-dev/pandas").unwrap();
        assert_eq!(src.owner, "pandas-dev");
        assert_eq!(src.repo, "pandas");
        assert!(src.git_ref.is_none());
    }

    #[test]
    fn test_parse_full_url() {
        let src = parse_source("https://github.com/pandas-dev/pandas").unwrap();
        assert_eq!(src.host, "github.com");
        assert_eq!(src.owner, "pandas-dev");
        assert_eq!(src.repo, "pandas");
        assert!(src.git_ref.is_none());
    }

    #[test]
    fn test_parse_enterprise_releases_tag_url() {
        let src = parse_source("https://github.company.com/org/repo/releases/tag/v3.0.3").unwrap();
        assert_eq!(src.host, "github.company.com");
        assert_eq!(src.owner, "org");
        assert_eq!(src.repo, "repo");
        assert_eq!(src.git_ref.as_deref(), Some("v3.0.3"));
    }

    #[test]
    fn test_parse_url_with_at_ref() {
        let src = parse_source("https://github.com/pandas-dev/pandas@v2.2.2").unwrap();
        assert_eq!(src.git_ref.as_deref(), Some("v2.2.2"));
    }

    #[test]
    fn test_parse_url_tree_ref() {
        let src = parse_source("https://github.com/pandas-dev/pandas/tree/v2.2.2").unwrap();
        assert_eq!(src.git_ref.as_deref(), Some("v2.2.2"));
    }

    #[test]
    fn test_parse_url_releases_tag() {
        let src = parse_source("https://github.com/pandas-dev/pandas/releases/tag/v2.2.2").unwrap();
        assert_eq!(src.git_ref.as_deref(), Some("v2.2.2"));
    }

    #[test]
    fn test_parse_url_git_suffix() {
        let src = parse_source("https://github.com/owner/repo.git@v1.0").unwrap();
        assert_eq!(src.repo, "repo");
        assert_eq!(src.git_ref.as_deref(), Some("v1.0"));
    }

    #[test]
    fn test_parse_subdir() {
        let src = parse_source("owner/repo@v1.0#packages/core").unwrap();
        assert_eq!(src.subdir.as_deref(), Some("packages/core"));
        assert_eq!(src.git_ref.as_deref(), Some("v1.0"));
    }

    #[test]
    fn test_bare_name_version_is_none() {
        assert!(parse_source("pandas@2.2.2").is_none());
        assert!(parse_source("aws-sdk@1.11.210").is_none());
        assert!(parse_source("mylib").is_none());
    }

    #[test]
    fn test_sanitize_bundle_name() {
        assert_eq!(sanitize_bundle_name("pandas"), "pandas");
        assert_eq!(sanitize_bundle_name("pandas-dev"), "pandas-dev");
        assert_eq!(sanitize_bundle_name("My_Lib"), "my-lib");
        assert_eq!(sanitize_bundle_name("--bad--"), "bad");
        assert_eq!(sanitize_bundle_name("a"), "a00");
    }

    #[test]
    fn test_archive_url_tag() {
        let r = ResolvedSource {
            host: "github.com".into(),
            owner: "pandas-dev".into(),
            repo: "pandas".into(),
            git_ref: "v2.2.2".into(),
            commit_sha: "a".repeat(40),
            is_tag: true,
            package_name: "pandas".into(),
            version: "2.2.2".into(),
            source_repo_url: "https://github.com/pandas-dev/pandas".into(),
        };
        assert_eq!(
            archive_tarball_url(&r),
            "https://github.com/pandas-dev/pandas/archive/refs/tags/v2.2.2.tar.gz"
        );
    }

    #[test]
    fn test_archive_url_branch() {
        let r = ResolvedSource {
            host: "github.com".into(),
            owner: "owner".into(),
            repo: "repo".into(),
            git_ref: "main".into(),
            commit_sha: "b".repeat(40),
            is_tag: false,
            package_name: "repo".into(),
            version: "main".into(),
            source_repo_url: "https://github.com/owner/repo".into(),
        };
        assert_eq!(
            archive_tarball_url(&r),
            "https://github.com/owner/repo/archive/refs/heads/main.tar.gz"
        );
    }

    #[test]
    fn test_archive_url_enterprise_tag() {
        let r = ResolvedSource {
            host: "github.company.com".into(),
            owner: "org".into(),
            repo: "repo".into(),
            git_ref: "v3.0.3".into(),
            commit_sha: "c".repeat(40),
            is_tag: true,
            package_name: "repo".into(),
            version: "3.0.3".into(),
            source_repo_url: "https://github.company.com/org/repo".into(),
        };
        assert_eq!(
            archive_tarball_url(&r),
            "https://github.company.com/org/repo/archive/refs/tags/v3.0.3.tar.gz"
        );
    }
}
