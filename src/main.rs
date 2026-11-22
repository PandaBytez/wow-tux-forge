mod addon;
mod github;
mod security;
mod storage;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use github::{Release, ReleaseAsset, ReleaseOption, RepoSummary};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table,
    TableState, Tabs, Wrap,
};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;
use storage::{Config, InstalledAddon, State};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Installed,
    Search,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchFocus {
    Input,
    Results,
}

enum BgResult {
    SearchDone(Result<Vec<RepoSummary>, String>),
    ReleaseForInstall(String, Result<Vec<ReleaseOption>, String>),
    InstallBatchDone {
        installed: Vec<(String, InstalledAddon)>,
        flagged: Vec<(PendingInstall, security::AuditReport)>,
        errors: Vec<String>,
    },
    CheckUpdatesDone(Vec<(String, Result<Release, String>)>),
    UpdateAllDone {
        updated: Vec<(String, InstalledAddon)>,
        flagged: Vec<(PendingInstall, security::AuditReport)>,
        errors: Vec<String>,
        up_to_date_count: usize,
    },
    LinkLocalDone {
        linked: Vec<(String, InstalledAddon)>,
        failed: usize,
    },
    AuditDone {
        folder_count: usize,
        report: security::AuditReport,
    },
}

struct AssetPicker {
    repo: String,
    options: Vec<ReleaseOption>,
    selected: Vec<bool>,
    state: ListState,
}

struct SnippetPreview {
    title: String,
    target_line: usize,
    lines: Vec<(usize, String)>,
}

struct PendingInstall {
    state_key: String,
    tag: String,
    asset_name: String,
    tmp_zip: PathBuf,
    old_folders: Vec<String>,
}

struct AuditModal {
    title: String,
    report: security::AuditReport,
    state: ListState,
    snippet: Option<SnippetPreview>,
    pending_install: Option<PendingInstall>,
}

struct ConfirmDelete {
    label: String,
    folders: Vec<String>,
}

struct App {
    config: Config,
    state: State,
    tab: Tab,
    installed_list_state: TableState,
    latest_tags: BTreeMap<String, Result<String, String>>,
    local_addons: Vec<addon::LocalAddon>,

    search_query: String,
    search_focus: SearchFocus,
    search_results: Vec<RepoSummary>,
    search_list_state: TableState,

    editing_path: bool,
    path_input: String,
    asset_picker: Option<AssetPicker>,
    audit_modal: Option<AuditModal>,
    pending_audits: VecDeque<(PendingInstall, security::AuditReport)>,
    confirm_delete: Option<ConfirmDelete>,

    status: String,
    busy: bool,
    rx: Option<Receiver<BgResult>>,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        let config = storage::load_config();
        let state = storage::load_state();
        let needs_path = config.addons_path.trim().is_empty();
        let path_input = config.addons_path.clone();

        let mut installed_list_state = TableState::default();
        if !state.addons.is_empty() {
            installed_list_state.select(Some(0));
        }

        let latest_tags: BTreeMap<String, Result<String, String>> = state
            .cached_latest_tags
            .iter()
            .map(|(k, v)| (k.clone(), Ok(v.clone())))
            .collect();

        let status = if needs_path {
            "Set your WoW Interface/AddOns path to get started.".to_string()
        } else {
            format!("Loaded {} installed addon(s).", state.addons.len())
        };

