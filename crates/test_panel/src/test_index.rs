//! A project-wide index of tests, built by parsing files with tree-sitter in the
//! background and matching the languages' runnable queries, the same way the editor
//! discovers the tests of an open file.
//!
//! The index only speeds things up: it is cached under Zed's cache directory, outside the
//! project, and a missing, stale or corrupt cache is simply rebuilt. Files are re-indexed
//! when their size or modification time changes.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use fs::MTime;
use gpui::{AppContext as _, Context, Entity, EventEmitter, SharedString, Task};
use language::{Buffer, BufferSnapshot, Language, LanguageRegistry, Point};
use project::{Project, ProjectPath, WorktreeId};
use rope::Rope;
use serde::{Deserialize, Serialize};
use util::{ResultExt as _, rel_path::RelPath};
use worktree::{Entry, EntryKind, PathChange};

const CACHE_VERSION: u32 = 1;
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
/// How many files are indexed between refreshes of the panel.
const PROGRESS_BATCH: usize = 25;

/// Markers that, when present in the declaration of a test, indicate that the test
/// runner will skip it. Checked against the test's attribute or decorator lines and
/// the line declaring the test itself.
const IGNORED_TEST_MARKERS: &[&str] = &[
    "#[ignore",
    ".skip",
    ".todo",
    "xit(",
    "xtest(",
    "xdescribe(",
    "@pytest.mark.skip",
    "@unittest.skip",
    "@skip",
    "@Disabled",
    "@Ignore",
];

/// Runnable captures that hold the name of the test, most specific first.
const NAME_CAPTURES: &[&str] = &[
    "_test_name",
    "_subtest_name",
    "_table_test_case_name",
    "_pytest_method_name",
    "_unittest_method_name",
    "DENO_TEST_NAME",
    "_pytest_class_name",
    "_unittest_class_name",
    "_doc_test_name",
];

pub fn tag_is_test(tag: &str) -> bool {
    tag.to_lowercase().contains("test")
}

/// The name a test is listed under: the name captured by the language's runnables
/// query when there is one, otherwise the enclosing outline symbol, otherwise the line
/// the runnable starts on.
pub fn test_name(
    snapshot: &BufferSnapshot,
    row: u32,
    column: u32,
    extra_variables: &HashMap<String, String>,
) -> SharedString {
    for capture in NAME_CAPTURES {
        if let Some(value) = extra_variables.get(*capture) {
            let value = value
                .trim()
                .trim_matches(|c| c == '"' || c == '\'' || c == '`');
            if !value.is_empty() {
                return value.to_owned().into();
            }
        }
    }
    let position = Point::new(row, column);
    if let Some(symbol) = snapshot.symbols_containing(position, None).pop() {
        let name_range = symbol
            .name_ranges
            .last()
            .cloned()
            .unwrap_or(0..symbol.text.len());
        if let Some(name) = symbol.text.get(name_range) {
            return name.to_owned().into();
        }
        return symbol.text;
    }
    line_text(snapshot, row).trim().to_string().into()
}

fn line_text(snapshot: &BufferSnapshot, row: u32) -> String {
    snapshot
        .text_for_range(Point::new(row, 0)..Point::new(row, snapshot.line_len(row)))
        .collect()
}

/// Detects tests that their runner will skip, by looking for well-known markers in the
/// attribute or decorator lines preceding the test and in the line declaring it.
pub fn is_ignored_test(snapshot: &BufferSnapshot, row: u32, context_start: usize) -> bool {
    let mut first_row = snapshot.offset_to_point(context_start).row.min(row);
    while first_row > 0 {
        let previous_line = line_text(snapshot, first_row - 1);
        let previous_line = previous_line.trim_start();
        if previous_line.starts_with('@') || previous_line.starts_with("#[") {
            first_row -= 1;
        } else {
            break;
        }
    }
    let declaration: String = snapshot
        .text_for_range(Point::new(first_row, 0)..Point::new(row, snapshot.line_len(row)))
        .collect();
    IGNORED_TEST_MARKERS
        .iter()
        .any(|marker| declaration.contains(marker))
}