        let mut app = Self {
            config,
            state,
            tab: Tab::Installed,
            installed_list_state,
            latest_tags,
            local_addons: Vec::new(),
            search_query: String::new(),
            search_focus: SearchFocus::Input,
            search_results: Vec::new(),
            search_list_state: TableState::default(),
            editing_path: needs_path,
            path_input,
            asset_picker: None,
            audit_modal: None,
            pending_audits: VecDeque::new(),
            confirm_delete: None,
            status,
            busy: false,
            rx: None,
            should_quit: false,
        };
        app.refresh_local_addons();
        if !needs_path {
            let addons_path = Path::new(&app.config.addons_path);
            let mut backfilled = false;
            for entry in app.state.addons.values_mut() {
                if entry.installed_at.is_none()
                    && let Some(ts) = folder_mtime_unix(addons_path, &entry.folders)
                {
                    entry.installed_at = Some(ts);
                    backfilled = true;
                }
            }
            if backfilled {
                let _ = storage::save_state(&app.state);
            }
            app.status = format!(
                "Loaded {} GitHub addon(s), {} local addon folder(s).",
                app.state.addons.len(),
                app.local_addons.len()
            );
            if app.local_addons.iter().any(|l| l.github_hint.is_some()) {
                app.spawn_link_local_addons();
            } else if !app.state.addons.is_empty() {
                app.spawn_check_updates();
            }
        }
        app
    }

    fn refresh_local_addons(&mut self) {
        let managed: HashSet<String> = self
            .state
            .addons
            .values()
            .flat_map(|a| a.folders.iter().cloned())
            .collect();
        self.local_addons = addon::scan_unmanaged_addons(Path::new(&self.config.addons_path), &managed);
        let total = self.installed_total_len();
        if total == 0 {
            self.installed_list_state.select(None);
        } else if self.installed_list_state.selected().is_none() {
            self.installed_list_state.select(Some(0));
        } else if let Some(sel) = self.installed_list_state.selected()
            && sel >= total
        {
            self.installed_list_state.select(Some(total - 1));
        }
    }

    fn installed_total_len(&self) -> usize {
        self.state.addons.len() + self.local_addons.len()
    }

    fn installed_keys(&self) -> Vec<String> {
        self.state.addons.keys().cloned().collect()
    }

    fn spawn_search(&mut self) {
        let query = self.search_query.trim().to_string();
        if query.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        self.status = format!("Searching GitHub for \"{query}\"...");
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            let res = github::make_client().and_then(|c| github::search_repos(&c, &query));
            let _ = tx.send(BgResult::SearchDone(res));
        });
    }

    fn spawn_fetch_release(&mut self, repo: String) {
        if self.busy {
            return;
        }
        if !Path::new(&self.config.addons_path).is_dir() {
            self.status = "Invalid WoW AddOns path. Press 'c' to configure it.".to_string();
            return;
        }
        self.busy = true;
        self.status = format!("Fetching releases for {repo}...");
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        thread::spawn(move || {
            let res = github::make_client().and_then(|c| github::fetch_install_options(&c, &repo));
            let _ = tx.send(BgResult::ReleaseForInstall(repo, res));
        });
    }

    fn spawn_install_batch(&mut self, items: Vec<(String, String, ReleaseAsset)>) {
        if self.busy || items.is_empty() {
            return;
        }
        let addons_dir = self.config.addons_path.clone();
        let existing_folders: Vec<Vec<String>> = items
            .iter()
            .map(|(key, _, _)| {
                self.state
                    .addons
                    .get(key)
                    .map(|a| a.folders.clone())
                    .unwrap_or_default()
            })
            .collect();

        self.busy = true;
        if items.len() == 1 {
            let (key, _, asset) = &items[0];
            self.status = format!(
                "Downloading {key} ({}, {} downloads)...",
                asset.name, asset.download_count
            );
        } else {
            self.status = format!("Downloading and installing {} package(s)...", items.len());
        }

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);

        thread::spawn(move || {
            let mut installed = Vec::new();
            let mut flagged = Vec::new();
            let mut errors = Vec::new();

            let client = match github::make_client() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(BgResult::InstallBatchDone {
                        installed,
                        flagged,
                        errors: vec![e],
                    });
                    return;
                }
            };

            let addons_path = Path::new(&addons_dir);
            for (idx, ((state_key, tag, asset), old_folders)) in
                items.into_iter().zip(existing_folders).enumerate()
            {
                let tmp_zip = cache_temp_zip(&format!("install-{idx}"));
                match download_and_audit_zip(
                    &client,
                    &asset.browser_download_url,
                    addons_path,
                    &state_key,
                    &tag,
                    &asset.name,
                    old_folders,
                    &tmp_zip,
                ) {
                    Ok(InstallOutcome::Installed(entry)) => {
                        let _ = std::fs::remove_file(&tmp_zip);
                        installed.push((state_key, entry));
                    }
                    Ok(InstallOutcome::Flagged(pending, report)) => {
                        flagged.push((pending, report));
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp_zip);
                        errors.push(format!("{state_key}: {e}"));
                    }
                }
            }

            let _ = tx.send(BgResult::InstallBatchDone {
                installed,
                flagged,
                errors,
            });
        });
    }

    fn spawn_check_updates(&mut self) {
        if self.busy || self.state.addons.is_empty() {
            return;
        }
        const CHECK_COOLDOWN_SECS: u64 = 3600;
        if let Some(last) = self.state.last_check_at {
            let elapsed = now_unix_secs().saturating_sub(last);
            if elapsed < CHECK_COOLDOWN_SECS {
                let mins_left = (CHECK_COOLDOWN_SECS - elapsed).div_ceil(60);
                self.status = format!(
                    "Update check skipped: checked {}m ago (limit: once per hour, next in {mins_left}m).",
                    elapsed / 60
                );
                return;
            }
        }
        let repos = self.installed_keys();
        self.busy = true;
        self.status = format!("Checking updates for {} addon(s)...", repos.len());
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);

        thread::spawn(move || {
            let results = match github::make_client() {
                Ok(client) => map_parallel(repos, 6, |repo| {
                    let res = github::latest_release_for_key(&client, &repo);
                    (repo, res)
                }),
                Err(e) => repos.into_iter().map(|r| (r, Err(e.clone()))).collect(),
            };
            let _ = tx.send(BgResult::CheckUpdatesDone(results));
        });
    }

    fn spawn_update_all(&mut self) {
        if self.busy {
            return;
        }
        if self.state.addons.is_empty() {
            self.status = "No addons installed yet.".to_string();
            return;
        }
        if !Path::new(&self.config.addons_path).is_dir() {
            self.status = "Invalid WoW AddOns path. Press 'c' to configure it.".to_string();
            return;
        }

        let addons_dir = self.config.addons_path.clone();
        let total = self.state.addons.len();
        let tracked: BTreeMap<String, InstalledAddon> = self
            .state
            .addons
            .iter()
            .filter(|(repo, current)| match self.latest_tags.get(*repo) {
                Some(Ok(latest)) => latest != &current.tag,
                Some(Err(_)) | None => self.latest_tags.is_empty(),
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        if tracked.is_empty() {
            self.status = format!("All {total} addon(s) are already up to date.");
            return;
        }
        let skipped_up_to_date = total - tracked.len();

        self.busy = true;
        self.status = format!("Updating {} addon(s)...", tracked.len());
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);

        thread::spawn(move || {
            let mut updated = Vec::new();
            let mut flagged = Vec::new();
            let mut errors = Vec::new();
            let mut up_to_date_count = skipped_up_to_date;

            let client = match github::make_client() {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(BgResult::UpdateAllDone {
                        updated,
                        flagged,
                        errors: vec![e],
                        up_to_date_count,
                    });
                    return;
                }
            };

            let addons_path = Path::new(&addons_dir);
            for (idx, (repo, current)) in tracked.into_iter().enumerate() {
                let release = match github::latest_release_for_key(&client, &repo) {
                    Ok(r) => r,
                    Err(e) => {
                        errors.push(format!("{repo}: {e}"));
                        continue;
                    }
                };

                if release.tag_name == current.tag {
                    up_to_date_count += 1;
                    continue;
                }

                let asset = release
                    .assets
                    .iter()
                    .find(|a| a.name == current.asset_name)
                    .or_else(|| release.assets.first())
                    .cloned();

                let Some(asset) = asset else {
                    errors.push(format!("{repo}: no .zip asset in {}", release.tag_name));
                    continue;
                };

                let tmp_zip = cache_temp_zip(&format!("update-{idx}"));
                match download_and_audit_zip(
                    &client,
                    &asset.browser_download_url,
                    addons_path,
                    &repo,
                    &release.tag_name,
                    &asset.name,
                    current.folders,
                    &tmp_zip,
                ) {
                    Ok(InstallOutcome::Installed(new_entry)) => {
                        let _ = std::fs::remove_file(&tmp_zip);
                        updated.push((repo, new_entry));
                    }
                    Ok(InstallOutcome::Flagged(pending, report)) => {
                        flagged.push((pending, report));
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp_zip);
                        errors.push(format!("{repo}: {e}"));
                    }
                }
            }

            let _ = tx.send(BgResult::UpdateAllDone {
                updated,
                flagged,
                errors,
                up_to_date_count,
            });
        });
    }

    fn spawn_link_local_addons(&mut self) {
        if self.busy {
            return;
        }
        let hinted: Vec<(String, Option<String>, String)> = self
            .local_addons
            .iter()
            .filter_map(|l| {
                l.github_hint
                    .as_ref()
                    .map(|repo| (l.folder.clone(), l.version.clone(), repo.clone()))
            })
            .collect();

        if hinted.is_empty() {
            self.status = "No local addons have GitHub URLs in their .toc. Press Enter on one to search.".to_string();
            return;
        }

        self.busy = true;
        self.status = format!("Linking {} local addon(s) via .toc GitHub URLs...", hinted.len());
        let addons_dir = self.config.addons_path.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);

        thread::spawn(move || {
            let mut linked = Vec::new();
            let mut failed = 0usize;

            let Ok(client) = github::make_client() else {
                let _ = tx.send(BgResult::LinkLocalDone {
                    linked,
                    failed: hinted.len(),
                });
                return;
            };

            // Group folders by repo in case multiple folders point to the same GitHub repo.
            let mut by_repo: BTreeMap<String, (Vec<String>, Option<String>)> = BTreeMap::new();
            for (folder, version, repo) in hinted {
                let entry = by_repo.entry(repo).or_insert_with(|| (Vec::new(), version));
                entry.0.push(folder);
            }

            let results = map_parallel(
                by_repo.into_iter().collect(),
                6,
                |(repo, (folders, local_ver))| match github::latest_release(&client, &repo) {
                    Ok(rel) if !rel.assets.is_empty() => {
                        let installed_at = folder_mtime_unix(Path::new(&addons_dir), &folders)
                            .or_else(|| Some(now_unix_secs()));
                        Ok((
                            repo,
                            InstalledAddon {
                                tag: local_ver.unwrap_or_else(|| "local".to_string()),
                                asset_name: rel.assets[0].name.clone(),
                                folders,
                                installed_at,
                            },
                        ))
                    }
                    _ => Err(()),
                },
            );
            for res in results {
                match res {
                    Ok(item) => linked.push(item),
                    Err(()) => failed += 1,
                }
            }

            let _ = tx.send(BgResult::LinkLocalDone { linked, failed });
        });
    }

    fn prompt_delete_selected(&mut self) {
        let Some(idx) = self.installed_list_state.selected() else {
            return;
        };
        let managed_len = self.state.addons.len();
        if idx < managed_len {
            let Some(repo) = self.installed_keys().get(idx).cloned() else {
                return;
            };
            let Some(entry) = self.state.addons.get(&repo) else {
                return;
            };
            self.confirm_delete = Some(ConfirmDelete {
                label: repo,
                folders: entry.folders.clone(),
            });
        } else if let Some(local) = self.local_addons.get(idx - managed_len) {
            self.confirm_delete = Some(ConfirmDelete {
                label: local.folder.clone(),
                folders: vec![local.folder.clone()],
            });
        }
    }

    fn execute_delete(&mut self, confirm: ConfirmDelete) {
        let addons_path = Path::new(&self.config.addons_path);
        if addons_path.is_dir()
            && let Err(e) = addon::remove_folders(addons_path, &confirm.folders)
        {
            self.status = format!("Failed removing {}: {e}", confirm.label);
            return;
        }

        if self.state.addons.remove(&confirm.label).is_some() {
            self.latest_tags.remove(&confirm.label);
            self.state.cached_latest_tags.remove(&confirm.label);
            if let Err(e) = storage::save_state(&self.state) {
                self.status = e;
                return;
            }
        }

        self.refresh_local_addons();
        self.status = format!("Removed {}.", confirm.label);
    }

    fn spawn_audit_all(&mut self) {
        if self.busy {
            return;
        }
        let addons_path = Path::new(&self.config.addons_path);
        if !addons_path.is_dir() {
            self.status = "Invalid WoW AddOns path.".to_string();
            return;
        }

        let mut folders_set = std::collections::BTreeSet::new();
        for entry in self.state.addons.values() {
            for f in &entry.folders {
                folders_set.insert(f.clone());
            }
        }
        for local in &self.local_addons {
            folders_set.insert(local.folder.clone());
        }
        if folders_set.is_empty() {
            self.status = "No installed addon folders to audit.".to_string();
            return;
        }

        let folders: Vec<String> = folders_set.into_iter().collect();
        let folder_count = folders.len();
        let addons_dir = self.config.addons_path.clone();

        self.busy = true;
        self.status = format!("Auditing {folder_count} addon folder(s)...");
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);

        thread::spawn(move || {
            let report = security::audit_installed_folders(Path::new(&addons_dir), &folders);
            let _ = tx.send(BgResult::AuditDone {
                folder_count,
                report,
            });
        });
    }

    fn poll_bg(&mut self) {
        let Some(rx) = &self.rx else {
            return;
        };
        let Ok(msg) = rx.try_recv() else {
            return;
        };
        self.rx = None;
        self.busy = false;

        match msg {
            BgResult::SearchDone(Ok(items)) => {
                let count = items.len();
                self.search_results = items;
                if count > 0 {
                    self.search_list_state.select(Some(0));
                    self.search_focus = SearchFocus::Results;
                } else {
                    self.search_list_state.select(None);
                }
                self.status = format!("Found {count} repository result(s).");
            }
            BgResult::SearchDone(Err(e)) => {
                self.status = format!("Search error: {e}");
            }
            BgResult::ReleaseForInstall(repo, Ok(options)) => {
                if options.is_empty() {
                    self.status = format!("{repo} has no .zip assets.");
                } else if options.len() == 1 {
                    let opt = options.into_iter().next().unwrap();
                    let state_key = install_state_key(&repo, &opt.tag_name);
                    self.spawn_install_batch(vec![(state_key, opt.tag_name, opt.asset)]);
                } else {
                    let mut state = ListState::default();
                    state.select(Some(0));
                    let selected = vec![false; options.len()];
                    self.status = format!(
                        "Select packages for {repo} ({} options): Space to toggle, Enter to install.",
                        options.len()
                    );
                    self.asset_picker = Some(AssetPicker {
                        repo,
                        options,
                        selected,
                        state,
                    });
                }
            }
            BgResult::ReleaseForInstall(repo, Err(e)) => {
                self.status = format!("{repo}: {e}");
            }
            BgResult::InstallBatchDone {
                installed,
                flagged,
                errors,
            } => {
                let installed_count = installed.len();
                let mut last_key = None;
                for (key, entry) in installed {
                    self.state
                        .cached_latest_tags
                        .insert(key.clone(), entry.tag.clone());
                    self.latest_tags.insert(key.clone(), Ok(entry.tag.clone()));
                    self.state.addons.insert(key.clone(), entry);
                    last_key = Some(key);
                }
                for item in flagged {
                    self.pending_audits.push_back(item);
                }
                if let Err(e) = storage::save_state(&self.state) {
                    self.status = e;
                } else {
                    self.refresh_local_addons();
                    if let Some(ref k) = last_key
                        && let Some(idx) = self.installed_keys().iter().position(|x| x == k)
                    {
                        self.installed_list_state.select(Some(idx));
                    }
                    if errors.is_empty() {
                        self.status = format!("Installed {installed_count} package(s).");
                    } else {
                        self.status = format!(
                            "Installed {installed_count}, {} error(s): {}",
                            errors.len(),
                            errors[0]
                        );
                    }
                    self.open_next_pending_audit();
                }
            }
            BgResult::CheckUpdatesDone(results) => {
                let mut updates = 0usize;
                let mut any_ok = false;
                for (repo, res) in results {
                    match res {
                        Ok(rel) => {
                            any_ok = true;
                            if let Some(cur) = self.state.addons.get(&repo)
                                && rel.tag_name != cur.tag
                            {
                                updates += 1;
                            }
                            self.state
                                .cached_latest_tags
                                .insert(repo.clone(), rel.tag_name.clone());
                            self.latest_tags.insert(repo, Ok(rel.tag_name));
                        }
                        Err(e) => {
                            self.latest_tags.insert(repo, Err(e));
                        }
                    }
                }
                if any_ok {
                    self.state.last_check_at = Some(now_unix_secs());
                    let _ = storage::save_state(&self.state);
                }
                self.status = format!("Check complete: {updates} update(s) available.");
            }
            BgResult::UpdateAllDone {
                updated,
                flagged,
                errors,
                up_to_date_count,
            } => {
                let updated_count = updated.len();
                for (repo, entry) in updated {
                    self.state
                        .cached_latest_tags
                        .insert(repo.clone(), entry.tag.clone());
                    self.latest_tags.insert(repo.clone(), Ok(entry.tag.clone()));
                    self.state.addons.insert(repo, entry);
                }
                for item in flagged {
                    self.pending_audits.push_back(item);
                }
                self.refresh_local_addons();
                let save_err = storage::save_state(&self.state).err();
                if let Some(e) = save_err {
                    self.status = e;
                } else if errors.is_empty() {
                    self.status = format!(
                        "Updated {updated_count} addon(s), {up_to_date_count} already up to date."
                    );
                } else {
                    self.status = format!(
                        "Updated {updated_count}, up-to-date {up_to_date_count}, {} error(s): {}",
                        errors.len(),
                        errors[0]
                    );
                }
                self.open_next_pending_audit();
            }
            BgResult::LinkLocalDone { linked, failed } => {
                let count = linked.len();
                for (repo, entry) in linked {
                    if let Some(existing) = self.state.addons.get_mut(&repo) {
                        for f in entry.folders {
                            if !existing.folders.contains(&f) {
                                existing.folders.push(f);
                            }
                        }
                    } else {
                        self.state.addons.insert(repo, entry);
                    }
                }
                self.refresh_local_addons();
                if let Err(e) = storage::save_state(&self.state) {
                    self.status = e;
                } else {
                    self.status = format!(
                        "Linked {count} addon(s) to GitHub ({failed} had no GitHub release zips)."
                    );
                    if self.latest_tags.is_empty() && !self.state.addons.is_empty() {
                        self.spawn_check_updates();
                    }
                }
            }
            BgResult::AuditDone {
                folder_count,
                report,
            } => {
                let finding_count = report.findings.len();
                let mut state = ListState::default();
                if finding_count > 0 {
                    state.select(Some(0));
                }
                self.status = if finding_count == 0 {
                    format!("Security Report: {folder_count} folder(s) audited — Clean.")
                } else {
                    format!(
                        "Security Report: {folder_count} folder(s) audited — {finding_count} finding(s)."
                    )
                };
                self.audit_modal = Some(AuditModal {
                    title: format!("{folder_count} folder(s)"),
                    report,
                    state,
                    snippet: None,
                    pending_install: None,
                });
            }
        }
    }

    fn open_next_pending_audit(&mut self) {
        if self.audit_modal.is_some() {
            return;
        }
        if let Some((pending, report)) = self.pending_audits.pop_front() {
            let mut state = ListState::default();
            if !report.findings.is_empty() {
                state.select(Some(0));
            }
            self.status = format!(
                "Security audit flagged {} ({} finding(s)) — review and press [y] to install or [n] to cancel.",
                pending.state_key,
                report.findings.len()
            );
            self.audit_modal = Some(AuditModal {
                title: pending.state_key.clone(),
                report,
                state,
                snippet: None,
                pending_install: Some(pending),
            });
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        if self.editing_path {
            match key.code {
                KeyCode::Esc => {
                    if !self.config.addons_path.is_empty() {
                        self.editing_path = false;
                        self.path_input = self.config.addons_path.clone();
                    }
                }
                KeyCode::Enter => {
                    let trimmed = self.path_input.trim().to_string();
                    if Path::new(&trimmed).is_dir() {
                        self.config.addons_path = trimmed;
                        match storage::save_config(&self.config) {
                            Ok(()) => {
                                self.editing_path = false;
                                self.refresh_local_addons();
                                self.status = format!(
                                    "Saved AddOns path ({} local folder(s) found).",
                                    self.local_addons.len()
                                );
                                if self.local_addons.iter().any(|l| l.github_hint.is_some()) {
                                    self.spawn_link_local_addons();
                                }
                            }
                            Err(e) => self.status = e,
                        }
                    } else {
                        self.status = "Directory does not exist. Check the path and try again.".to_string();
                    }
                }
                KeyCode::Backspace => {
                    self.path_input.pop();
                }
                KeyCode::Char(c) => {
                    self.path_input.push(c);
                }
                _ => {}
            }
            return;
        }

        if let Some(confirm) = self.confirm_delete.take() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.execute_delete(confirm);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.status = "Delete cancelled.".to_string();
                }
                _ => {
                    self.confirm_delete = Some(confirm);
                }
            }
            return;
        }

        if let Some(modal) = &mut self.audit_modal {
            if modal.snippet.is_some() {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    modal.snippet = None;
                }
                return;
            }
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') if modal.pending_install.is_some() => {
                    let modal = self.audit_modal.take().unwrap();
                    let pending = modal.pending_install.unwrap();
                    let addons_path = Path::new(&self.config.addons_path);
                    let res = extract_staged_zip(&pending.tmp_zip, addons_path, &pending.old_folders);
                    let _ = std::fs::remove_file(&pending.tmp_zip);
                    match res {
                        Ok(folders) => {
                            let entry = InstalledAddon {
                                tag: pending.tag.clone(),
                                asset_name: pending.asset_name,
                                folders,
                                installed_at: Some(now_unix_secs()),
                            };
                            self.latest_tags
                                .insert(pending.state_key.clone(), Ok(pending.tag));
                            self.state.addons.insert(pending.state_key.clone(), entry);
                            if let Err(e) = storage::save_state(&self.state) {
                                self.status = e;
                            } else {
                                self.refresh_local_addons();
                                self.status = format!("Installed {} (risk accepted).", pending.state_key);
                            }
                        }
                        Err(e) => {
                            self.status = format!("{}: {e}", pending.state_key);
                        }
                    }
                    self.open_next_pending_audit();
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Char('N') => {
                    let modal = self.audit_modal.take().unwrap();
                    if let Some(pending) = modal.pending_install {
                        let _ = std::fs::remove_file(&pending.tmp_zip);
                        self.status = format!("Cancelled {} (security audit).", pending.state_key);
                        self.open_next_pending_audit();
                    }
                }
                KeyCode::Enter => {
                    if modal.report.findings.is_empty() {
                        self.audit_modal = None;
                    } else if let Some(idx) = modal.state.selected()
                        && let Some(finding) = modal.report.findings.get(idx)
                    {
                        let res = if let Some(pending) = &modal.pending_install {
                            load_finding_snippet_from_zip(&pending.tmp_zip, &finding.location)
                        } else {
                            load_finding_snippet(
                                Path::new(&self.config.addons_path),
                                &finding.location,
                            )
                        };
                        match res {
                            Ok(snip) => modal.snippet = Some(snip),
                            Err(e) => self.status = e,
                        }
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let len = modal.report.findings.len();
                    modal.state.select(move_selection(modal.state.selected(), len, -1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let len = modal.report.findings.len();
                    modal.state.select(move_selection(modal.state.selected(), len, 1));
                }
                _ => {}
            }
            return;
        }

        if let Some(picker) = &mut self.asset_picker {
            match key.code {
                KeyCode::Esc => {
                    self.asset_picker = None;
                    self.status = "Install cancelled.".to_string();
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let len = picker.options.len();
                    picker.state.select(move_selection(picker.state.selected(), len, -1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let len = picker.options.len();
                    picker.state.select(move_selection(picker.state.selected(), len, 1));
                }
                KeyCode::Char(' ') => {
                    if let Some(idx) = picker.state.selected()
                        && let Some(flag) = picker.selected.get_mut(idx)
                    {
                        *flag = !*flag;
                    }
                }
                KeyCode::Char('a') => {
                    let all_selected = picker.selected.iter().all(|&s| s);
                    picker.selected.fill(!all_selected);
                }
                KeyCode::Enter => {
                    let picker = self.asset_picker.take().unwrap();
                    let mut chosen: Vec<(String, String, ReleaseAsset)> = picker
                        .options
                        .iter()
                        .zip(picker.selected.iter())
                        .filter(|&(_, checked)| *checked)
                        .map(|(opt, _)| {
                            (
                                install_state_key(&picker.repo, &opt.tag_name),
                                opt.tag_name.clone(),
                                opt.asset.clone(),
                            )
                        })
                        .collect();

                    if chosen.is_empty()
                        && let Some(idx) = picker.state.selected()
                    {
                        let opt = &picker.options[idx];
                        chosen.push((
                            install_state_key(&picker.repo, &opt.tag_name),
                            opt.tag_name.clone(),
                            opt.asset.clone(),
                        ));
                    }
                    self.spawn_install_batch(chosen);
                }
                _ => {}
            }
            return;
        }

        // Global shortcuts when not typing in search input.
        let typing_search = self.tab == Tab::Search && self.search_focus == SearchFocus::Input;
        if !typing_search {
            match key.code {
                KeyCode::Char('q') => {
                    self.should_quit = true;
                    return;
                }
                KeyCode::Char('1') => {
                    self.tab = Tab::Installed;
                    return;
                }
                KeyCode::Char('2') => {
                    self.tab = Tab::Search;
                    return;
                }
                KeyCode::Char('c') => {
                    self.path_input = self.config.addons_path.clone();
                    self.editing_path = true;
                    return;
                }
                _ => {}
            }
        }

        if key.code == KeyCode::Tab {
            self.tab = match self.tab {
                Tab::Installed => Tab::Search,
                Tab::Search => Tab::Installed,
            };
            return;
        }

        match self.tab {
            Tab::Installed => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    let len = self.installed_total_len();
                    self.installed_list_state
                        .select(move_selection(self.installed_list_state.selected(), len, -1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let len = self.installed_total_len();
                    self.installed_list_state
                        .select(move_selection(self.installed_list_state.selected(), len, 1));
                }
                KeyCode::Char('u') => self.spawn_update_all(),
                KeyCode::Char('l') => self.spawn_link_local_addons(),
                KeyCode::Char('a') => self.spawn_audit_all(),
                KeyCode::Enter => {
                    if let Some(idx) = self.installed_list_state.selected() {
                        let managed_len = self.state.addons.len();
                        if idx >= managed_len
                            && let Some(local) = self.local_addons.get(idx - managed_len)
                        {
                            self.search_query = local
                                .github_hint
                                .clone()
                                .unwrap_or_else(|| local.folder.clone());
                            self.tab = Tab::Search;
                            self.spawn_search();
                        }
                    }
                }
                KeyCode::Char('s') => {
                    self.refresh_local_addons();
                    self.status = format!(
                        "Scanned AddOns folder: {} GitHub, {} local.",
                        self.state.addons.len(),
                        self.local_addons.len()
                    );
                }
                KeyCode::Char('d') => {
                    if !self.busy {
                        self.prompt_delete_selected();
                    }
                }
                _ => {}
            },
            Tab::Search => match self.search_focus {
                SearchFocus::Input => match key.code {
                    KeyCode::Esc => {
                        if !self.search_results.is_empty() {
                            self.search_focus = SearchFocus::Results;
                        }
                    }
                    KeyCode::Down => {
                        if !self.search_results.is_empty() {
                            self.search_focus = SearchFocus::Results;
                        }
                    }
                    KeyCode::Enter => self.spawn_search(),
                    KeyCode::Backspace => {
                        self.search_query.pop();
                    }
                    KeyCode::Char(c) => {
                        self.search_query.push(c);
                    }
                    _ => {}
                },
                SearchFocus::Results => match key.code {
                    KeyCode::Char('/') => {
                        self.search_focus = SearchFocus::Input;
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let sel = self.search_list_state.selected().unwrap_or(0);
                        if sel == 0 {
                            self.search_focus = SearchFocus::Input;
                        } else {
                            self.search_list_state.select(Some(sel - 1));
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let len = self.search_results.len();
                        self.search_list_state
                            .select(move_selection(self.search_list_state.selected(), len, 1));
                    }
                    KeyCode::Enter | KeyCode::Char('i') => {
                        if let Some(idx) = self.search_list_state.selected()
                            && let Some(repo) = self.search_results.get(idx)
                        {
                            self.spawn_fetch_release(repo.full_name.clone());
                        }
                    }
                    _ => {}
                },
            },
        }
    }
}

fn extract_snippet_lines<R: io::BufRead>(
    reader: R,
    location: &str,
    target_line: usize,
) -> SnippetPreview {
    let start = target_line.saturating_sub(6).max(1);
    let end = target_line + 6;
    let mut lines = Vec::new();
    for (idx, line_res) in reader.lines().enumerate() {
        let num = idx + 1;
        if num > end {
            break;
        }
        if num >= start {
            lines.push((num, line_res.unwrap_or_default()));
        }
    }
    SnippetPreview {
        title: location.to_string(),
        target_line,
        lines,
    }
}

fn parse_location(location: &str) -> Result<(&str, usize), String> {
    let Some((rel, line_str)) = location.rsplit_once(':') else {
        return Err(format!("No line number available for {location}"));
    };
    let target_line: usize = line_str
        .parse()
        .map_err(|_| format!("Invalid line number in {location}"))?;
    Ok((rel, target_line))
}

fn load_finding_snippet(addons_dir: &Path, location: &str) -> Result<SnippetPreview, String> {
    let (rel, target_line) = parse_location(location)?;
    let file = std::fs::File::open(addons_dir.join(rel))
        .map_err(|e| format!("Failed opening {rel}: {e}"))?;
    Ok(extract_snippet_lines(io::BufReader::new(file), location, target_line))
}

fn load_finding_snippet_from_zip(zip_path: &Path, location: &str) -> Result<SnippetPreview, String> {
    let (rel, target_line) = parse_location(location)?;
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("Failed opening zip: {e}"))?;
    let mut archive = zip::ZipArchive::new(io::BufReader::new(file))
        .map_err(|e| format!("Invalid zip: {e}"))?;
    let entry = archive
        .by_name(rel)
        .map_err(|e| format!("File {rel} not found in zip: {e}"))?;
    Ok(extract_snippet_lines(io::BufReader::new(entry), location, target_line))
}

fn cache_temp_zip(kind: &str) -> PathBuf {
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("wow-tux-forge");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{kind}-{}.zip", std::process::id()))
}

enum InstallOutcome {
    Installed(InstalledAddon),
    Flagged(PendingInstall, security::AuditReport),
}

fn extract_staged_zip(
    tmp_zip: &Path,
    addons_path: &Path,
    old_folders: &[String],
) -> Result<Vec<String>, String> {
    // Extract to staging inside addons_path first so failed extractions never delete working addons.
    let staging = addons_path.join(format!(".wow-tux-forge-staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|e| format!("Failed creating staging dir {}: {e}", staging.display()))?;

    let folders = match addon::extract_addon_zip_file(tmp_zip, &staging) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    if !old_folders.is_empty()
        && let Err(e) = addon::remove_folders(addons_path, old_folders)
    {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    for folder in &folders {
        let dest = addons_path.join(folder);
        if dest.exists() {
            let _ = std::fs::remove_dir_all(&dest);
        }
        if let Err(e) = std::fs::rename(staging.join(folder), &dest) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(format!("Failed moving {folder} into AddOns: {e}"));
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
    Ok(folders)
}

#[allow(clippy::too_many_arguments)]
fn download_and_audit_zip(
    client: &reqwest::blocking::Client,
    url: &str,
    addons_path: &Path,
    state_key: &str,
    tag: &str,
    asset_name: &str,
    old_folders: Vec<String>,
    tmp_zip: &Path,
) -> Result<InstallOutcome, String> {
    github::download_to_file(client, url, tmp_zip)?;
    let report = security::audit_zip_file(tmp_zip)?;
    if !report.findings.is_empty() {
        return Ok(InstallOutcome::Flagged(
            PendingInstall {
                state_key: state_key.to_string(),
                tag: tag.to_string(),
                asset_name: asset_name.to_string(),
                tmp_zip: tmp_zip.to_path_buf(),
                old_folders,
            },
            report,
        ));
    }

    let folders = extract_staged_zip(tmp_zip, addons_path, &old_folders)?;
    Ok(InstallOutcome::Installed(InstalledAddon {
        tag: tag.to_string(),
        asset_name: asset_name.to_string(),
        folders,
        installed_at: Some(now_unix_secs()),
    }))
}

fn map_parallel<T: Send, R: Send>(
    items: Vec<T>,
    workers: usize,
    f: impl Fn(T) -> R + Send + Sync,
) -> Vec<R> {
    let total = items.len();
    if total == 0 {
        return Vec::new();
    }
    let queue = std::sync::Mutex::new(items.into_iter());
    let results = std::sync::Mutex::new(Vec::with_capacity(total));
    let worker_count = workers.min(total).max(1);
    thread::scope(|s| {
        for _ in 0..worker_count {
            s.spawn(|| {
                loop {
                    let next = queue.lock().unwrap().next();
                    let Some(item) = next else { break };
                    let out = f(item);
                    results.lock().unwrap().push(out);
                }
            });
        }
    });
    results.into_inner().unwrap()
}

fn move_selection(cur: Option<usize>, len: usize, delta: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let next = match (cur, delta) {
        (None, _) => 0,
        (Some(i), d) if d < 0 => i.saturating_sub((-d) as usize),
        (Some(i), d) => (i + d as usize).min(len - 1),
    };
    Some(next)
}

fn centered_rect(percent_x: u16, height: u16, r: Rect) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(height),
            Constraint::Fill(1),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1])[1]
}

fn install_state_key(repo: &str, tag_name: &str) -> String {
    if let Some((prefix, _)) = tag_name.split_once('/') {
        format!("{repo}#{prefix}")
    } else {
        repo.to_string()
    }
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 || text.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let word_len = word.chars().count();
        let cur_len = current.chars().count();
        if current.is_empty() {
            current.push_str(word);
        } else if cur_len + 1 + word_len <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current);
            current = word.to_string();
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn key_hints_line(hints: &[(&str, &str)]) -> Line<'static> {
    let gold = Style::default()
        .fg(Color::Rgb(255, 209, 0))
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::Rgb(150, 150, 165));
    let mut spans = Vec::with_capacity(hints.len() * 3);
    for (i, (key, label)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", dim));
        }
        spans.push(Span::styled(format!("[{key}]"), gold));
        spans.push(Span::styled(format!(" {label}"), dim));
    }
    Line::from(spans)
}

fn draw(f: &mut Frame, app: &mut App) {
    let gold = Color::Rgb(255, 209, 0);
    let arcane = Color::Rgb(105, 204, 240);
    let sel_bg = Color::Rgb(28, 52, 84);
    let bronze = Color::Rgb(130, 115, 85);
    let dim = Color::Rgb(140, 140, 155);
    let emerald = Color::Rgb(64, 255, 128);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Min(6),
        ])
        .split(f.area());

    // Header: Tabs + WoW Path
    let header_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(46), Constraint::Min(20)])
        .split(chunks[0]);

    let installed_tab = format!("1: Installed ({})", app.installed_total_len());
    let search_tab = if app.search_results.is_empty() {
        "2: Search".to_string()
    } else {
        format!("2: Search ({})", app.search_results.len())
    };
    let tab_idx = match app.tab {
        Tab::Installed => 0,
        Tab::Search => 1,
    };
    let tabs = Tabs::new(vec![installed_tab, search_tab])
        .select(tab_idx)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(gold))
                .title(Span::styled(
                    " ⚔ WOW TUX FORGE ",
                    Style::default().fg(gold).add_modifier(Modifier::BOLD),
                )),
        )
        .style(Style::default().fg(dim))
        .highlight_style(Style::default().fg(gold).add_modifier(Modifier::BOLD));
    f.render_widget(tabs, header_chunks[0]);

    let path_label = if app.config.addons_path.is_empty() {
        "Not configured — press [c] to set path"
    } else {
        &app.config.addons_path
    };
    let inner_w = header_chunks[1].width.saturating_sub(2) as usize;
    let path_display = if inner_w > 1 && path_label.chars().count() > inner_w {
        let tail: String = path_label
            .chars()
            .rev()
            .take(inner_w - 1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    } else {
        path_label.to_string()
    };
    let path_box = Paragraph::new(Line::from(Span::styled(
        path_display,
        Style::default().fg(if app.config.addons_path.is_empty() {
            Color::Red
        } else {
            arcane
        }),
    )))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(bronze))
            .title(Span::styled(" WoW AddOns Directory ", Style::default().fg(dim))),
    );
    f.render_widget(path_box, header_chunks[1]);

    // Main body
    match app.tab {
        Tab::Installed => {
            let mut rows: Vec<Row> = Vec::new();
            for (repo, info) in &app.state.addons {
                let status_cell = match app.latest_tags.get(repo) {
                    Some(Ok(latest)) if latest != &info.tag => Cell::from(Span::styled(
                        format!("⬆ Update: {latest}"),
                        Style::default().fg(emerald).add_modifier(Modifier::BOLD),
                    )),
                    Some(Ok(_)) => {
                        Cell::from(Span::styled("✓ Up to date", Style::default().fg(dim)))
                    }
                    Some(Err(e)) => {
                        Cell::from(Span::styled(format!("✖ {e}"), Style::default().fg(Color::Red)))
                    }
                    None => Cell::from(Span::styled("• GitHub (not checked)", Style::default().fg(dim))),
                };
                let installed_ago = info
                    .installed_at
                    .map(relative_unix_ago)
                    .unwrap_or_else(|| "—".to_string());
                rows.push(Row::new(vec![
                    Cell::from(Span::styled(
                        repo.as_str(),
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(info.tag.as_str(), Style::default().fg(arcane))),
                    Cell::from(Span::styled(installed_ago, Style::default().fg(dim))),
                    status_cell,
                ]));
            }

            for local in &app.local_addons {
                let ver = local.version.as_deref().unwrap_or("-");
                let status_cell = match &local.github_hint {
                    Some(repo) => Cell::from(Span::styled(
                        format!("◈ Local -> {repo} ([l] link)"),
                        Style::default().fg(gold),
                    )),
                    None => Cell::from(Span::styled(
                        "○ Local ([Enter] search GitHub)",
                        Style::default().fg(arcane),
                    )),
                };
                rows.push(Row::new(vec![
                    Cell::from(Span::raw(local.folder.as_str())),
                    Cell::from(Span::styled(ver, Style::default().fg(dim))),
                    Cell::from(Span::styled("—", Style::default().fg(dim))),
                    status_cell,
                ]));
            }

            if rows.is_empty() {
                rows.push(Row::new(vec![
                    Cell::from(Span::styled(
                        "No addons found. Press [Tab] or [2] to search GitHub.",
                        Style::default().fg(dim),
                    )),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                ]));
            }

            let header = Row::new(vec!["Addon / Repository", "Version", "Installed", "Status / Source"])
                .style(Style::default().fg(gold).add_modifier(Modifier::BOLD))
                .bottom_margin(1);

            let table = Table::new(
                rows,
                [
                    Constraint::Percentage(34),
                    Constraint::Percentage(18),
                    Constraint::Length(11),
                    Constraint::Percentage(37),
                ],
            )
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(bronze))
                    .title(Span::styled(" Installed Addons ", Style::default().fg(gold))),
            )
            .row_highlight_style(
                Style::default()
                    .bg(sel_bg)
                    .fg(gold)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");

            f.render_stateful_widget(table, chunks[2], &mut app.installed_list_state);
        }
        Tab::Search => {
            let search_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(3)])
                .split(chunks[2]);

            let input_focused = app.search_focus == SearchFocus::Input;
            let input_text = if app.search_query.is_empty() {
                Span::styled(
                    "Type addon name or owner/repo and press Enter...",
                    Style::default().fg(dim),
                )
            } else {
                Span::styled(
                    app.search_query.as_str(),
                    Style::default().fg(Color::White),
                )
            };
            let input = Paragraph::new(Line::from(input_text)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(if input_focused { gold } else { bronze }))
                    .title(Span::styled(
                        " GitHub Search ",
                        Style::default().fg(if input_focused { gold } else { dim }),
                    )),
            );
            f.render_widget(input, search_chunks[0]);

            let results_focused = app.search_focus == SearchFocus::Results;
            let desc_width = ((search_chunks[1].width.saturating_sub(6)) as usize * 51 / 100).max(20);
            let rows: Vec<Row> = app
                .search_results
                .iter()
                .map(|r| {
                    let desc = r.description.as_deref().unwrap_or("");
                    let wrapped = wrap_text(desc, desc_width);
                    let height = wrapped.len().max(1) as u16;
                    let desc_text = Text::from(
                        wrapped
                            .into_iter()
                            .map(|l| Line::from(Span::styled(l, Style::default().fg(dim))))
                            .collect::<Vec<_>>(),
                    );
                    let updated = r
                        .pushed_at
                        .as_deref()
                        .map(relative_time_ago)
                        .unwrap_or_else(|| "—".to_string());
                    Row::new(vec![
                        Cell::from(Span::styled(
                            r.full_name.as_str(),
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        )),
                        Cell::from(Span::styled(
                            format!("★ {}", r.stargazers_count),
                            Style::default().fg(gold),
                        )),
                        Cell::from(Span::styled(updated, Style::default().fg(arcane))),
                        Cell::from(desc_text),
                    ])
                    .height(height)
                })
                .collect();

            let header = Row::new(vec!["Repository", "Stars", "Updated", "Description"])
                .style(Style::default().fg(gold).add_modifier(Modifier::BOLD))
                .bottom_margin(1);

            let table = Table::new(
                rows,
                [
                    Constraint::Percentage(30),
                    Constraint::Length(9),
                    Constraint::Length(10),
                    Constraint::Percentage(51),
                ],
            )
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(if results_focused { gold } else { bronze }))
                    .title(Span::styled(
                        " Search Results ",
                        Style::default().fg(if results_focused { gold } else { dim }),
                    )),
            )
            .row_highlight_style(
                Style::default()
                    .bg(sel_bg)
                    .fg(gold)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
            f.render_stateful_widget(table, search_chunks[1], &mut app.search_list_state);
        }
    }

    // Footer
    let hints = match app.tab {
        Tab::Installed => key_hints_line(&[
            ("u", "Update All"),
            ("l", "Link GitHub"),
            ("a", "Security Audit"),
            ("s", "Scan"),
            ("d", "Remove"),
            ("c", "Path"),
            ("Tab", "Search"),
            ("q", "Quit"),
        ]),
        Tab::Search => key_hints_line(&[
            ("Enter", "Search / Install"),
            ("/", "Focus Search"),
            ("c", "Path"),
            ("Tab", "Installed"),
            ("q", "Quit"),
        ]),
    };

    let status_line = if app.busy {
        Line::from(vec![
            Span::styled(
                "⚡ Working... ",
                Style::default().fg(gold).add_modifier(Modifier::BOLD),
            ),
            Span::styled(app.status.as_str(), Style::default().fg(Color::White)),
        ])
    } else {
        Line::from(Span::styled(
            app.status.as_str(),
            Style::default().fg(Color::White),
        ))
    };

    let status_bar = Paragraph::new(vec![status_line, hints]).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(bronze)),
    );
    f.render_widget(status_bar, chunks[1]);

    // Path editor modal
    if app.editing_path {
        let area = centered_rect(75, 6, f.area());
        f.render_widget(Clear, area);
        let p = Paragraph::new(app.path_input.as_str())
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::White))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(gold))
                    .title(Span::styled(
                        " WoW Interface/AddOns Path ([Enter] Save, [Esc] Cancel) ",
                        Style::default().fg(gold).add_modifier(Modifier::BOLD),
                    )),
            );
        f.render_widget(p, area);
    }

    // Asset picker modal
    if let Some(picker) = &mut app.asset_picker {
        let height = (picker.options.len() as u16 + 2).clamp(5, 16);
        let area = centered_rect(76, height, f.area());
        f.render_widget(Clear, area);
        let items: Vec<ListItem> = picker
            .options
            .iter()
            .zip(picker.selected.iter())
            .map(|(opt, &checked)| {
                let sz = format_size(opt.asset.size);
                let mark = if checked { "[x]" } else { "[ ]" };
                let mark_style = if checked {
                    Style::default().fg(emerald).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(dim)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{mark} "), mark_style),
                    Span::raw(format!(
                        "[{}] {} ({sz}, {} downloads)",
                        opt.tag_name, opt.asset.name, opt.asset.download_count
                    )),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(gold))
                    .title(Span::styled(
                        format!(
                            " {} — [Space] Toggle, [a] All, [Enter] Install, [Esc] Cancel ",
                            picker.repo
                        ),
                        Style::default().fg(gold).add_modifier(Modifier::BOLD),
                    )),
            )
            .highlight_style(
                Style::default()
                    .bg(sel_bg)
                    .fg(gold)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, area, &mut picker.state);
    }

    // Security audit modal
    if let Some(modal) = &mut app.audit_modal {
        let count = modal.report.findings.len();
        let (threat_label, border_col) = if count > 0 {
            ("HIGH", Color::Red)
        } else {
            ("NONE (Clean)", emerald)
        };

        let area_w = (f.area().width * 84 / 100).max(40);
        let wrap_w = area_w.saturating_sub(10) as usize;
        let total_lines: usize = modal
            .report
            .findings
            .iter()
            .map(|x| 1 + wrap_text(&x.explanation, wrap_w).len())
            .sum();
        let height = (total_lines as u16 + 3).clamp(5, 22);
        let area = centered_rect(84, height, f.area());
        f.render_widget(Clear, area);

        if let Some(snip) = &modal.snippet {
            let snip_h = (snip.lines.len() as u16 + 2).clamp(6, 20);
            let snip_area = centered_rect(86, snip_h, f.area());
            f.render_widget(Clear, snip_area);
            let text_lines: Vec<Line> = snip
                .lines
                .iter()
                .map(|(n, line)| {
                    let is_target = *n == snip.target_line;
                    let prefix = if is_target { "▶ " } else { "  " };
                    let num_style = Style::default().fg(if is_target { Color::Red } else { dim });
                    let code_style = if is_target {
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    Line::from(vec![
                        Span::styled(format!("{prefix}{n:>4} | "), num_style),
                        Span::styled(line.as_str(), code_style),
                    ])
                })
                .collect();
            let p = Paragraph::new(text_lines)
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(gold))
                        .title(Span::styled(
                            format!(" {} ([Esc/Enter] Back) ", snip.title),
                            Style::default().fg(gold).add_modifier(Modifier::BOLD),
                        )),
                );
            f.render_widget(p, snip_area);
        } else if modal.report.findings.is_empty() {
            let p = Paragraph::new(Line::from(Span::styled(
                "✓ Threat: NONE — no executables, obfuscated Lua, or sensitive mail/trade/gold calls found.",
                Style::default().fg(emerald),
            )))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(border_col))
                    .title(Span::styled(
                        format!(
                            " Security Audit: {} | Threat: {threat_label} ([Esc/Enter] Close) ",
                            modal.title
                        ),
                        Style::default().fg(border_col).add_modifier(Modifier::BOLD),
                    )),
            );
            f.render_widget(p, area);
        } else {
            let items: Vec<ListItem> = modal
                .report
                .findings
                .iter()
                .map(|finding| {
                    let (badge, col) = ("[HIGH]", Color::Red);
                    let mut lines = vec![Line::from(vec![
                        Span::styled(
                            format!("{badge} "),
                            Style::default().fg(col).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{} — ", finding.location),
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(finding.reason.as_str(), Style::default().fg(gold)),
                    ])];
                    for (i, wline) in wrap_text(&finding.explanation, wrap_w).into_iter().enumerate() {
                        let prefix = if i == 0 { "   ↳ " } else { "     " };
                        lines.push(Line::from(Span::styled(
                            format!("{prefix}{wline}"),
                            Style::default().fg(dim),
                        )));
                    }
                    ListItem::new(Text::from(lines))
                })
                .collect();
            let modal_title = if modal.pending_install.is_some() {
                format!(
                    " ⚠ Security Audit: {} | {count} finding(s) — [y] Install Anyway, [Enter] View Code, [n/Esc] Cancel ",
                    modal.title
                )
            } else {
                format!(
                    " Security Audit: {} | Threat: {threat_label} ({count} finding(s) — [Enter] View Code, [Esc] Close) ",
                    modal.title
                )
            };
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(border_col))
                        .title(Span::styled(
                            modal_title,
                            Style::default().fg(border_col).add_modifier(Modifier::BOLD),
                        )),
                )
                .highlight_style(Style::default().bg(sel_bg).add_modifier(Modifier::BOLD))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, area, &mut modal.state);
        }
    }

    // Delete confirmation modal
    if let Some(confirm) = &app.confirm_delete {
        let area = centered_rect(70, 6, f.area());
        f.render_widget(Clear, area);
        let folders_list = confirm.folders.join(", ");
        let p = Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Delete {}?", confirm.label),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("Folders: {folders_list}"),
                Style::default().fg(dim),
            )),
            Line::from(vec![
                Span::styled("[y / Enter]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::raw(" Delete   "),
                Span::styled("[n / Esc]", Style::default().fg(gold).add_modifier(Modifier::BOLD)),
                Span::raw(" Cancel"),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Red))
                .title(Span::styled(
                    " Confirm Deletion ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
        );
        f.render_widget(p, area);
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

fn main() -> io::Result<()> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    while !app.should_quit {
        app.poll_bg();
        terminal.draw(|f| draw(f, &mut app))?;

        if event::poll(Duration::from_millis(80))?
            && let Event::Key(key) = event::read()?
        {
            app.handle_key(key);
        }
    }

    restore_terminal();
    Ok(())
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn folder_mtime_unix(addons_dir: &Path, folders: &[String]) -> Option<u64> {
    folders
        .iter()
        .filter_map(|f| std::fs::metadata(addons_dir.join(f)).ok())
        .filter_map(|m| m.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .max()
}

fn format_relative_diff(diff: i64) -> String {
    let diff = diff.max(0);
    if diff < 3600 {
        format!("{}m ago", (diff / 60).max(1))
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 86400 * 30 {
        format!("{}d ago", diff / 86400)
    } else if diff < 86400 * 365 {
        format!("{}mo ago", diff / (86400 * 30))
    } else {
        format!("{}y ago", diff / (86400 * 365))
    }
}

fn relative_unix_ago(ts: u64) -> String {
    let now = now_unix_secs() as i64;
    format_relative_diff(now - ts as i64)
}

fn relative_time_ago(iso: &str) -> String {
    relative_time_ago_from(iso, now_unix_secs() as i64)
}

fn relative_time_ago_from(iso: &str, now_secs: i64) -> String {
    let Some((date, time)) = iso.trim_end_matches('Z').split_once('T') else {
        return "—".to_string();
    };
    let mut d = date.split('-');
    let mut t = time.split(':');
    let (Some(y), Some(m), Some(day)) = (
        d.next().and_then(|s| s.parse::<i64>().ok()),
        d.next().and_then(|s| s.parse::<i64>().ok()),
        d.next().and_then(|s| s.parse::<i64>().ok()),
    ) else {
        return "—".to_string();
    };
    let (Some(hh), Some(mm), Some(ss)) = (
        t.next().and_then(|s| s.parse::<i64>().ok()),
        t.next().and_then(|s| s.parse::<i64>().ok()),
        t.next().and_then(|s| s.parse::<i64>().ok()),
    ) else {
        return "—".to_string();
    };

    // Howard Hinnant's days-from-civil algorithm (days since 1970-01-01).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let ts = days * 86400 + hh * 3600 + mm * 60 + ss;

    format_relative_diff(now_secs - ts)
}

#[cfg(test)]
mod tests {
    use super::{
        install_state_key, load_finding_snippet, move_selection, relative_time_ago_from, wrap_text,
    };

    #[test]
    fn formats_relative_time_ago() {
        // 2026-11-22T12:00:00Z
        let now = 1_795_348_800;
        assert_eq!(relative_time_ago_from("2026-11-22T11:30:00Z", now), "30m ago");
        assert_eq!(relative_time_ago_from("2026-11-22T09:00:00Z", now), "3h ago");
        assert_eq!(relative_time_ago_from("2026-11-19T12:00:00Z", now), "3d ago");
        assert_eq!(relative_time_ago_from("2026-09-22T12:00:00Z", now), "2mo ago");
        assert_eq!(relative_time_ago_from("2024-11-22T12:00:00Z", now), "2y ago");
    }

    #[test]
    fn builds_state_keys_and_wraps_text() {
        assert_eq!(install_state_key("Questie/Questie", "v1.0"), "Questie/Questie");
        assert_eq!(
            install_state_key("Questie/Questie", "bundle/v12"),
            "Questie/Questie#bundle"
        );
        assert_eq!(wrap_text("hello world test", 11), vec!["hello world", "test"]);
        assert_eq!(move_selection(None, 3, 1), Some(0));
        assert_eq!(move_selection(Some(2), 3, 1), Some(2));
        assert_eq!(move_selection(Some(0), 3, -1), Some(0));
    }

    #[test]
    fn loads_finding_snippet_with_context() {
        let tmp = std::env::temp_dir().join(format!("wow-tux-forge-snip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("MyAddon")).unwrap();
        let content = (1..=15)
            .map(|i| format!("local line_{i} = {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tmp.join("MyAddon/Core.lua"), content).unwrap();

        let snip = load_finding_snippet(&tmp, "MyAddon/Core.lua:10").unwrap();
        assert_eq!(snip.target_line, 10);
        assert_eq!(snip.lines.first().unwrap().0, 4);
        assert_eq!(snip.lines.last().unwrap().0, 15);
        assert!(load_finding_snippet(&tmp, "MyAddon/payload.exe").is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