/// Extracts the tests of a parsed file.
pub fn tests_in_snapshot(snapshot: &BufferSnapshot) -> Vec<IndexedTest> {
    let mut tests = Vec::new();
    for range in snapshot.runnable_ranges(0..snapshot.len()) {
        let tags: Vec<String> = range
            .runnable
            .tags
            .iter()
            .map(|tag| tag.0.to_string())
            .collect();
        if !tags.iter().any(|tag| tag_is_test(tag)) {
            continue;
        }
        let point = snapshot.offset_to_point(range.run_range.start);
        let extra_variables: HashMap<String, String> = range.extra_captures.into_iter().collect();
        tests.push(IndexedTest {
            name: test_name(snapshot, point.row, point.column, &extra_variables).to_string(),
            row: point.row,
            column: point.column,
            context_range: range.full_range.clone(),
            ignored: is_ignored_test(snapshot, point.row, range.full_range.start),
            tags,
            extra_variables,
        });
    }
    tests
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedTest {
    pub name: String,
    pub row: u32,
    pub column: u32,
    pub context_range: Range<usize>,
    pub ignored: bool,
    pub tags: Vec<String>,
    pub extra_variables: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct IndexedFile {
    pub project_path: ProjectPath,
    pub abs_path: PathBuf,
    /// The path shown to the user, including the worktree's root name.
    pub display_path: SharedString,
    pub mtime: Option<MTime>,
    pub size: u64,
    pub tests: Vec<IndexedTest>,
}

#[derive(Serialize, Deserialize)]
struct CachedFile {
    path: String,
    mtime: Option<MTime>,
    size: u64,
    tests: Vec<IndexedTest>,
}

#[derive(Serialize, Deserialize)]
struct CachedWorktree {
    version: u32,
    files: Vec<CachedFile>,
}

pub enum TestIndexEvent {
    Updated,
    /// A full scan of a worktree finished.
    ScanCompleted,
}

pub struct TestIndex {
    project: Entity<Project>,
    files: HashMap<ProjectPath, IndexedFile>,
    /// Languages by file extension, `None` when the language has no runnables query.
    languages_by_extension: HashMap<String, Option<Arc<Language>>>,
    /// The full scans in progress; incremental scans of changed paths run detached.
    scan_tasks: HashMap<WorktreeId, Task<()>>,
    scanning: HashSet<WorktreeId>,
    save_tasks: HashMap<WorktreeId, Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<TestIndexEvent> for TestIndex {}

impl TestIndex {
    pub fn new(project: Entity<Project>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.subscribe(&project, |this, _, event, cx| match event {
            project::Event::WorktreeAdded(worktree_id) => {
                this.scan_worktree(*worktree_id, None, cx)
            }
            project::Event::WorktreeRemoved(worktree_id) => {
                let worktree_id = *worktree_id;
                this.files.retain(|path, _| path.worktree_id != worktree_id);
                this.scan_tasks.remove(&worktree_id);
                this.scanning.remove(&worktree_id);
                cx.emit(TestIndexEvent::Updated);
            }
            project::Event::WorktreeUpdatedEntries(worktree_id, changes) => {
                let mut changed: Vec<Arc<RelPath>> = Vec::new();
                let mut removed = false;
                for (path, _, change) in changes.iter() {
                    match change {
                        PathChange::Removed => {
                            removed |= this
                                .files
                                .remove(&ProjectPath {
                                    worktree_id: *worktree_id,
                                    path: path.clone(),
                                })
                                .is_some();
                        }
                        PathChange::Added
                        | PathChange::Updated
                        | PathChange::AddedOrUpdated
                        | PathChange::Loaded => changed.push(path.clone()),
                    }
                }
                if removed {
                    cx.emit(TestIndexEvent::Updated);
                    this.schedule_save(*worktree_id, cx);
                }
                if !changed.is_empty() {
                    this.scan_worktree(*worktree_id, Some(changed), cx);
                }
            }
            _ => {}
        });

        let mut this = Self {
            project: project.clone(),
            files: HashMap::default(),
            languages_by_extension: HashMap::default(),
            scan_tasks: HashMap::default(),
            scanning: HashSet::default(),
            save_tasks: HashMap::default(),
            _subscriptions: vec![subscription],
        };
        let worktree_ids: Vec<WorktreeId> = project
            .read(cx)
            .worktrees(cx)
            .map(|worktree| worktree.read(cx).id())
            .collect();
        for worktree_id in worktree_ids {
            this.scan_worktree(worktree_id, None, cx);
        }
        this
    }

    pub fn files(&self) -> impl Iterator<Item = &IndexedFile> {
        self.files.values()
    }

    /// Indexes the given paths of a worktree, or the whole worktree when `paths` is
    /// `None`. A full scan first restores the cached index so that unchanged files do
    /// not need to be parsed again.
    fn scan_worktree(
        &mut self,
        worktree_id: WorktreeId,
        paths: Option<Vec<Arc<RelPath>>>,
        cx: &mut Context<Self>,
    ) {
        let Some(worktree) = self.project.read(cx).worktree_for_id(worktree_id, cx) else {
            return;
        };
        let worktree = worktree.read(cx);
        if !worktree.is_local() {
            return;
        }
        let snapshot = worktree.snapshot();
        let fs = self.project.read(cx).fs().clone();
        let languages = self.project.read(cx).languages().clone();
        let is_full_scan = paths.is_none();
        if is_full_scan {
            self.files.retain(|path, _| path.worktree_id != worktree_id);
            self.scanning.insert(worktree_id);
        }

        let task = cx.spawn(async move |this, cx| {
            let cache_path = cache_path_for_worktree(snapshot.abs_path());
            let mut cached: HashMap<String, CachedFile> = HashMap::default();
            if is_full_scan {
                cached = load_cache(&cache_path).await.unwrap_or_default();
            }

            let entries: Vec<Entry> = match &paths {
                Some(paths) => paths
                    .iter()
                    .filter_map(|path| snapshot.entry_for_path(path).cloned())
                    .collect(),
                None => snapshot.files(false, 0).cloned().collect(),
            };

            let mut processed = 0;
            for entry in entries {
                if entry.kind != EntryKind::File || entry.is_external || entry.is_ignored {
                    continue;
                }
                let abs_path = snapshot.abs_path().join(entry.path.as_std_path());
                let Some(language) =
                    Self::language_for_path(&this, &languages, &abs_path, cx).await
                else {
                    continue;
                };
                let project_path = ProjectPath {
                    worktree_id,
                    path: entry.path.clone(),
                };
                let unchanged = this
                    .read_with(cx, |this, _| {
                        this.files.get(&project_path).is_some_and(|indexed| {
                            indexed.mtime == entry.mtime && indexed.size == entry.size
                        })
                    })
                    .unwrap_or(false);
                if unchanged {
                    continue;
                }
                let display_path: SharedString =
                    format!("{}/{}", snapshot.root_name_str(), entry.path.as_unix_str()).into();

                let cached_file = cached
                    .remove(entry.path.as_unix_str())
                    .filter(|cached| cached.mtime == entry.mtime && cached.size == entry.size);
                let tests = match cached_file {
                    Some(cached) => cached.tests,
                    None => {
                        match Self::index_file(&fs, &abs_path, language, &languages, cx).await {
                            Ok(tests) => tests,
                            Err(error) => {
                                log::debug!("failed to index tests in {abs_path:?}: {error:#}");
                                Vec::new()
                            }
                        }
                    }
                };

                let file = IndexedFile {
                    project_path: project_path.clone(),
                    abs_path,
                    display_path,
                    mtime: entry.mtime,
                    size: entry.size,
                    tests,
                };
                processed += 1;
                let emit = processed % PROGRESS_BATCH == 0;
                this.update(cx, |this, cx| {
                    if file.tests.is_empty() {
                        this.files.remove(&project_path);
                    } else {
                        this.files.insert(project_path, file);
                    }
                    if emit {
                        cx.emit(TestIndexEvent::Updated);
                    }
                })
                .ok();
            }

            this.update(cx, |this, cx| {
                cx.emit(TestIndexEvent::Updated);
                if is_full_scan {
                    this.scanning.remove(&worktree_id);
                    cx.emit(TestIndexEvent::ScanCompleted);
                }
                this.schedule_save(worktree_id, cx);
            })
            .ok();
        });
        // Only the full scan is kept, so that a worktree's removal cancels it. Incremental
        // scans must not replace (and thereby cancel) it or each other.
        if is_full_scan {
            self.scan_tasks.insert(worktree_id, task);
        } else {
            task.detach();
        }
    }

    /// Whether a full scan of some worktree is still in progress.
    pub fn is_scanning(&self) -> bool {
        !self.scanning.is_empty()
    }

    async fn language_for_path(
        this: &gpui::WeakEntity<Self>,
        languages: &Arc<LanguageRegistry>,
        abs_path: &Path,
        cx: &mut gpui::AsyncApp,
    ) -> Option<Arc<Language>> {
        let extension = abs_path
            .extension()
            .map(|extension| extension.to_string_lossy().into_owned())
            .unwrap_or_default();
        let cached = this
            .read_with(cx, |this, _| {
                this.languages_by_extension.get(&extension).cloned()
            })
            .ok()?;
        if let Some(language) = cached {
            return language;
        }
        let language = languages
            .load_language_for_file_path(abs_path)
            .await
            .ok()
            .filter(|language| {
                language
                    .grammar()
                    .is_some_and(|grammar| grammar.runnable_config.is_some())
            });
        this.update(cx, |this, _| {
            this.languages_by_extension
                .insert(extension, language.clone());
        })
        .ok();
        language
    }

    async fn index_file(
        fs: &Arc<dyn fs::Fs>,
        abs_path: &Path,
        language: Arc<Language>,
        languages: &Arc<LanguageRegistry>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<Vec<IndexedTest>> {
        let text = fs
            .load(abs_path)
            .await
            .with_context(|| format!("loading {abs_path:?}"))?;
        let rope = Rope::from(text);
        let languages = languages.clone();
        let snapshot =
            cx.update(|cx| Buffer::build_snapshot(rope, Some(language), Some(languages), None, cx));
        let tests = cx
            .background_spawn(async move {
                let snapshot = snapshot.await;
                tests_in_snapshot(&snapshot)
            })
            .await;
        Ok(tests)
    }

    fn schedule_save(&mut self, worktree_id: WorktreeId, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let Ok(Some((cache_path, cache))) = this.read_with(cx, |this, cx| {
                let worktree = this.project.read(cx).worktree_for_id(worktree_id, cx)?;
                let cache_path = cache_path_for_worktree(&worktree.read(cx).abs_path());
                let files = this
                    .files
                    .values()
                    .filter(|file| file.project_path.worktree_id == worktree_id)
                    .map(|file| CachedFile {
                        path: file.project_path.path.as_unix_str().to_owned(),
                        mtime: file.mtime,
                        size: file.size,
                        tests: file.tests.clone(),
                    })
                    .collect();
                Some((
                    cache_path,
                    CachedWorktree {
                        version: CACHE_VERSION,
                        files,
                    },
                ))
            }) else {
                return;
            };
            cx.background_spawn(async move {
                save_cache(&cache_path, &cache).await.log_err();
            })
            .await;
        });
        self.save_tasks.insert(worktree_id, task);
    }
}

fn cache_path_for_worktree(abs_path: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    abs_path.hash(&mut hasher);
    let name = abs_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_owned());
    paths::temp_dir()
        .join("test_index")
        .join(format!("{name}-{:016x}.json", hasher.finish()))
}

async fn load_cache(path: &Path) -> Option<HashMap<String, CachedFile>> {
    let contents = smol::fs::read_to_string(path).await.ok()?;
    let cache: CachedWorktree = serde_json::from_str(&contents).ok()?;
    if cache.version != CACHE_VERSION {
        return None;
    }
    Some(
        cache
            .files
            .into_iter()
            .map(|file| (file.path.clone(), file))
            .collect(),
    )
}

async fn save_cache(path: &Path, cache: &CachedWorktree) -> Result<()> {
    if let Some(parent) = path.parent() {
        smol::fs::create_dir_all(parent).await?;
    }
    let contents = serde_json::to_string(cache)?;
    let temporary = path.with_extension("json.tmp");
    smol::fs::write(&temporary, contents).await?;
    smol::fs::rename(&temporary, path).await?;
    Ok(())
}
