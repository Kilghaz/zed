mod test_index;
mod test_panel_settings;
mod test_report;

use std::{
    ops::Range, path::PathBuf, process::ExitStatus, sync::Arc, time::Duration, time::Instant,
};

use anyhow::Result;
use chrono::{DateTime, Local};
use collections::{HashMap, HashSet};
use editor::{
    Editor, RunnableEntry, RunnableTaskStatus, RunnableTasks, SelectionEffects, actions::MoveToEnd,
    scroll::Autoscroll,
};
use fs::Fs;
use gpui::{
    Action, Anchor, App, AsyncWindowContext, ClipboardItem, Context, DismissEvent, DragMoveEvent,
    Entity, EntityId, EventEmitter, FocusHandle, Focusable, MouseDownEvent, Pixels,
    Point as GpuiPoint, SharedString, Subscription, Task, UniformListScrollHandle, WeakEntity,
    Window, actions, anchored, deferred, uniform_list,
};
use language::{Point, language_settings::SoftWrap};
use project::{Location, Project, ProjectPath, TaskSourceKind};
use settings::{DockPosition as DockPositionSetting, Settings, update_settings_file};
use task::{ResolvedTask, Shell, TaskTemplate, TaskVariables, VariableName};
use terminal::Terminal;
use terminal_view::terminal_panel::prepare_task_for_spawn;
use text::BufferId;
use ui::{
    CommonAnimationExt as _, ContextMenu, Disclosure, Divider, IconButton, ListItem,
    ListItemSpacing, PopoverMenu, PopoverMenuHandle, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{
    HideStatusItem, Panel, TaskSpawnHandler, Workspace,
    dock::{DockPosition, PanelEvent},
};

use test_index::{IndexedTest, TestIndex, TestIndexEvent, is_ignored_test, tag_is_test, test_name};
pub use test_panel_settings::TestPanelSettings;
use test_report::{
    CollectedTest, EntryKind, ReportEntry, ReportFormat, Runner, TestReport, TestStatus,
};

actions!(
    test_panel,
    [
        /// Toggles focus on the test panel.
        ToggleFocus,
        /// Runs every test listed in the test panel.
        RunAllTests,
        /// Runs the tests of the most recent run again.
        RerunAllTests,
        /// Runs the tests that failed in the current run again.
        RerunFailedTests,
        /// Asks the test runners to list their tests without running them.
        DiscoverTests,
        /// Toggles whether passing tests are shown in the test panel.
        ToggleShowPassed,
        /// Toggles whether failing tests are shown in the test panel.
        ToggleShowFailed,
        /// Toggles whether ignored tests are shown in the test panel.
        ToggleShowIgnored,
        /// Expands every entry in the test panel.
        ExpandAllEntries,
        /// Collapses every entry in the test panel.
        CollapseAllEntries,
        /// Toggles between a test's own output and the full output of its run.
        ToggleFullOutput,
    ]
);

const TEST_PANEL_KEY: &str = "TestPanel";
const UPDATE_DEBOUNCE: Duration = Duration::from_millis(150);
const REPARSE_DEBOUNCE: Duration = Duration::from_millis(250);
const MAX_HISTORY: usize = 20;
const DEFAULT_LIST_WIDTH: Pixels = px(320.);
const MIN_PANE_WIDTH: Pixels = px(160.);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TestPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &RunAllTests, window, cx| {
            if let Some(panel) = workspace.panel::<TestPanel>(cx) {
                panel.update(cx, |panel, cx| panel.run_all_tests(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &RerunAllTests, window, cx| {
            if let Some(panel) = workspace.panel::<TestPanel>(cx) {
                panel.update(cx, |panel, cx| panel.rerun_all(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &RerunFailedTests, window, cx| {
            if let Some(panel) = workspace.panel::<TestPanel>(cx) {
                panel.update(cx, |panel, cx| panel.rerun_failed(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &DiscoverTests, _, cx| {
            if let Some(panel) = workspace.panel::<TestPanel>(cx) {
                panel.update(cx, |panel, cx| panel.discover_tests(cx));
            }
        });
    })
    .detach();
}

/// A dock panel listing the tests of the project as a tree, the outcome of the current
/// test run for each of them, and the output the test runner attributed to the selected
/// test.
///
/// Tests come from two sources: the runnables of the open editors, and a background
/// [`TestIndex`] of the project's files. Test tasks scheduled anywhere in the workspace
/// are routed to this panel through [`TaskSpawnHandler`]; the panel spawns them into
/// terminals it owns, parses the runner's output into a [`TestReport`] as it arrives, and
/// maps the reported tests back onto the tree.
pub struct TestPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    index: Entity<TestIndex>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    files: Vec<FileGroup>,
    runs: Vec<TestRun>,
    current_run: Option<usize>,
    open_group: Option<OpenRunGroup>,
    next_run_id: usize,
    entries: Vec<PanelEntry>,
    collapsed: HashSet<NodeKey>,
    selected: Option<Selection>,
    show_passed: bool,
    show_failed: bool,
    show_ignored: bool,
    sort_order: SortOrder,
    show_full_output: bool,
    output_editor: Entity<Editor>,
    displayed_output: Option<(Selection, bool, String)>,
    list_width: Option<Pixels>,
    context_menu: Option<(Entity<ContextMenu>, GpuiPoint<Pixels>, Subscription)>,
    sort_menu_handle: PopoverMenuHandle<ContextMenu>,
    history_menu_handle: PopoverMenuHandle<ContextMenu>,
    more_menu_handle: PopoverMenuHandle<ContextMenu>,
    editor_subscriptions: HashMap<EntityId, Subscription>,
    update_task: Task<()>,
    reparse_task: Task<()>,
    last_reparse_duration: Duration,
    /// Whether the nodes for runner-reported tests have to be rebuilt, because the tree
    /// was rebuilt or new reported tests arrived.
    reported_nodes_stale: bool,
    /// Tests the current run is expected to execute, shown as running until the runner
    /// reports them.
    pending_nodes: HashSet<NodeKey>,
    /// The last known outcome of every test, accumulated over runs: a run only replaces
    /// the results of the tests it executed.
    results: HashMap<NodeKey, NodeResult>,
    /// Tests reported by runs that are not listed statically, kept so that they stay in
    /// the tree after the run that reported them.
    reported: Vec<ReportEntry>,
    /// Tests enumerated by the runners' collection modes, on top of the static analysis.
    discovered: Vec<ReportEntry>,
    discovery_task: Task<()>,
    discovery_started: bool,
    _subscriptions: Vec<Subscription>,
}

/// Identifies a test by its file and the row where its runnable starts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TestKey {
    file: FileKey,
    row: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum NodeKey {
    Runnable(TestKey),
    /// A test only known from a runner's report, identified by its group and its path.
    Reported {
        group: SharedString,
        path: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum FileKey {
    /// A file with an open editor.
    Buffer(EntityId),
    /// A file known from the project index.
    Indexed(ProjectPath),
    /// A group synthesized for reported tests that belong to no known file.
    Synthetic(SharedString),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Selection {
    Run,
    File(FileKey),
    Node(NodeKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortOrder {
    Declaration,
    Alphabetical,
    Duration,
}

impl SortOrder {
    fn label(&self) -> &'static str {
        match self {
            SortOrder::Declaration => "By Declaration Order",
            SortOrder::Alphabetical => "Alphabetically",
            SortOrder::Duration => "By Duration",
        }
    }
}

struct FileGroup {
    key: FileKey,
    path: SharedString,
    abs_path: Option<PathBuf>,
    nodes: Vec<TestNode>,
}

/// A test with an open editor, run through the editor's runnable machinery.
#[derive(Clone, Debug)]
struct OpenSource {
    editor: Entity<Editor>,
    buffer_id: BufferId,
    tasks: Arc<RunnableTasks>,
    /// The status the editor currently shows in its gutter for this runnable.
    editor_status: Option<RunnableTaskStatus>,
}

/// A test known from the index, whose buffer is opened when it is run.
#[derive(Clone, Debug)]
struct IndexedSource {
    project_path: ProjectPath,
    test: IndexedTest,
}

#[derive(Clone, Debug)]
enum NodeSource {
    Open(OpenSource),
    Indexed(IndexedSource),
}

#[derive(Clone, Debug)]
struct TestNode {
    key: NodeKey,
    name: SharedString,
    /// The name as compared against runner reports, see [`normalize_name`].
    normalized_name: String,
    ignored: bool,
    row: u32,
    column: u32,
    context_range: Range<usize>,
    source: Option<NodeSource>,
    children: Vec<TestNode>,
    // State derived from the current run.
    status: Option<RunnableTaskStatus>,
    skipped: bool,
    duration: Option<Duration>,
    output: String,
    /// The run and process that produced the node's result.
    run_ref: Option<(usize, usize)>,
}

/// The outcome of a test in the run that last executed it.
#[derive(Clone, Debug, Default)]
struct NodeResult {
    status: Option<RunnableTaskStatus>,
    skipped: bool,
    duration: Option<Duration>,
    output: String,
    run_ref: Option<(usize, usize)>,
}

impl TestNode {
    fn new(key: NodeKey, name: SharedString, row: u32) -> Self {
        Self {
            key,
            normalized_name: normalize_name(&name),
            name,
            ignored: false,
            row,
            column: 0,
            context_range: 0..0,
            source: None,
            children: Vec::new(),
            status: None,
            skipped: false,
            duration: None,
            output: String::new(),
            run_ref: None,
        }
    }

    fn apply_result(&mut self, result: &NodeResult) {
        if result.status.is_some() {
            self.status = result.status;
        }
        self.skipped |= result.skipped;
        if result.duration.is_some() {
            self.duration = result.duration;
        }
        self.output.push_str(&result.output);
        if result.run_ref.is_some() {
            self.run_ref = result.run_ref;
        }
    }

    fn reset_run_state(&mut self, prune_reported: bool) {
        self.status = None;
        self.skipped = false;
        self.duration = None;
        self.output.clear();
        self.run_ref = None;
        if prune_reported {
            self.children.retain(|child| child.source.is_some());
        }
        for child in &mut self.children {
            child.reset_run_state(prune_reported);
        }
    }

    fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}

/// One `TestRun` groups the processes started by a single user action, such as running a
/// file or rerunning the failed tests.
struct TestRun {
    id: usize,
    label: SharedString,
    started_at: DateTime<Local>,
    processes: Vec<RunProcess>,
}

impl TestRun {
    fn status(&self) -> RunnableTaskStatus {
        if self
            .processes
            .iter()
            .any(|process| process.status == RunnableTaskStatus::Running)
        {
            RunnableTaskStatus::Running
        } else if self
            .processes
            .iter()
            .any(|process| process.status == RunnableTaskStatus::Failed)
        {
            RunnableTaskStatus::Failed
        } else {
            RunnableTaskStatus::Passed
        }
    }

    fn run_output(&self) -> String {
        let mut output = String::new();
        for process in &self.processes {
            if self.processes.len() > 1 {
                output.push_str(&format!("$ {}\n", process.task.resolved.command_label));
            }
            output.push_str(&process.report.run_output);
            output.push('\n');
        }
        output
    }
}

struct RunProcess {
    task: ResolvedTask,
    runner: Runner,
    format: ReportFormat,
    /// The listed test the task was started for, if it could be determined.
    target: Option<TestKey>,
    terminal: Option<Entity<Terminal>>,
    report: TestReport,
    status: RunnableTaskStatus,
    _subscription: Option<Subscription>,
}

/// While a user action schedules several tasks, the processes they spawn are attached to
/// the same run.
struct OpenRunGroup {
    run_id: usize,
    remaining: usize,
    opened_at: Instant,
}

#[derive(Clone, Debug)]
enum PanelEntry {
    Run,
    File { file_index: usize },
    Node { file_index: usize, path: Vec<usize> },
}

#[derive(Default)]
struct StatusSummary {
    running: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
}

impl StatusSummary {
    fn record(&mut self, node: &TestNode) {
        if node.skipped {
            self.skipped += 1;
        }
        match node.status {
            Some(RunnableTaskStatus::Running) => self.running += 1,
            Some(RunnableTaskStatus::Passed) => self.passed += 1,
            Some(RunnableTaskStatus::Failed) => self.failed += 1,
            None => {}
        }
    }
}

struct NameIndex {
    by_name: HashMap<String, Vec<IndexedNode>>,
}

struct IndexedNode {
    file_index: usize,
    ancestors: Vec<String>,
    key: NodeKey,
}

struct SelectedOutput {
    name: SharedString,
    status: Option<RunnableTaskStatus>,
    ignored: bool,
    duration: Option<Duration>,
    output: String,
    run_ref: Option<(usize, usize)>,
}

struct DraggedSplitter;

impl Render for DraggedSplitter {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

struct TestPanelTaskSpawnHandler(WeakEntity<TestPanel>);

impl TaskSpawnHandler for TestPanelTaskSpawnHandler {
    fn spawn(
        &self,
        task: &ResolvedTask,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Task<Option<Result<ExitStatus>>>> {
        if !is_test_task(task) {
            return None;
        }
        let panel = self.0.upgrade()?;
        Some(panel.update(cx, |panel, cx| {
            panel.spawn_process(task.clone(), window, cx)
        }))
    }
}

fn is_test_runnable(entry: &RunnableEntry) -> bool {
    entry
        .tasks
        .templates
        .iter()
        .any(|(_, template)| template.tags.iter().any(|tag| tag_is_test(tag)))
}

/// Whether a scheduled task is a test run: either it is bound to a test runnable, or its
/// command line mentions tests, as for `cargo test` or `pytest` task configurations.
fn is_test_task(task: &ResolvedTask) -> bool {
    task.original_task().tags.iter().any(|tag| tag_is_test(tag))
        || task.resolved.command_label.to_lowercase().contains("test")
}

/// Whether a task template runs a set of tests (a file, a package or the whole project)
/// rather than a single named test.
fn is_scoped_test_template(template: &TaskTemplate) -> bool {
    let text = format!(
        "{} {} {}",
        template.label,
        template.command,
        template.args.join(" ")
    );
    if !text.to_lowercase().contains("test")
        || text.contains("$ZED_SYMBOL")
        || text.contains("$ZED_SELECTED_TEXT")
    {
        return false;
    }
    !text
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .any(|token| {
            token.starts_with("$ZED_CUSTOM_")
                && ["TEST", "SUITE", "CASE", "DOC"]
                    .iter()
                    .any(|marker| token.contains(marker))
        })
}

fn mentions_file(template: &TaskTemplate) -> bool {
    template.args.iter().any(|arg| arg.contains("$ZED_FILE"))
}

/// The words in a template's arguments that name the runner, such as `vitest` or
/// `pytest`. Flags, variables and generic verbs do not identify a runner.
fn runner_words(template: &TaskTemplate) -> Vec<&str> {
    template
        .args
        .iter()
        .map(String::as_str)
        .filter(|arg| {
            !arg.starts_with('-')
                && !arg.contains('$')
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                && !matches!(*arg, "exec" | "run" | "test" | "tests" | "x")
        })
        .collect()
}

/// The scope of a test run started from the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunScope {
    /// The tests of one file.
    File,
    /// Every test the runner knows about.
    Project,
}

/// Picks, among the templates available for a buffer, the one that runs the given scope
/// with the same runner as the test's own templates.
fn pick_scoped_template(
    templates: &[(TaskSourceKind, TaskTemplate)],
    node_templates: &[(TaskSourceKind, TaskTemplate)],
    scope: RunScope,
) -> Option<(TaskSourceKind, TaskTemplate)> {
    // Runners invoked through the project's package manager (e.g. `npm exec -- vitest`)
    // are preferred over bare runtimes such as `node --test` when a file matches both.
    let reference = node_templates
        .iter()
        .map(|(_, template)| template)
        .max_by_key(|template| usize::from(template.command.contains("$ZED_CUSTOM_")))?;

    // A candidate has to invoke the same runner, which rules out unrelated tasks whose
    // label merely mentions tests, such as a `type-check-tests` package script.
    let reference_runner = runner_words(reference);
    let (kind, template) = templates
        .iter()
        .filter(|(_, template)| is_scoped_test_template(template))
        .filter(|(_, template)| {
            reference_runner.is_empty()
                || template
                    .args
                    .iter()
                    .any(|arg| reference_runner.contains(&arg.as_str()))
        })
        .map(|(kind, template)| {
            let mut score: i32 = template
                .args
                .iter()
                .filter(|arg| reference.args.contains(arg))
                .count() as i32;
            if reference.command == template.command {
                score += 5;
            }
            let module_wide = template.args.iter().any(|arg| arg.contains("./..."));
            match scope {
                RunScope::File => {
                    if mentions_file(template) {
                        score += 10;
                    }
                    if module_wide {
                        score -= 20;
                    }
                }
                RunScope::Project => {
                    if !mentions_file(template) {
                        score += 10;
                    }
                    if module_wide {
                        score += 5;
                    }
                }
            }
            (score, kind, template)
        })
        .max_by_key(|(score, _, _)| *score)
        .map(|(_, kind, template)| (kind.clone(), template.clone()))?;

    let mut template = template;
    if scope == RunScope::Project && mentions_file(&template) {
        // Dropping the file argument makes the runner pick up every test it knows about,
        // and the file parallelism Zed disables for single-file runs is wanted again.
        template
            .args
            .retain(|arg| !arg.contains("$ZED_FILE") && arg != "--no-file-parallelism");
        template.label = format!("{} (all)", template.label);
    }
    Some((kind, template))
}

/// The sorted runnable tags of a test, which identify the runner it is bound to.
fn runner_tags(node: &TestNode) -> Vec<String> {
    let mut tags: Vec<String> = match &node.source {
        Some(NodeSource::Open(source)) => source
            .tasks
            .templates
            .iter()
            .flat_map(|(_, template)| template.tags.iter().cloned())
            .collect(),
        Some(NodeSource::Indexed(source)) => source.test.tags.clone(),
        None => Vec::new(),
    };
    tags.sort();
    tags.dedup();
    tags
}

fn normalize_name(name: &str) -> String {
    name.trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .replace(char::is_whitespace, "_")
}

fn status_from_exit(exit_status: Option<ExitStatus>) -> RunnableTaskStatus {
    match exit_status {
        Some(exit_status) if exit_status.success() => RunnableTaskStatus::Passed,
        _ => RunnableTaskStatus::Failed,
    }
}

fn status_from_report(status: TestStatus) -> Option<RunnableTaskStatus> {
    match status {
        TestStatus::Running => Some(RunnableTaskStatus::Running),
        TestStatus::Passed => Some(RunnableTaskStatus::Passed),
        TestStatus::Failed => Some(RunnableTaskStatus::Failed),
        TestStatus::Skipped | TestStatus::NotRun => None,
    }
}

/// Logs phases of the panel's main-thread work that take long enough to be felt, so that
/// slowness with large projects can be attributed.
fn log_slow(phase: &str, started: Instant) {
    let elapsed = started.elapsed();
    if elapsed > Duration::from_millis(50) {
        log::info!("test panel: {phase} took {elapsed:?}");
    }
}

fn format_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

impl TestPanel {
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> Task<Result<Entity<Self>>> {
        cx.spawn(async move |cx| {
            let panel = workspace.update_in(cx, |workspace, window, cx| {
                let workspace_handle = cx.entity();
                cx.new(|cx| Self::new(workspace_handle, workspace, window, cx))
            })?;
            workspace.update(cx, |workspace, _| {
                workspace.set_task_spawn_handler(TestPanelTaskSpawnHandler(panel.downgrade()));
            })?;
            Ok(panel)
        })
    }

    fn new(
        workspace_handle: Entity<Workspace>,
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let workspace_subscription =
            cx.subscribe(&workspace_handle, |this, _, event, cx| match event {
                workspace::Event::ItemAdded { .. }
                | workspace::Event::ItemRemoved { .. }
                | workspace::Event::ActiveItemChanged
                | workspace::Event::PaneRemoved => this.schedule_update(cx),
                _ => {}
            });

        let project = workspace.project().clone();
        let index = cx.new(|cx| TestIndex::new(project.clone(), cx));
        let index_subscription = cx.subscribe(&index, |this, _, event, cx| match event {
            TestIndexEvent::Updated => this.schedule_update(cx),
            TestIndexEvent::ScanCompleted => {
                this.schedule_update(cx);
                // Discovery needs the tree, which the scheduled update rebuilds first.
                if !this.discovery_started {
                    this.discovery_started = true;
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(UPDATE_DEBOUNCE * 2).await;
                        this.update(cx, |this, cx| this.discover_tests(cx)).ok();
                    })
                    .detach();
                }
            }
        });

        let output_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_read_only(true);
            editor.disable_scrollbars_and_minimap(window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_line_numbers(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_bookmarks(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_show_edit_predictions(Some(false), window, cx);
            editor.set_use_modal_editing(false);
            editor.disable_mouse_wheel_zoom();
            editor.set_soft_wrap_mode(SoftWrap::EditorWidth, cx);
            editor
        });

        let mut this = Self {
            workspace: workspace_handle.downgrade(),
            project,
            index,
            fs: workspace.app_state().fs.clone(),
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            files: Vec::new(),
            runs: Vec::new(),
            current_run: None,
            open_group: None,
            next_run_id: 0,
            entries: Vec::new(),
            collapsed: HashSet::default(),
            selected: None,
            show_passed: true,
            show_failed: true,
            show_ignored: false,
            sort_order: SortOrder::Declaration,
            show_full_output: false,
            output_editor,
            displayed_output: None,
            list_width: None,
            context_menu: None,
            sort_menu_handle: PopoverMenuHandle::default(),
            history_menu_handle: PopoverMenuHandle::default(),
            more_menu_handle: PopoverMenuHandle::default(),
            editor_subscriptions: HashMap::default(),
            update_task: Task::ready(()),
            reparse_task: Task::ready(()),
            last_reparse_duration: Duration::ZERO,
            reported_nodes_stale: true,
            pending_nodes: HashSet::default(),
            results: HashMap::default(),
            reported: Vec::new(),
            discovered: Vec::new(),
            discovery_task: Task::ready(()),
            discovery_started: false,
            _subscriptions: vec![workspace_subscription, index_subscription],
        };
        // The panel is created while the workspace is being updated, so the initial
        // refresh has to be deferred until the workspace can be read again.
        this.schedule_update(cx);
        this
    }

    fn schedule_update(&mut self, cx: &mut Context<Self>) {
        self.update_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(UPDATE_DEBOUNCE).await;
            this.update(cx, |this, cx| this.update_entries(cx)).ok();
        });
    }

    /// Rebuilds the tree from the open editors and the index, keeping the run state.
    fn update_entries(&mut self, cx: &mut Context<Self>) {
        let started = Instant::now();
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let editors: Vec<Entity<Editor>> = workspace
            .read(cx)
            .items_of_type::<Editor>(cx)
            .filter(|editor| editor.read(cx).mode().is_full())
            .collect();

        // Editors notify on every change, so they are only observed to schedule a
        // debounced refresh rather than rebuilding the entries synchronously.
        let mut live_editors = HashSet::default();
        for editor in &editors {
            live_editors.insert(editor.entity_id());
            self.editor_subscriptions
                .entry(editor.entity_id())
                .or_insert_with(|| cx.observe(editor, |this, _, cx| this.schedule_update(cx)));
        }
        self.editor_subscriptions
            .retain(|editor_id, _| live_editors.contains(editor_id));

        let mut seen_buffers = HashSet::default();
        let mut open_paths: HashSet<PathBuf> = HashSet::default();
        let mut files = Vec::new();
        for editor in editors {
            let Some(buffer) = editor.read(cx).buffer().read(cx).as_singleton() else {
                continue;
            };
            if !seen_buffers.insert(buffer.entity_id()) {
                continue;
            }
            let Some(file) = buffer.read(cx).file().cloned() else {
                continue;
            };
            let path = file.full_path(cx).to_string_lossy().into_owned();
            let abs_path = file.as_local().map(|file| file.abs_path(cx));
            let file_key = FileKey::Buffer(buffer.entity_id());

            let snapshot = buffer.read(cx).snapshot();
            let tests: Vec<TestNode> = editor
                .read(cx)
                .runnable_entries(cx)
                .into_iter()
                .filter(|entry| entry.buffer.entity_id() == buffer.entity_id())
                .filter(is_test_runnable)
                .map(|entry| {
                    let context_range =
                        entry.tasks.context_range.start.0..entry.tasks.context_range.end.0;
                    let key = TestKey {
                        file: file_key.clone(),
                        row: entry.buffer_row,
                    };
                    let mut node = TestNode::new(
                        NodeKey::Runnable(key),
                        test_name(
                            &snapshot,
                            entry.buffer_row,
                            entry.tasks.column,
                            &entry.tasks.extra_variables,
                        ),
                        entry.buffer_row,
                    );
                    node.column = entry.tasks.column;
                    node.context_range = context_range.clone();
                    node.ignored =
                        is_ignored_test(&snapshot, entry.buffer_row, context_range.start);
                    node.source = Some(NodeSource::Open(OpenSource {
                        editor: editor.clone(),
                        buffer_id: snapshot.remote_id(),
                        tasks: entry.tasks,
                        editor_status: entry.status,
                    }));
                    node
                })
                .collect();
            if let Some(abs_path) = &abs_path {
                open_paths.insert(abs_path.clone());
            }
            if tests.is_empty() {
                continue;
            }
            files.push(FileGroup {
                key: file_key,
                path: path.into(),
                abs_path,
                nodes: Self::build_hierarchy(tests),
            });
        }

        for indexed in self.index.read(cx).files() {
            if open_paths.contains(&indexed.abs_path) {
                continue;
            }
            let file_key = FileKey::Indexed(indexed.project_path.clone());
            let tests: Vec<TestNode> = indexed
                .tests
                .iter()
                .map(|test| {
                    let key = TestKey {
                        file: file_key.clone(),
                        row: test.row,
                    };
                    let mut node =
                        TestNode::new(NodeKey::Runnable(key), test.name.clone().into(), test.row);
                    node.column = test.column;
                    node.context_range = test.context_range.clone();
                    node.ignored = test.ignored;
                    node.source = Some(NodeSource::Indexed(IndexedSource {
                        project_path: indexed.project_path.clone(),
                        test: test.clone(),
                    }));
                    node
                })
                .collect();
            if tests.is_empty() {
                continue;
            }
            files.push(FileGroup {
                key: file_key,
                path: indexed.display_path.clone(),
                abs_path: Some(indexed.abs_path.clone()),
                nodes: Self::build_hierarchy(tests),
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));

        self.files = files;
        self.reported_nodes_stale = true;
        log_slow("rebuilding the tree", started);
        let started = Instant::now();
        self.apply_current_run(cx);
        log_slow("applying results", started);
        let started = Instant::now();
        self.rebuild_flat_entries();
        log_slow("flattening the tree", started);
        cx.notify();
    }

    /// Nests each test under the closest runnable whose range encloses it, e.g. `it`
    /// blocks under their `describe`.
    fn build_hierarchy(mut nodes: Vec<TestNode>) -> Vec<TestNode> {
        nodes.sort_by(|a, b| {
            a.context_range
                .start
                .cmp(&b.context_range.start)
                .then_with(|| b.context_range.end.cmp(&a.context_range.end))
        });
        let mut roots: Vec<TestNode> = Vec::new();
        let mut stack: Vec<TestNode> = Vec::new();

        fn attach(stack: &mut Vec<TestNode>, roots: &mut Vec<TestNode>, node: TestNode) {
            match stack.last_mut() {
                Some(parent) => parent.children.push(node),
                None => roots.push(node),
            }
        }

        for node in nodes {
            while let Some(parent) = stack.last() {
                if parent.context_range.start <= node.context_range.start
                    && node.context_range.end <= parent.context_range.end
                {
                    break;
                }
                let finished = stack.pop().expect("stack is non-empty");
                attach(&mut stack, &mut roots, finished);
            }
            stack.push(node);
        }
        while let Some(finished) = stack.pop() {
            attach(&mut stack, &mut roots, finished);
        }
        roots
    }

    // --- Run state -------------------------------------------------------------------

    fn current_run(&self) -> Option<&TestRun> {
        self.current_run.and_then(|index| self.runs.get(index))
    }

    /// Recomputes the status, duration and output of every node from the reports of the
    /// current run, and adds nodes for tests that were reported but are not listed.
    fn apply_current_run(&mut self, cx: &mut Context<Self>) {
        let rebuild_reported = self.reported_nodes_stale;
        if rebuild_reported {
            self.files
                .retain(|file| !matches!(file.key, FileKey::Synthetic(_)));
        }
        for file in &mut self.files {
            if rebuild_reported {
                file.nodes.retain(|node| node.source.is_some());
            }
            for node in &mut file.nodes {
                node.reset_run_state(rebuild_reported);
            }
        }

        if rebuild_reported {
            // Tests known only from runners come first, so that results can attach to
            // them. They only need rebuilding when the tree itself was rebuilt.
            let known: Vec<ReportEntry> = self
                .discovered
                .iter()
                .chain(self.reported.iter())
                .cloned()
                .collect();
            let name_index = self.build_name_index();
            let unlisted: Vec<(Option<(usize, usize)>, ReportEntry)> = known
                .into_iter()
                .filter(|entry| self.find_node_for_entry(&name_index, entry).is_none())
                .map(|entry| (None, entry))
                .collect();
            let started = Instant::now();
            self.add_reported_nodes(unlisted);
            log_slow("adding runner-reported tests", started);
            self.reported_nodes_stale = false;
        }
        let started = Instant::now();
        let name_index = self.build_name_index();
        log_slow("indexing node names", started);

        let viewing_latest = self
            .current_run
            .is_none_or(|index| index + 1 == self.runs.len());
        let mut running_targets: Vec<TestKey> = Vec::new();

        if let Some(run_index) = self.current_run {
            let run = &self.runs[run_index];
            let run_id = run.id;
            let run_is_running = run.status() == RunnableTaskStatus::Running;
            let mut fresh: HashMap<NodeKey, NodeResult> = HashMap::default();
            let mut unmatched: Vec<(usize, ReportEntry)> = Vec::new();

            for (process_index, process) in run.processes.iter().enumerate() {
                if process.status == RunnableTaskStatus::Running
                    && let Some(target) = &process.target
                {
                    running_targets.push(target.clone());
                }
                for entry in &process.report.entries {
                    let matched = self
                        .find_node_for_entry(&name_index, entry)
                        .map(|node| node.key.clone());
                    // A runner filtered down to one test reports the file's other tests as
                    // skipped; that says nothing about those tests.
                    if entry.status == TestStatus::Skipped
                        && let Some(target) = &process.target
                        && matched.as_ref() != Some(&NodeKey::Runnable(target.clone()))
                    {
                        continue;
                    }
                    match matched {
                        Some(key) => {
                            let result = fresh.entry(key).or_default();
                            if entry.kind == EntryKind::Test {
                                result.status = status_from_report(entry.status);
                                result.skipped = entry.status == TestStatus::Skipped;
                            }
                            if entry.duration.is_some() {
                                result.duration = entry.duration;
                            }
                            result.output.push_str(&entry.output);
                            result.run_ref = Some((run_id, process_index));
                        }
                        None => unmatched.push((process_index, entry.clone())),
                    }
                }
            }

            if !unmatched.is_empty() {
                log::info!(
                    "test panel: {} of the run's report entries do not match a listed test",
                    unmatched.len()
                );
            }
            if run_is_running {
                running_targets.extend(self.pending_nodes.iter().filter_map(|key| match key {
                    NodeKey::Runnable(test_key) if !fresh.contains_key(key) => {
                        Some(test_key.clone())
                    }
                    _ => None,
                }));
            }
            if viewing_latest {
                self.results.extend(fresh);
                for (_, entry) in &unmatched {
                    if entry.kind == EntryKind::Test {
                        self.reported.retain(|reported| {
                            reported.path != entry.path || reported.file != entry.file
                        });
                        self.reported.push(entry.clone());
                        self.reported_nodes_stale = true;
                    }
                }
                self.add_reported_nodes(
                    unmatched
                        .into_iter()
                        .map(|(process_index, entry)| (Some((run_id, process_index)), entry))
                        .collect(),
                );
            } else {
                self.add_reported_nodes(
                    unmatched
                        .into_iter()
                        .map(|(process_index, entry)| (Some((run_id, process_index)), entry))
                        .collect(),
                );
                let results = fresh;
                Self::visit_nodes_mut(&mut self.files, &mut |node| {
                    if let Some(result) = results.get(&node.key) {
                        node.apply_result(result);
                    }
                });
            }
        }

        if viewing_latest {
            let results = &self.results;
            Self::visit_nodes_mut(&mut self.files, &mut |node| {
                if let Some(result) = results.get(&node.key) {
                    node.apply_result(result);
                }
            });
        }

        for target in running_targets {
            if let Some(node) = self.node_by_key_mut(&NodeKey::Runnable(target)) {
                node.status = Some(RunnableTaskStatus::Running);
            }
        }

        let started = Instant::now();
        self.sync_editor_statuses(cx);
        log_slow("syncing gutter statuses", started);
    }

    fn visit_nodes_mut(files: &mut [FileGroup], visit: &mut dyn FnMut(&mut TestNode)) {
        fn walk(nodes: &mut [TestNode], visit: &mut dyn FnMut(&mut TestNode)) {
            for node in nodes {
                visit(node);
                walk(&mut node.children, visit);
            }
        }
        for file in files {
            walk(&mut file.nodes, visit);
        }
    }

    /// Mirrors the tree's statuses into the editors' gutters. Only statuses that differ
    /// from what the editor shows are written, since writing notifies the editor, which
    /// in turn schedules a refresh of this panel.
    fn sync_editor_statuses(&mut self, cx: &mut Context<Self>) {
        fn visit(node: &mut TestNode, cx: &mut App) {
            if let Some(NodeSource::Open(source)) = &mut node.source
                && source.editor_status != node.status
            {
                source.editor_status = node.status;
                let (buffer_id, row) = (source.buffer_id, node.row);
                source.editor.update(cx, |editor, cx| match node.status {
                    Some(status) => editor.set_runnable_task_status(buffer_id, row, status, cx),
                    None => editor.clear_runnable_task_status(buffer_id, row, cx),
                });
            }
            for child in &mut node.children {
                visit(child, cx);
            }
        }
        for file in &mut self.files {
            for node in &mut file.nodes {
                visit(node, cx);
            }
        }
    }

    /// Indexes every node by its normalized name, so that a run's report entries can be
    /// matched without scanning the whole tree for each of them.
    fn build_name_index(&self) -> NameIndex {
        fn walk(
            nodes: &[TestNode],
            file_index: usize,
            ancestors: &mut Vec<String>,
            by_name: &mut HashMap<String, Vec<IndexedNode>>,
        ) {
            for node in nodes {
                by_name
                    .entry(node.normalized_name.clone())
                    .or_default()
                    .push(IndexedNode {
                        file_index,
                        ancestors: ancestors.clone(),
                        key: node.key.clone(),
                    });
                ancestors.push(node.normalized_name.clone());
                walk(&node.children, file_index, ancestors, by_name);
                ancestors.pop();
            }
        }
        let mut by_name = HashMap::default();
        for (file_index, file) in self.files.iter().enumerate() {
            walk(&file.nodes, file_index, &mut Vec::new(), &mut by_name);
        }
        NameIndex { by_name }
    }

    /// Finds the listed test a reported entry refers to: the node whose name matches the
    /// last segment of the entry's path, preferring nodes whose ancestors match the
    /// preceding segments and whose file matches the reported file.
    fn find_node_for_entry<'a>(
        &self,
        index: &'a NameIndex,
        entry: &ReportEntry,
    ) -> Option<&'a IndexedNode> {
        let (last, parents) = entry.path.split_last()?;
        let candidates = index.by_name.get(&normalize_name(last))?;
        let parents: Vec<String> = parents
            .iter()
            .map(|segment| normalize_name(segment))
            .collect();

        candidates
            .iter()
            .filter(|candidate| {
                entry.file.as_ref().is_none_or(|reported_file| {
                    self.files
                        .get(candidate.file_index)
                        .is_some_and(|file| Self::file_matches(file, reported_file))
                })
            })
            .map(|candidate| {
                let mut score = 1;
                for (ancestor, parent) in candidate.ancestors.iter().rev().zip(parents.iter().rev())
                {
                    if ancestor != parent {
                        break;
                    }
                    score += 1;
                }
                if candidate.ancestors.len() == parents.len() {
                    score += 1;
                }
                if entry.file.is_some() {
                    score += 10;
                }
                (score, candidate)
            })
            .max_by_key(|(score, _)| *score)
            .map(|(_, candidate)| candidate)
    }

    fn file_matches(file: &FileGroup, reported_file: &str) -> bool {
        let reported = reported_file.replace('\\', "/");
        let reported = reported.trim_start_matches("./");
        let matches_path = |path: &str| {
            let path = path.replace('\\', "/");
            path == reported || path.ends_with(&format!("/{reported}")) || reported.ends_with(&path)
        };
        file.abs_path
            .as_ref()
            .is_some_and(|abs_path| matches_path(&abs_path.to_string_lossy()))
            || matches_path(&file.path)
    }

    /// Adds nodes for reported tests that are not listed, nested by their path. Existing
    /// nodes are looked up through a map of normalized paths, so inserting many entries
    /// stays linear even when a run reports thousands of tests.
    fn add_reported_nodes(&mut self, entries: Vec<(Option<(usize, usize)>, ReportEntry)>) {
        if entries.is_empty() {
            return;
        }
        // (file index, normalized path) -> index path within the file's nodes.
        let mut lookup: HashMap<(usize, Vec<String>), Vec<usize>> = HashMap::default();
        fn walk(
            nodes: &[TestNode],
            file_index: usize,
            path: &mut Vec<usize>,
            names: &mut Vec<String>,
            lookup: &mut HashMap<(usize, Vec<String>), Vec<usize>>,
        ) {
            for (index, node) in nodes.iter().enumerate() {
                path.push(index);
                names.push(node.normalized_name.clone());
                lookup
                    .entry((file_index, names.clone()))
                    .or_insert_with(|| path.clone());
                walk(&node.children, file_index, path, names, lookup);
                names.pop();
                path.pop();
            }
        }
        for (file_index, file) in self.files.iter().enumerate() {
            walk(
                &file.nodes,
                file_index,
                &mut Vec::new(),
                &mut Vec::new(),
                &mut lookup,
            );
        }
        let mut groups_by_name: HashMap<SharedString, usize> = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| match &file.key {
                FileKey::Synthetic(name) => Some((name.clone(), index)),
                _ => None,
            })
            .collect();

        for (run_ref, entry) in entries {
            let matched_file = entry.file.as_ref().and_then(|reported| {
                self.files
                    .iter()
                    .position(|file| Self::file_matches(file, reported))
            });
            let file_index = match matched_file {
                Some(index) => index,
                None => {
                    let group: SharedString = match &entry.file {
                        Some(file) => file.clone().into(),
                        None => "Other tests".into(),
                    };
                    match groups_by_name.get(&group) {
                        Some(index) => *index,
                        None => {
                            self.files.push(FileGroup {
                                key: FileKey::Synthetic(group.clone()),
                                path: group.clone(),
                                abs_path: None,
                                nodes: Vec::new(),
                            });
                            let index = self.files.len() - 1;
                            groups_by_name.insert(group, index);
                            index
                        }
                    }
                }
            };
            let group_name = self.files[file_index].path.clone();

            let mut index_path: Vec<usize> = Vec::new();
            let mut names: Vec<String> = Vec::new();
            let mut path_so_far: Vec<String> = Vec::new();
            for (depth, segment) in entry.path.iter().enumerate() {
                path_so_far.push(segment.clone());
                names.push(normalize_name(segment));
                let is_last = depth + 1 == entry.path.len();
                let key = (file_index, names.clone());
                let child_index = match lookup.get(&key) {
                    Some(existing) => *existing.last().expect("paths are non-empty"),
                    None => {
                        let siblings = match Self::nodes_at_mut(
                            &mut self.files[file_index].nodes,
                            &index_path,
                        ) {
                            Some(siblings) => siblings,
                            None => break,
                        };
                        siblings.push(TestNode::new(
                            NodeKey::Reported {
                                group: group_name.clone(),
                                path: path_so_far.clone(),
                            },
                            segment.clone().into(),
                            u32::MAX,
                        ));
                        let child_index = siblings.len() - 1;
                        let mut new_path = index_path.clone();
                        new_path.push(child_index);
                        lookup.insert(key, new_path);
                        child_index
                    }
                };
                index_path.push(child_index);
                if is_last
                    && let Some(node) =
                        Self::node_at_path_mut(&mut self.files[file_index].nodes, &index_path)
                {
                    if entry.kind == EntryKind::Test {
                        node.status = status_from_report(entry.status);
                        node.skipped = entry.status == TestStatus::Skipped;
                    }
                    if entry.duration.is_some() {
                        node.duration = entry.duration;
                    }
                    node.output.push_str(&entry.output);
                    if run_ref.is_some() {
                        node.run_ref = run_ref;
                    }
                }
            }
        }
    }

    /// The children vector reached by following `path` from the given roots.
    fn nodes_at_mut<'a>(
        roots: &'a mut Vec<TestNode>,
        path: &[usize],
    ) -> Option<&'a mut Vec<TestNode>> {
        let mut nodes = roots;
        for index in path {
            nodes = &mut nodes.get_mut(*index)?.children;
        }
        Some(nodes)
    }

    fn node_at_path_mut<'a>(roots: &'a mut [TestNode], path: &[usize]) -> Option<&'a mut TestNode> {
        let (first, rest) = path.split_first()?;
        let mut node = roots.get_mut(*first)?;
        for index in rest {
            node = node.children.get_mut(*index)?;
        }
        Some(node)
    }

    fn node_at(&self, file_index: usize, path: &[usize]) -> Option<&TestNode> {
        let (first, rest) = path.split_first()?;
        let mut node = self.files.get(file_index)?.nodes.get(*first)?;
        for index in rest {
            node = node.children.get(*index)?;
        }
        Some(node)
    }

    fn node_by_key(&self, key: &NodeKey) -> Option<&TestNode> {
        fn visit<'a>(nodes: &'a [TestNode], key: &NodeKey) -> Option<&'a TestNode> {
            for node in nodes {
                if &node.key == key {
                    return Some(node);
                }
                if let Some(found) = visit(&node.children, key) {
                    return Some(found);
                }
            }
            None
        }
        self.files.iter().find_map(|file| visit(&file.nodes, key))
    }

    fn node_by_key_mut(&mut self, key: &NodeKey) -> Option<&mut TestNode> {
        fn visit<'a>(nodes: &'a mut [TestNode], key: &NodeKey) -> Option<&'a mut TestNode> {
            for node in nodes {
                if &node.key == key {
                    return Some(node);
                }
                if let Some(found) = visit(&mut node.children, key) {
                    return Some(found);
                }
            }
            None
        }
        self.files
            .iter_mut()
            .find_map(|file| visit(&mut file.nodes, key))
    }

    fn all_nodes(&self) -> Vec<&TestNode> {
        fn visit<'a>(nodes: &'a [TestNode], out: &mut Vec<&'a TestNode>) {
            for node in nodes {
                out.push(node);
                visit(&node.children, out);
            }
        }
        let mut out = Vec::new();
        for file in &self.files {
            visit(&file.nodes, &mut out);
        }
        out
    }

    fn summary(&self) -> StatusSummary {
        let mut summary = StatusSummary::default();
        for node in self.all_nodes() {
            if node.is_leaf() {
                summary.record(node);
            }
        }
        summary
    }

    // --- Visibility and flattening -------------------------------------------------------

    fn passes_filter(&self, node: &TestNode) -> bool {
        if node.skipped || node.ignored {
            return self.show_ignored;
        }
        match node.status {
            Some(RunnableTaskStatus::Passed) => self.show_passed,
            Some(RunnableTaskStatus::Failed) => self.show_failed,
            _ => true,
        }
    }

    fn is_visible(&self, node: &TestNode) -> bool {
        self.passes_filter(node) || node.children.iter().any(|child| self.is_visible(child))
    }

    fn sorted_children<'a>(&self, nodes: &'a [TestNode]) -> Vec<(usize, &'a TestNode)> {
        let mut children: Vec<(usize, &TestNode)> = nodes.iter().enumerate().collect();
        match self.sort_order {
            SortOrder::Declaration => children.sort_by_key(|(_, node)| node.row),
            SortOrder::Alphabetical => {
                children.sort_by_key(|(_, node)| node.name.to_lowercase());
            }
            SortOrder::Duration => {
                children.sort_by(|(_, a), (_, b)| match (a.duration, b.duration) {
                    (Some(a), Some(b)) => b.cmp(&a),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a.row.cmp(&b.row),
                });
            }
        }
        children
    }

    fn rebuild_flat_entries(&mut self) {
        let mut entries = Vec::new();
        if self.current_run.is_some() {
            entries.push(PanelEntry::Run);
        }
        for file_index in 0..self.files.len() {
            let file = &self.files[file_index];
            entries.push(PanelEntry::File { file_index });
            if self.collapsed.contains(&Self::file_collapse_key(file)) {
                continue;
            }
            let mut path = Vec::new();
            self.flatten_nodes(file_index, &file.nodes, &mut path, &mut entries);
        }
        self.entries = entries;
    }

    fn flatten_nodes(
        &self,
        file_index: usize,
        nodes: &[TestNode],
        path: &mut Vec<usize>,
        entries: &mut Vec<PanelEntry>,
    ) {
        for (index, node) in self.sorted_children(nodes) {
            if !self.is_visible(node) {
                continue;
            }
            path.push(index);
            entries.push(PanelEntry::Node {
                file_index,
                path: path.clone(),
            });
            if !node.children.is_empty() && !self.collapsed.contains(&node.key) {
                self.flatten_nodes(file_index, &node.children, path, entries);
            }
            path.pop();
        }
    }

    fn file_collapse_key(file: &FileGroup) -> NodeKey {
        NodeKey::Reported {
            group: file.path.clone(),
            path: Vec::new(),
        }
    }

    fn toggle_collapsed(&mut self, key: NodeKey, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild_flat_entries();
        cx.notify();
    }

    fn expand_all(&mut self, cx: &mut Context<Self>) {
        self.collapsed.clear();
        self.rebuild_flat_entries();
        cx.notify();
    }

    fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let mut keys = Vec::new();
        for file in &self.files {
            keys.push(Self::file_collapse_key(file));
        }
        for node in self.all_nodes() {
            if !node.children.is_empty() {
                keys.push(node.key.clone());
            }
        }
        self.collapsed.extend(keys);
        self.rebuild_flat_entries();
        cx.notify();
    }

    fn set_sort_order(&mut self, sort_order: SortOrder, cx: &mut Context<Self>) {
        self.sort_order = sort_order;
        self.rebuild_flat_entries();
        cx.notify();
    }

    fn toggle_show_passed(&mut self, cx: &mut Context<Self>) {
        self.show_passed = !self.show_passed;
        self.rebuild_flat_entries();
        cx.notify();
    }

    fn toggle_show_failed(&mut self, cx: &mut Context<Self>) {
        self.show_failed = !self.show_failed;
        self.rebuild_flat_entries();
        cx.notify();
    }

    fn toggle_show_ignored(&mut self, cx: &mut Context<Self>) {
        self.show_ignored = !self.show_ignored;
        self.rebuild_flat_entries();
        cx.notify();
    }

    fn select_run(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.runs.len() {
            self.current_run = Some(index);
            self.selected = Some(Selection::Run);
            self.apply_current_run(cx);
            self.rebuild_flat_entries();
            cx.notify();
        }
    }

    // --- Running tests -------------------------------------------------------------------

    /// Opens a run that the next `count` scheduled test tasks are attached to, and clears
    /// the results of the previous run.
    fn open_run_group(
        &mut self,
        label: SharedString,
        count: usize,
        cx: &mut Context<Self>,
    ) -> usize {
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        self.runs.push(TestRun {
            id: run_id,
            label,
            started_at: Local::now(),
            processes: Vec::new(),
        });
        if self.runs.len() > MAX_HISTORY {
            self.runs.remove(0);
        }
        self.current_run = Some(self.runs.len() - 1);
        self.open_group = Some(OpenRunGroup {
            run_id,
            remaining: count,
            opened_at: Instant::now(),
        });
        self.selected = Some(Selection::Run);
        self.show_full_output = false;
        self.pending_nodes.clear();
        self.apply_current_run(cx);
        self.rebuild_flat_entries();
        cx.notify();
        run_id
    }

    fn consume_open_group_slot(&mut self) -> Option<usize> {
        let group = self.open_group.as_mut()?;
        // A group left open by a task that never spawned should not swallow later runs.
        if group.opened_at.elapsed() > Duration::from_secs(60) {
            self.open_group = None;
            return None;
        }
        group.remaining = group.remaining.saturating_sub(1);
        let run_id = group.run_id;
        if group.remaining == 0 {
            self.open_group = None;
        }
        Some(run_id)
    }

    fn schedule_resolved(
        workspace: &WeakEntity<Workspace>,
        task_source_kind: TaskSourceKind,
        resolved_task: ResolvedTask,
        cx: &mut AsyncWindowContext,
    ) {
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.schedule_resolved_task(
                    task_source_kind,
                    resolved_task,
                    false,
                    window,
                    cx,
                );
            })
            .ok();
    }

    /// Resolves the task of a listed test. Tests with an open editor use the editor's
    /// runnable data; indexed tests open their buffer first and look up the task templates
    /// bound to the runnable's tags, the way the editor does.
    fn resolve_node_task(
        &self,
        node: &TestNode,
        cx: &mut Context<Self>,
    ) -> Task<Option<(TaskSourceKind, ResolvedTask)>> {
        let project = self.project.clone();
        let (row, column) = (node.row, node.column);
        let source = match node.source.clone() {
            Some(NodeSource::Open(source)) => {
                let task_context = source.editor.update(cx, |editor, cx| {
                    editor.runnable_task_context(source.buffer_id, row, cx)
                });
                let tasks = source.tasks.clone();
                return cx.spawn(async move |_, _| {
                    let task_context = task_context.await.log_err().flatten()?;
                    tasks.resolve(&task_context).next()
                });
            }
            Some(NodeSource::Indexed(source)) => source,
            None => return Task::ready(None),
        };
        let project_path = source.project_path.clone();
        let tags = source.test.tags.clone();
        let extra_variables = source.test.extra_variables;

        cx.spawn(async move |_, cx| {
            let buffer = project
                .update(cx, |project, cx| {
                    project.open_buffer(project_path.clone(), cx)
                })
                .await
                .log_err()?;
            let templates = project
                .update(cx, |project, cx| {
                    let language = buffer.read(cx).language().cloned();
                    let inventory = project.task_store().read(cx).task_inventory().cloned()?;
                    Some(inventory.read(cx).list_tasks(
                        Some(buffer.clone()),
                        language,
                        Some(project_path.worktree_id),
                        cx,
                    ))
                })?
                .await;
            let templates: Vec<(TaskSourceKind, TaskTemplate)> = templates
                .into_iter()
                .filter(|(_, template)| template.tags.iter().any(|tag| tags.contains(tag)))
                .collect();
            if templates.is_empty() {
                log::warn!("no task template is bound to the tags {tags:?}");
                return None;
            }
            let mut captured_variables = TaskVariables::default();
            for (name, value) in extra_variables {
                captured_variables.insert(VariableName::Custom(name.into()), value);
            }
            let task_context = project
                .update(cx, |project, cx| {
                    let anchor = buffer.read(cx).anchor_after(Point::new(row, column));
                    let location = Location {
                        buffer: buffer.clone(),
                        range: anchor..anchor,
                    };
                    project.task_store().update(cx, |task_store, cx| {
                        task_store.task_context_for_location(captured_variables, location, cx)
                    })
                })
                .await
                .log_err()
                .flatten()?;
            templates.into_iter().find_map(|(kind, template)| {
                template
                    .resolve_task(&kind.to_id_base(), &task_context)
                    .map(|resolved| (kind, resolved))
            })
        })
    }

    /// Schedules the task of a listed test through the workspace, the same way the gutter
    /// run indicator does. The task is routed back into the panel by the
    /// [`TaskSpawnHandler`] registered in [`Self::load`].
    fn schedule_node_task(&mut self, node: &TestNode, window: &mut Window, cx: &mut Context<Self>) {
        let resolved = self.resolve_node_task(node, cx);
        let workspace = self.workspace.clone();
        let key = node.key.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Some((task_source_kind, resolved_task)) = resolved.await else {
                this.update(cx, |this, cx| {
                    this.consume_open_group_slot();
                    if let Some(node) = this.node_by_key_mut(&key) {
                        node.status = Some(RunnableTaskStatus::Failed);
                    }
                    this.sync_editor_statuses(cx);
                    cx.notify();
                })
                .ok();
                return;
            };
            Self::schedule_resolved(&workspace, task_source_kind, resolved_task, cx);
        })
        .detach();
    }

    fn collect_keys(node: &TestNode, keys: &mut HashSet<NodeKey>) {
        if node.source.is_some() {
            keys.insert(node.key.clone());
        }
        for child in &node.children {
            Self::collect_keys(child, keys);
        }
    }

    fn run_node(&mut self, key: &NodeKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(node) = self.node_by_key(key).cloned() else {
            return;
        };
        if node.source.is_none() {
            return;
        }
        self.open_run_group(node.name.clone(), 1, cx);
        self.selected = Some(Selection::Node(key.clone()));
        Self::collect_keys(&node, &mut self.pending_nodes);
        self.apply_current_run(cx);
        self.rebuild_flat_entries();
        self.schedule_node_task(&node, window, cx);
        cx.notify();
    }

    /// Resolves the task that runs the given scope for a file, given one of its tests to
    /// derive the runner and the task context from.
    fn resolve_scoped_task(
        &self,
        node: &TestNode,
        scope: RunScope,
        cx: &mut Context<Self>,
    ) -> Task<Option<(TaskSourceKind, ResolvedTask)>> {
        let Some(source) = node.source.clone() else {
            return Task::ready(None);
        };
        let project = self.project.clone();
        let (row, column) = (node.row, node.column);

        cx.spawn(async move |_, cx| {
            let (buffer, worktree_id, node_templates, task_context) = match &source {
                NodeSource::Open(source) => {
                    let buffer = project.read_with(cx, |project, cx| {
                        project.buffer_for_id(source.buffer_id, cx)
                    })?;
                    let worktree_id = buffer.read_with(cx, |buffer, cx| {
                        buffer.file().map(|file| file.worktree_id(cx))
                    })?;
                    let task_context = source
                        .editor
                        .update(cx, |editor, cx| {
                            editor.runnable_task_context(source.buffer_id, row, cx)
                        })
                        .await
                        .log_err()
                        .flatten()?;
                    (
                        buffer,
                        worktree_id,
                        source.tasks.templates.clone(),
                        task_context,
                    )
                }
                NodeSource::Indexed(source) => {
                    let buffer = project
                        .update(cx, |project, cx| {
                            project.open_buffer(source.project_path.clone(), cx)
                        })
                        .await
                        .log_err()?;
                    let worktree_id = source.project_path.worktree_id;
                    let templates =
                        Self::list_templates(&project, &buffer, worktree_id, cx).await?;
                    let node_templates: Vec<(TaskSourceKind, TaskTemplate)> = templates
                        .into_iter()
                        .filter(|(_, template)| {
                            template
                                .tags
                                .iter()
                                .any(|tag| source.test.tags.contains(tag))
                        })
                        .collect();
                    let mut captured_variables = TaskVariables::default();
                    for (name, value) in source.test.extra_variables.clone() {
                        captured_variables.insert(VariableName::Custom(name.into()), value);
                    }
                    let task_context = project
                        .update(cx, |project, cx| {
                            let anchor = buffer.read(cx).anchor_after(Point::new(row, column));
                            let location = Location {
                                buffer: buffer.clone(),
                                range: anchor..anchor,
                            };
                            project.task_store().update(cx, |task_store, cx| {
                                task_store.task_context_for_location(
                                    captured_variables,
                                    location,
                                    cx,
                                )
                            })
                        })
                        .await
                        .log_err()
                        .flatten()?;
                    (buffer, worktree_id, node_templates, task_context)
                }
            };
            let templates = Self::list_templates(&project, &buffer, worktree_id, cx).await?;
            let (kind, template) = pick_scoped_template(&templates, &node_templates, scope)?;
            template
                .resolve_task(&kind.to_id_base(), &task_context)
                .map(|resolved| (kind, resolved))
        })
    }

    async fn list_templates(
        project: &Entity<Project>,
        buffer: &Entity<language::Buffer>,
        worktree_id: project::WorktreeId,
        cx: &mut gpui::AsyncApp,
    ) -> Option<Vec<(TaskSourceKind, TaskTemplate)>> {
        let templates = project.update(cx, |project, cx| {
            let language = buffer.read(cx).language().cloned();
            let inventory = project.task_store().read(cx).task_inventory().cloned()?;
            Some(inventory.read(cx).list_tasks(
                Some(buffer.clone()),
                language,
                Some(worktree_id),
                cx,
            ))
        })?;
        Some(templates.await)
    }

    /// Groups files by the runner their tests are bound to, returning one test per group
    /// to resolve the scoped task from, along with the visible top-level tests of the
    /// group's files. A project-wide scope resolves one task per runner rather than one
    /// per file, which would open every file's buffer only to reach the same command.
    fn group_files_by_runner(
        &self,
        file_indices: Vec<usize>,
        scope: RunScope,
    ) -> Vec<(TestNode, Vec<TestNode>)> {
        let mut groups: Vec<(Vec<String>, TestNode, Vec<TestNode>)> = Vec::new();
        for file_index in file_indices {
            let Some(file) = self.files.get(file_index) else {
                continue;
            };
            let roots: Vec<TestNode> = file
                .nodes
                .iter()
                .filter(|node| self.is_visible(node) && node.source.is_some())
                .cloned()
                .collect();
            let Some(first) = roots.first().cloned() else {
                continue;
            };
            let group_key = match scope {
                RunScope::File => vec![file.path.to_string()],
                RunScope::Project => runner_tags(&first),
            };
            match groups.iter_mut().find(|(key, _, _)| *key == group_key) {
                Some((_, _, group_roots)) => group_roots.extend(roots),
                None => groups.push((group_key, first, roots)),
            }
        }
        groups
            .into_iter()
            .map(|(_, representative, roots)| (representative, roots))
            .collect()
    }

    /// Runs the tests of the given files with the languages' scoped test tasks when there
    /// are any, so that the runner decides how to schedule them, and otherwise runs each
    /// top-level test on its own. Identical resolved commands run once, so a project-wide
    /// scope starts one process per runner (or per package for cargo).
    fn run_files(
        &mut self,
        file_indices: Vec<usize>,
        label: SharedString,
        scope: RunScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        struct FileRun {
            roots: Vec<TestNode>,
            file_task: Task<Option<(TaskSourceKind, ResolvedTask)>>,
        }

        let mut file_runs: Vec<FileRun> = Vec::new();
        for (representative, roots) in self.group_files_by_runner(file_indices, scope) {
            let file_task = self.resolve_scoped_task(&representative, scope, cx);
            file_runs.push(FileRun { roots, file_task });
        }
        if file_runs.is_empty() {
            return;
        }

        self.open_run_group(label, 1, cx);
        let mut pending = HashSet::default();
        for file_run in &file_runs {
            for root in &file_run.roots {
                Self::collect_keys(root, &mut pending);
            }
        }
        self.pending_nodes = pending;
        self.apply_current_run(cx);
        self.rebuild_flat_entries();
        let workspace = self.workspace.clone();

        cx.spawn_in(window, async move |this, cx| {
            let mut scheduled_commands: HashSet<String> = HashSet::default();
            let mut pending: Vec<(TaskSourceKind, ResolvedTask)> = Vec::new();
            let mut fallback_nodes: Vec<TestNode> = Vec::new();

            for file_run in file_runs {
                match file_run.file_task.await {
                    Some((kind, resolved)) => {
                        let command = format!(
                            "{}@{:?}",
                            resolved.resolved.command_label, resolved.resolved.cwd
                        );
                        if scheduled_commands.insert(command) {
                            pending.push((kind, resolved));
                        }
                    }
                    None => fallback_nodes.extend(file_run.roots),
                }
            }

            let total = pending.len() + fallback_nodes.len();
            this.update(cx, |this, cx| match this.open_group.as_mut() {
                Some(group) if total > 0 => group.remaining = total,
                _ => {
                    this.open_group = None;
                    cx.notify();
                }
            })
            .ok();
            for (kind, resolved) in pending {
                Self::schedule_resolved(&workspace, kind, resolved, cx);
            }
            this.update_in(cx, |this, window, cx| {
                for node in &fallback_nodes {
                    if let Some(node) = this.node_by_key_mut(&node.key) {
                        node.status = Some(RunnableTaskStatus::Running);
                    }
                    this.schedule_node_task(node, window, cx);
                }
                this.sync_editor_statuses(cx);
            })
            .ok();
        })
        .detach();
    }

    pub fn run_all_tests(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let file_indices: Vec<usize> = (0..self.files.len())
            .filter(|index| !matches!(self.files[*index].key, FileKey::Synthetic(_)))
            .collect();
        self.run_files(
            file_indices,
            "All tests".into(),
            RunScope::Project,
            window,
            cx,
        );
    }

    fn run_file(&mut self, file_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.files.get(file_index) else {
            return;
        };
        let label = file.path.clone();
        self.run_files(vec![file_index], label, RunScope::File, window, cx);
    }

    /// Asks the runners that can enumerate tests to list them, adding tests the static
    /// analysis cannot see, such as parametrized cases. Runs quietly in the background:
    /// one runner at a time, without a terminal in the panel and without touching the
    /// current run.
    pub fn discover_tests(&mut self, cx: &mut Context<Self>) {
        self.discovery_started = true;
        let file_indices: Vec<usize> = (0..self.files.len())
            .filter(|index| !matches!(self.files[*index].key, FileKey::Synthetic(_)))
            .collect();
        let file_tasks: Vec<Task<Option<(TaskSourceKind, ResolvedTask)>>> = self
            .group_files_by_runner(file_indices, RunScope::Project)
            .into_iter()
            .map(|(representative, _)| {
                self.resolve_scoped_task(&representative, RunScope::Project, cx)
            })
            .collect();
        let project = self.project.clone();

        self.discovery_task = cx.spawn(async move |this, cx| {
            let mut seen_commands: HashSet<String> = HashSet::default();
            let mut tasks: Vec<ResolvedTask> = Vec::new();
            for file_task in file_tasks {
                if let Some((_, resolved)) = file_task.await {
                    let command = format!(
                        "{}@{:?}",
                        resolved.resolved.command_label, resolved.resolved.cwd
                    );
                    if seen_commands.insert(command) {
                        tasks.push(resolved);
                    }
                }
            }

            let mut discovered: Vec<ReportEntry> = Vec::new();
            for mut task in tasks {
                let runner = Runner::detect(&task.resolved.command_label);
                let Some(args) = runner.collection_args(&task.resolved.args) else {
                    continue;
                };
                Self::extend_task(&mut task, &args);
                Self::disable_colors(&mut task);
                let spawn_in_terminal = project.read_with(cx, |project, cx| {
                    if project.is_via_collab() {
                        return None;
                    }
                    let is_windows = project.path_style(cx).is_windows();
                    let remote_shell = project
                        .remote_client()
                        .and_then(|remote_client| remote_client.read(cx).shell());
                    let shell = match remote_shell {
                        Some(remote_shell) if task.resolved.shell == Shell::System => {
                            Shell::Program(remote_shell)
                        }
                        _ => task.resolved.shell.clone(),
                    };
                    Some(prepare_task_for_spawn(&task.resolved, &shell, is_windows))
                });
                let Some(spawn_in_terminal) = spawn_in_terminal else {
                    continue;
                };
                let terminal = project
                    .update(cx, |project, cx| {
                        project.create_terminal_task(spawn_in_terminal, cx)
                    })
                    .await;
                let Some(terminal) = terminal.log_err() else {
                    continue;
                };
                terminal
                    .read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
                    .await;
                let content = terminal.read_with(cx, |terminal, _| terminal.get_content());
                discovered.extend(runner.parse_collection(&content).into_iter().map(
                    |CollectedTest { file, path }| ReportEntry {
                        kind: EntryKind::Test,
                        file,
                        path,
                        status: TestStatus::NotRun,
                        duration: None,
                        output: String::new(),
                    },
                ));
                let discovered = discovered.clone();
                this.update(cx, |this, cx| {
                    this.discovered = discovered;
                    this.reported_nodes_stale = true;
                    this.apply_current_run(cx);
                    this.rebuild_flat_entries();
                    cx.notify();
                })
                .ok();
            }
        });
    }

    pub fn rerun_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(run) = self.current_run() else {
            return;
        };
        let tasks: Vec<ResolvedTask> = run
            .processes
            .iter()
            .map(|process| process.task.clone())
            .collect();
        let label = run.label.clone();
        if tasks.is_empty() {
            return;
        }
        self.open_run_group(label, tasks.len(), cx);
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |_, cx| {
            for task in tasks {
                Self::schedule_resolved(&workspace, TaskSourceKind::UserInput, task, cx);
            }
        })
        .detach();
    }

    pub fn rerun_failed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let failed: Vec<TestNode> = self
            .all_nodes()
            .into_iter()
            .filter(|node| {
                node.is_leaf()
                    && node.status == Some(RunnableTaskStatus::Failed)
                    && node.source.is_some()
            })
            .cloned()
            .collect();
        if failed.is_empty() {
            return;
        }
        self.open_run_group("Failed tests".into(), failed.len(), cx);
        self.pending_nodes = failed.iter().map(|node| node.key.clone()).collect();
        self.apply_current_run(cx);
        self.rebuild_flat_entries();
        for node in &failed {
            self.schedule_node_task(node, window, cx);
        }
        cx.notify();
    }

    /// Finds the listed test a scheduled task belongs to, using the file and row the task
    /// context was built from.
    fn target_for_task(&self, task: &ResolvedTask) -> Option<TestKey> {
        let env = &task.resolved.env;
        let file = env.get(&VariableName::File.to_string())?;
        let row = env
            .get(&VariableName::Row.to_string())?
            .parse::<u32>()
            .ok()?
            .saturating_sub(1);
        let file_group = self.files.iter().find(|file_group| {
            file_group
                .abs_path
                .as_ref()
                .is_some_and(|abs_path| abs_path.to_string_lossy() == *file)
        })?;

        fn visit(nodes: &[TestNode], row: u32) -> Option<&TestNode> {
            for node in nodes {
                if node.source.is_none() {
                    continue;
                }
                if node.row == row {
                    return Some(node);
                }
                if node.row < row {
                    if let Some(found) = visit(&node.children, row) {
                        return Some(found);
                    }
                    let end_row = node
                        .children
                        .iter()
                        .map(|child| child.row)
                        .max()
                        .unwrap_or(node.row);
                    if row <= end_row {
                        return Some(node);
                    }
                }
            }
            None
        }
        let node = visit(&file_group.nodes, row)?;
        match &node.key {
            NodeKey::Runnable(key) => Some(key.clone()),
            NodeKey::Reported { .. } => None,
        }
    }

    /// Spawns a scheduled test task into a terminal owned by the panel, parses its output
    /// as it arrives, and resolves to the task's exit status once it has finished.
    fn spawn_process(
        &mut self,
        mut task: ResolvedTask,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Result<ExitStatus>>> {
        let runner = Runner::detect(&task.resolved.command_label);
        // Reporter scripts live on this machine, so they can only be used for local
        // projects; remote runs fall back to parsing the runner's verbose output.
        let reporter_dir = if self.project.read(cx).is_local() {
            test_report::write_reporter_files(&paths::data_dir().join("test_reporters")).log_err()
        } else {
            None
        };
        let setup = runner.reporter_setup(&task.resolved.args, reporter_dir.as_deref());
        let format = setup.format;
        Self::extend_task(&mut task, &setup.args);
        for (key, value) in setup.env {
            let value = match (key.as_str(), std::env::var_os(&key)) {
                ("PYTHONPATH", Some(existing)) if !existing.is_empty() => {
                    format!("{value}:{}", existing.to_string_lossy())
                }
                _ => value,
            };
            task.resolved.env.insert(key, value);
        }
        Self::disable_colors(&mut task);

        let workspace = self.workspace.clone();
        let project = self.project.clone();

        cx.spawn_in(window, async move |this, cx| {
            let (run_id, process_index) = this
                .update(cx, |this, cx| {
                    let target = this.target_for_task(&task);
                    let run_id = match this.consume_open_group_slot() {
                        Some(run_id) => run_id,
                        None => {
                            let label: SharedString = task.resolved_label.clone().into();
                            let run_id = this.open_run_group(label, 1, cx);
                            this.consume_open_group_slot();
                            run_id
                        }
                    };
                    let run = this
                        .runs
                        .iter_mut()
                        .find(|run| run.id == run_id)
                        .expect("run was just created or is still open");
                    run.processes.push(RunProcess {
                        task: task.clone(),
                        runner,
                        format,
                        target: target.clone(),
                        terminal: None,
                        report: TestReport::default(),
                        status: RunnableTaskStatus::Running,
                        _subscription: None,
                    });
                    let process_index = run.processes.len() - 1;
                    if let Some(target) = target
                        && run.processes.len() == 1
                    {
                        this.selected = Some(Selection::Node(NodeKey::Runnable(target)));
                    }
                    this.apply_current_run(cx);
                    this.rebuild_flat_entries();
                    cx.notify();
                    (run_id, process_index)
                })
                .ok()?;
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_panel::<Self>(window, cx);
                })
                .ok()?;

            let spawn_in_terminal = project.read_with(cx, |project, cx| {
                anyhow::ensure!(
                    !project.is_via_collab(),
                    "tests cannot be run in a shared project"
                );
                let is_windows = project.path_style(cx).is_windows();
                let remote_shell = project
                    .remote_client()
                    .and_then(|remote_client| remote_client.read(cx).shell());
                let shell = match remote_shell {
                    Some(remote_shell) if task.resolved.shell == Shell::System => {
                        Shell::Program(remote_shell)
                    }
                    _ => task.resolved.shell.clone(),
                };
                Ok(prepare_task_for_spawn(&task.resolved, &shell, is_windows))
            });
            let spawn_in_terminal = match spawn_in_terminal {
                Ok(spawn_in_terminal) => spawn_in_terminal,
                Err(error) => {
                    Self::finish_process(
                        &this,
                        run_id,
                        process_index,
                        RunnableTaskStatus::Failed,
                        cx,
                    );
                    return Some(Err(error));
                }
            };

            let terminal = project
                .update(cx, |project, cx| {
                    project.create_terminal_task(spawn_in_terminal, cx)
                })
                .await;
            let terminal = match terminal {
                Ok(terminal) => terminal,
                Err(error) => {
                    Self::finish_process(
                        &this,
                        run_id,
                        process_index,
                        RunnableTaskStatus::Failed,
                        cx,
                    );
                    return Some(Err(error));
                }
            };

            this.update(cx, |this, cx| {
                let subscription = cx.subscribe(&terminal, move |this, _, event, cx| {
                    if let terminal::Event::Wakeup = event {
                        this.schedule_reparse(cx);
                    }
                });
                if let Some(process) = this.process_mut(run_id, process_index) {
                    process.terminal = Some(terminal.clone());
                    process._subscription = Some(subscription);
                }
                cx.notify();
            })
            .ok()?;

            let exit_status = terminal
                .read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
                .await;
            Self::finish_process(
                &this,
                run_id,
                process_index,
                status_from_exit(exit_status),
                cx,
            );
            exit_status.map(Ok)
        })
    }

    fn disable_colors(task: &mut ResolvedTask) {
        for (key, value) in [
            ("NO_COLOR", "1"),
            ("FORCE_COLOR", "0"),
            ("CARGO_TERM_COLOR", "never"),
        ] {
            task.resolved.env.insert(key.to_owned(), value.to_owned());
        }
    }

    fn extend_task(task: &mut ResolvedTask, args: &[String]) {
        if args.is_empty() {
            return;
        }
        task.resolved.args.extend(args.iter().cloned());
        task.resolved.command_label = format!("{} {}", task.resolved.command_label, args.join(" "));
    }

    fn process_mut(&mut self, run_id: usize, process_index: usize) -> Option<&mut RunProcess> {
        self.runs
            .iter_mut()
            .find(|run| run.id == run_id)?
            .processes
            .get_mut(process_index)
    }

    fn finish_process(
        this: &WeakEntity<Self>,
        run_id: usize,
        process_index: usize,
        status: RunnableTaskStatus,
        cx: &mut AsyncWindowContext,
    ) {
        this.update(cx, |this, cx| {
            this.reparse_reports(cx);
            let unreported_target = match this.process_mut(run_id, process_index) {
                Some(process) => {
                    process.status = status;
                    let reported = process
                        .report
                        .entries
                        .iter()
                        .any(|entry| entry.kind == EntryKind::Test);
                    process.target.clone().filter(|_| !reported)
                }
                None => None,
            };
            this.apply_current_run(cx);
            // A test that ran but was never reported still gets the process outcome.
            if let Some(target) = unreported_target
                && let Some(node) = this.node_by_key_mut(&NodeKey::Runnable(target))
            {
                node.status = Some(status);
                this.sync_editor_statuses(cx);
            }
            this.rebuild_flat_entries();
            cx.notify();
        })
        .log_err();
    }

    fn schedule_reparse(&mut self, cx: &mut Context<Self>) {
        // A pass that took long is followed by a proportionally longer pause, so that
        // parsing a large run's output never monopolizes the main thread.
        let delay = REPARSE_DEBOUNCE.max(self.last_reparse_duration * 4);
        self.reparse_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |this, cx| {
                let started = Instant::now();
                let changed = this.reparse_reports(cx);
                log_slow("parsing run output", started);
                if changed {
                    let started = Instant::now();
                    this.apply_current_run(cx);
                    log_slow("applying results", started);
                    let started = Instant::now();
                    this.rebuild_flat_entries();
                    log_slow("flattening the tree", started);
                    cx.notify();
                }
                this.last_reparse_duration = started.elapsed();
            })
            .ok();
        });
    }

    /// Re-parses the terminal output of the current run's processes. Returns whether any
    /// report changed.
    fn reparse_reports(&mut self, cx: &App) -> bool {
        let Some(run_index) = self.current_run else {
            return false;
        };
        let mut changed = false;
        for process in &mut self.runs[run_index].processes {
            let Some(terminal) = &process.terminal else {
                continue;
            };
            let content = terminal.read(cx).get_content();
            let content = content.trim_end_matches(|c| c == '\n' || c == ' ');
            let report = process.runner.parse(content, process.format);
            if report != process.report {
                process.report = report;
                changed = true;
            }
        }
        changed
    }

    // --- Selection and output --------------------------------------------------------------

    fn select(&mut self, selection: Selection, cx: &mut Context<Self>) {
        self.selected = Some(selection);
        cx.notify();
    }

    fn select_node(&mut self, key: NodeKey, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(Selection::Node(key.clone()));
        cx.notify();
        self.jump_to_node(&key, false, window, cx);
    }

    fn jump_to_node(
        &mut self,
        key: &NodeKey,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(node) = self.node_by_key(key) else {
            return;
        };
        let Some(source) = node.source.clone() else {
            return;
        };
        let row = node.row;
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        match source {
            NodeSource::Open(source) => {
                workspace.update(cx, |workspace, cx| {
                    workspace.activate_item(&source.editor, true, focus, window, cx);
                });
                Self::select_row(&source.editor, row, window, cx);
            }
            NodeSource::Indexed(source) => {
                let open = workspace.update(cx, |workspace, cx| {
                    workspace.open_path(source.project_path.clone(), None, focus, window, cx)
                });
                cx.spawn_in(window, async move |_, cx| {
                    let item = open.await.log_err()?;
                    let editor = cx.update(|_, cx| item.act_as::<Editor>(cx)).ok()??;
                    cx.update(|window, cx| Self::select_row(&editor, row, window, cx))
                        .ok()
                })
                .detach();
            }
        }
    }

    fn select_row(editor: &Entity<Editor>, row: u32, window: &mut Window, cx: &mut App) {
        let position = Point::new(row, 0);
        editor.update(cx, |editor, cx| {
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_ranges([position..position]),
            );
        });
    }

    fn selected_output(&self) -> Option<SelectedOutput> {
        let run = self.current_run();
        match self.selected.as_ref()? {
            Selection::Run => {
                let run = run?;
                Some(SelectedOutput {
                    name: run.label.clone(),
                    status: Some(run.status()),
                    ignored: false,
                    duration: None,
                    output: run.run_output(),
                    run_ref: Some((run.id, 0)),
                })
            }
            Selection::File(key) => {
                let file = self.files.iter().find(|file| &file.key == key)?;
                let mut output = String::new();
                fn collect(nodes: &[TestNode], output: &mut String) {
                    for node in nodes {
                        if !node.output.is_empty() {
                            output.push_str(&format!("--- {} ---\n{}\n", node.name, node.output));
                        }
                        collect(&node.children, output);
                    }
                }
                collect(&file.nodes, &mut output);
                Some(SelectedOutput {
                    name: file.path.clone(),
                    status: None,
                    ignored: false,
                    duration: None,
                    output,
                    run_ref: None,
                })
            }
            Selection::Node(key) => {
                let node = self.node_by_key(key)?;
                Some(SelectedOutput {
                    name: node.name.clone(),
                    status: node.status,
                    ignored: node.skipped || node.ignored,
                    duration: node.duration,
                    output: node.output.clone(),
                    run_ref: node.run_ref,
                })
            }
        }
    }

    fn update_output_editor(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(selection) = self.selected.clone() else {
            return;
        };
        let full = self.show_full_output;
        let same_view =
            |displayed: &(Selection, bool, String)| displayed.0 == selection && displayed.1 == full;
        if self
            .displayed_output
            .as_ref()
            .is_some_and(|displayed| same_view(displayed) && displayed.2 == text)
        {
            return;
        }
        let follow_output = self.displayed_output.as_ref().is_some_and(|displayed| {
            same_view(displayed) && text.starts_with(displayed.2.as_str())
        });
        self.output_editor.update(cx, |editor, cx| {
            editor.set_text(text.to_owned(), window, cx);
            if follow_output {
                editor.move_to_end(&MoveToEnd, window, cx);
            }
        });
        self.displayed_output = Some((selection, full, text.to_owned()));
    }

    /// The complete, unparsed output of the run a selection belongs to, with one section
    /// per process.
    fn full_run_output(&self, run_ref: Option<(usize, usize)>, cx: &App) -> String {
        let run = run_ref
            .and_then(|(run_id, _)| self.runs.iter().find(|run| run.id == run_id))
            .or_else(|| self.current_run());
        let Some(run) = run else {
            return String::new();
        };
        let mut output = String::new();
        for process in &run.processes {
            if run.processes.len() > 1 {
                output.push_str(&format!("$ {}\n", process.task.resolved.command_label));
            }
            if let Some(terminal) = &process.terminal {
                let content = terminal.read(cx).get_content();
                output.push_str(&test_report::strip_ansi(content.trim_end()));
                output.push('\n');
            }
        }
        output
    }

    fn copy_name(&self, key: &NodeKey, cx: &mut Context<Self>) {
        if let Some(node) = self.node_by_key(key) {
            cx.write_to_clipboard(ClipboardItem::new_string(node.name.to_string()));
        }
    }

    // --- Context menu --------------------------------------------------------------------------

    fn deploy_context_menu(
        &mut self,
        position: GpuiPoint<Pixels>,
        entry: PanelEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = cx.weak_entity();
        let node = match &entry {
            PanelEntry::Node { file_index, path } => self.node_at(*file_index, path).cloned(),
            _ => None,
        };
        let file_index = match &entry {
            PanelEntry::File { file_index } => Some(*file_index),
            _ => None,
        };
        let file = file_index.and_then(|index| self.files.get(index));
        let file_key = file.map(|file| file.key.clone());
        let can_run_file = file.is_some_and(|file| !matches!(file.key, FileKey::Synthetic(_)));
        if let Some(node) = &node {
            self.selected = Some(Selection::Node(node.key.clone()));
        } else if let Some(file_key) = file_key {
            self.selected = Some(Selection::File(file_key));
        } else if matches!(entry, PanelEntry::Run) {
            self.selected = Some(Selection::Run);
        }

        let context_menu = ContextMenu::build(window, cx, |menu, _, _| {
            let menu = menu.context(self.focus_handle.clone());
            match (&entry, &node) {
                (PanelEntry::Run, _) => menu
                    .action("Rerun All Tests", Box::new(RerunAllTests))
                    .action("Rerun Failed Tests", Box::new(RerunFailedTests)),
                (PanelEntry::File { .. }, _) => menu
                    .when(can_run_file, |menu| {
                        menu.entry("Run Tests in File", None, {
                            let panel = panel.clone();
                            move |window, cx| {
                                if let Some(file_index) = file_index {
                                    panel
                                        .update(cx, |panel, cx| {
                                            panel.run_file(file_index, window, cx)
                                        })
                                        .ok();
                                }
                            }
                        })
                    })
                    .separator()
                    .action("Expand All", Box::new(ExpandAllEntries))
                    .action("Collapse All", Box::new(CollapseAllEntries)),
                (PanelEntry::Node { .. }, Some(node)) => {
                    let key = node.key.clone();
                    let runnable = node.source.is_some();
                    let run_label = if node.is_leaf() {
                        "Run Test"
                    } else {
                        "Run Test Group"
                    };
                    menu.when(runnable, |menu| {
                        menu.entry(run_label, None, {
                            let panel = panel.clone();
                            let key = key.clone();
                            move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.run_node(&key, window, cx))
                                    .ok();
                            }
                        })
                        .entry("Jump to Source", None, {
                            let panel = panel.clone();
                            let key = key.clone();
                            move |window, cx| {
                                panel
                                    .update(cx, |panel, cx| {
                                        panel.jump_to_node(&key, true, window, cx)
                                    })
                                    .ok();
                            }
                        })
                    })
                    .entry("Copy Test Name", None, {
                        let panel = panel.clone();
                        move |_, cx| {
                            panel.update(cx, |panel, cx| panel.copy_name(&key, cx)).ok();
                        }
                    })
                    .separator()
                    .action("Rerun Failed Tests", Box::new(RerunFailedTests))
                }
                _ => menu,
            }
        });

        window.focus(&context_menu.focus_handle(cx), cx);
        let subscription = cx.subscribe(&context_menu, |this, _, _: &DismissEvent, cx| {
            this.context_menu.take();
            cx.notify();
        });
        self.context_menu = Some((context_menu, position, subscription));
        cx.notify();
    }

    // --- Rendering -------------------------------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let summary = self.summary();
        let total = self
            .all_nodes()
            .iter()
            .filter(|node| node.is_leaf())
            .count();
        let focus_handle = self.focus_handle.clone();
        let has_run = self.current_run.is_some();
        let has_failed = summary.failed > 0;
        let panel = cx.weak_entity();

        let tooltip = |title: &'static str, action: &'static dyn Action| {
            let focus_handle = focus_handle.clone();
            move |_: &mut Window, cx: &mut App| {
                Tooltip::for_action_in(title, action, &focus_handle, cx)
            }
        };

        h_flex()
            .px_1()
            .py_0p5()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .justify_between()
            .child(
                h_flex()
                    .gap_0p5()
                    .child(
                        IconButton::new("run-all-tests", IconName::PlayFilled)
                            .icon_size(IconSize::Small)
                            .disabled(total == 0)
                            .tooltip(tooltip("Run All Tests", &RunAllTests))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.run_all_tests(window, cx)),
                            ),
                    )
                    .child(
                        IconButton::new("rerun-all-tests", IconName::Rerun)
                            .icon_size(IconSize::Small)
                            .disabled(!has_run)
                            .tooltip(tooltip("Rerun All Tests", &RerunAllTests))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.rerun_all(window, cx)),
                            ),
                    )
                    .child(
                        IconButton::new("rerun-failed-tests", IconName::RotateCcw)
                            .icon_size(IconSize::Small)
                            .icon_color(if has_failed {
                                Color::Error
                            } else {
                                Color::Default
                            })
                            .disabled(!has_failed)
                            .tooltip(tooltip("Rerun Failed Tests", &RerunFailedTests))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.rerun_failed(window, cx)),
                            ),
                    )
                    .child(
                        IconButton::new("discover-tests", IconName::ListTree)
                            .icon_size(IconSize::Small)
                            .disabled(total == 0)
                            .tooltip(tooltip("Discover Tests", &DiscoverTests))
                            .on_click(cx.listener(|this, _, _, cx| this.discover_tests(cx))),
                    )
                    .child(Divider::vertical())
                    .child(
                        IconButton::new("show-passed-tests", IconName::Check)
                            .icon_size(IconSize::Small)
                            .toggle_state(self.show_passed)
                            .selected_icon_color(Color::Success)
                            .tooltip(tooltip("Show Passed Tests", &ToggleShowPassed))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_show_passed(cx))),
                    )
                    .child(
                        IconButton::new("show-failed-tests", IconName::XCircle)
                            .icon_size(IconSize::Small)
                            .toggle_state(self.show_failed)
                            .selected_icon_color(Color::Error)
                            .tooltip(tooltip("Show Failed Tests", &ToggleShowFailed))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_show_failed(cx))),
                    )
                    .child(
                        IconButton::new("show-ignored-tests", IconName::Circle)
                            .icon_size(IconSize::Small)
                            .toggle_state(self.show_ignored)
                            .tooltip(tooltip("Show Ignored Tests", &ToggleShowIgnored))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_show_ignored(cx))),
                    )
                    .child(Divider::vertical())
                    .child(self.render_sort_menu(panel.clone()))
                    .child(self.render_history_menu(panel))
                    .child(self.render_more_menu()),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .px_1()
                    .child(
                        Label::new(total.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when(summary.passed > 0, |this| {
                        this.child(
                            Label::new(format!("{} passed", summary.passed))
                                .size(LabelSize::Small)
                                .color(Color::Success),
                        )
                    })
                    .when(summary.failed > 0, |this| {
                        this.child(
                            Label::new(format!("{} failed", summary.failed))
                                .size(LabelSize::Small)
                                .color(Color::Error),
                        )
                    })
                    .when(summary.skipped > 0, |this| {
                        this.child(
                            Label::new(format!("{} skipped", summary.skipped))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(summary.running > 0, |this| {
                        this.child(
                            Label::new(format!("{} running", summary.running))
                                .size(LabelSize::Small)
                                .color(Color::Accent),
                        )
                    }),
            )
    }

    fn render_sort_menu(&self, panel: WeakEntity<Self>) -> impl IntoElement {
        let sort_order = self.sort_order;
        PopoverMenu::new("test-panel-sort")
            .trigger_with_tooltip(
                IconButton::new("test-panel-sort-trigger", IconName::ArrowDown10)
                    .icon_size(IconSize::Small),
                Tooltip::text(format!("Sort: {}", sort_order.label())),
            )
            .anchor(Anchor::TopLeft)
            .with_handle(self.sort_menu_handle.clone())
            .menu(move |window, cx| {
                let panel = panel.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    for option in [
                        SortOrder::Alphabetical,
                        SortOrder::Declaration,
                        SortOrder::Duration,
                    ] {
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            option.label(),
                            sort_order == option,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.set_sort_order(option, cx))
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_history_menu(&self, panel: WeakEntity<Self>) -> impl IntoElement {
        let runs: Vec<(usize, SharedString, String, RunnableTaskStatus)> = self
            .runs
            .iter()
            .enumerate()
            .rev()
            .map(|(index, run)| {
                (
                    index,
                    run.label.clone(),
                    run.started_at.format("%H:%M:%S").to_string(),
                    run.status(),
                )
            })
            .collect();
        let current_run = self.current_run;
        PopoverMenu::new("test-panel-history")
            .trigger_with_tooltip(
                IconButton::new("test-panel-history-trigger", IconName::HistoryRerun)
                    .icon_size(IconSize::Small)
                    .disabled(runs.is_empty()),
                Tooltip::text("Test History"),
            )
            .anchor(Anchor::TopLeft)
            .with_handle(self.history_menu_handle.clone())
            .menu(move |window, cx| {
                let panel = panel.clone();
                let runs = runs.clone();
                Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                    menu = menu.header("Recent Runs");
                    for (index, label, time, status) in runs {
                        let status_label = match status {
                            RunnableTaskStatus::Running => "running",
                            RunnableTaskStatus::Passed => "passed",
                            RunnableTaskStatus::Failed => "failed",
                        };
                        let panel = panel.clone();
                        menu = menu.toggleable_entry(
                            format!("{time}  {label}  ({status_label})"),
                            current_run == Some(index),
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                panel
                                    .update(cx, |panel, cx| panel.select_run(index, cx))
                                    .ok();
                            },
                        );
                    }
                    menu
                }))
            })
    }

    fn render_more_menu(&self) -> impl IntoElement {
        let focus_handle = self.focus_handle.clone();
        PopoverMenu::new("test-panel-more")
            .trigger_with_tooltip(
                IconButton::new("test-panel-more-trigger", IconName::Ellipsis)
                    .icon_size(IconSize::Small),
                Tooltip::text("More Options"),
            )
            .anchor(Anchor::TopLeft)
            .with_handle(self.more_menu_handle.clone())
            .menu(move |window, cx| {
                let focus_handle = focus_handle.clone();
                Some(ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.context(focus_handle)
                        .action("Expand All", Box::new(ExpandAllEntries))
                        .action("Collapse All", Box::new(CollapseAllEntries))
                        .separator()
                        .action("Discover Tests", Box::new(DiscoverTests))
                        .action("Toggle Full Output", Box::new(ToggleFullOutput))
                }))
            })
    }

    fn render_status_icon(
        status: Option<RunnableTaskStatus>,
        ignored: bool,
        index: usize,
    ) -> AnyElement {
        match status {
            None if ignored => Icon::new(IconName::Circle)
                .size(IconSize::Small)
                .color(Color::Disabled)
                .into_any_element(),
            None => Icon::new(IconName::Circle)
                .size(IconSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            Some(RunnableTaskStatus::Running) => Icon::new(IconName::ArrowCircle)
                .size(IconSize::Small)
                .color(Color::Accent)
                .with_keyed_rotate_animation(("test-running", index), 2)
                .into_any_element(),
            Some(RunnableTaskStatus::Passed) => Icon::new(IconName::Check)
                .size(IconSize::Small)
                .color(Color::Success)
                .into_any_element(),
            Some(RunnableTaskStatus::Failed) => Icon::new(IconName::XCircle)
                .size(IconSize::Small)
                .color(Color::Error)
                .into_any_element(),
        }
    }

    fn render_run_entry(&self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let run = self.current_run()?;
        let is_selected = self.selected == Some(Selection::Run);
        let entry = PanelEntry::Run;
        Some(
            ListItem::new(("test-run", index))
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(is_selected)
                .start_slot(Self::render_status_icon(Some(run.status()), false, index))
                .child(
                    h_flex()
                        .gap_1p5()
                        .min_w_0()
                        .child(Label::new(run.label.clone()).truncate())
                        .child(
                            Label::new(run.started_at.format("%H:%M:%S").to_string())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .on_click(cx.listener(|this, _, _, cx| this.select(Selection::Run, cx)))
                .on_secondary_mouse_down(cx.listener(
                    move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.deploy_context_menu(event.position, entry.clone(), window, cx);
                    },
                ))
                .into_any_element(),
        )
    }

    fn render_file_entry(
        &self,
        index: usize,
        file_index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let file = self.files.get(file_index)?;
        let collapse_key = Self::file_collapse_key(file);
        let is_expanded = !self.collapsed.contains(&collapse_key);
        let is_selected = self.selected == Some(Selection::File(file.key.clone()));
        let mut summary = StatusSummary::default();
        let mut total = 0;
        fn visit(nodes: &[TestNode], summary: &mut StatusSummary, total: &mut usize) {
            for node in nodes {
                if node.is_leaf() {
                    summary.record(node);
                    *total += 1;
                }
                visit(&node.children, summary, total);
            }
        }
        visit(&file.nodes, &mut summary, &mut total);
        let file_key = file.key.clone();
        let entry = PanelEntry::File { file_index };
        let toggle_key = collapse_key.clone();

        Some(
            ListItem::new(("test-file", index))
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(is_selected)
                .start_slot(
                    Disclosure::new(("test-file-disclosure", index), is_expanded).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.toggle_collapsed(toggle_key.clone(), cx)
                        }),
                    ),
                )
                .child(
                    h_flex()
                        .gap_1p5()
                        .min_w_0()
                        .child(Label::new(file.path.clone()).truncate())
                        .child(
                            Label::new(total.to_string())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when(summary.failed > 0, |this| {
                            this.child(
                                Icon::new(IconName::XCircle)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            )
                        })
                        .when(
                            summary.failed == 0 && summary.passed > 0 && summary.running == 0,
                            |this| {
                                this.child(
                                    Icon::new(IconName::Check)
                                        .size(IconSize::Small)
                                        .color(Color::Success),
                                )
                            },
                        ),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select(Selection::File(file_key.clone()), cx);
                    this.toggle_collapsed(collapse_key.clone(), cx);
                }))
                .on_secondary_mouse_down(cx.listener(
                    move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.deploy_context_menu(event.position, entry.clone(), window, cx);
                    },
                ))
                .into_any_element(),
        )
    }

    fn render_node_entry(
        &self,
        index: usize,
        file_index: usize,
        path: &[usize],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let node = self.node_at(file_index, path)?;
        let key = node.key.clone();
        let is_selected = self.selected == Some(Selection::Node(key.clone()));
        let is_group = !node.children.is_empty();
        let is_expanded = !self.collapsed.contains(&key);
        let ignored = node.ignored || node.skipped;
        let entry = PanelEntry::Node {
            file_index,
            path: path.to_vec(),
        };
        let select_key = key.clone();
        let toggle_key = key;

        Some(
            ListItem::new(("test", index))
                .spacing(ListItemSpacing::Sparse)
                .indent_level(path.len())
                .indent_step_size(px(16.))
                .toggle_state(is_selected)
                .start_slot(
                    h_flex()
                        .gap_1()
                        .when(is_group, |this| {
                            this.child(
                                Disclosure::new(("test-group-disclosure", index), is_expanded)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_collapsed(toggle_key.clone(), cx)
                                    })),
                            )
                        })
                        .child(Self::render_status_icon(node.status, ignored, index)),
                )
                .child(
                    h_flex()
                        .gap_1p5()
                        .min_w_0()
                        .child(
                            Label::new(node.name.clone())
                                .truncate()
                                .when(ignored, |this| this.color(Color::Muted)),
                        )
                        .when_some(node.duration, |this, duration| {
                            this.child(
                                Label::new(format_duration(duration))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select_node(select_key.clone(), window, cx)
                }))
                .on_secondary_mouse_down(cx.listener(
                    move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.deploy_context_menu(event.position, entry.clone(), window, cx);
                    },
                ))
                .into_any_element(),
        )
    }

    fn render_entry(&self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        match self.entries.get(index)?.clone() {
            PanelEntry::Run => self.render_run_entry(index, cx),
            PanelEntry::File { file_index } => self.render_file_entry(index, file_index, cx),
            PanelEntry::Node { file_index, path } => {
                self.render_node_entry(index, file_index, &path, cx)
            }
        }
    }

    fn render_test_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.entries.is_empty() {
            let (title, detail) = if self.index.read(cx).is_scanning() {
                (
                    "Indexing tests",
                    "The project's files are being scanned for tests.",
                )
            } else {
                (
                    "No tests found",
                    "Tests of open files and of the project index are listed here.",
                )
            };
            return self.render_placeholder(title, detail).into_any_element();
        }

        uniform_list(
            "test-panel-entries",
            self.entries.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| {
                range
                    .filter_map(|index| this.render_entry(index, cx))
                    .collect()
            }),
        )
        .size_full()
        .track_scroll(&self.scroll_handle)
        .into_any_element()
    }

    fn render_output(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(selected) = self.selected_output() else {
            return self
                .render_placeholder(
                    "No test selected",
                    "Select a test to see the output of its last run.",
                )
                .into_any_element();
        };
        let text = if self.show_full_output {
            self.full_run_output(selected.run_ref, cx)
        } else {
            selected.output.clone()
        };
        self.update_output_editor(&text, window, cx);
        let focus_handle = self.focus_handle.clone();

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_0p5()
                    .gap_1p5()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .min_w_0()
                            .child(Self::render_status_icon(
                                selected.status,
                                selected.ignored,
                                usize::MAX,
                            ))
                            .child(Label::new(selected.name).truncate())
                            .when_some(selected.duration, |this, duration| {
                                this.child(
                                    Label::new(format_duration(duration))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        IconButton::new("toggle-full-output", IconName::Terminal)
                            .icon_size(IconSize::Small)
                            .toggle_state(self.show_full_output)
                            .tooltip(move |_, cx| {
                                Tooltip::for_action_in(
                                    "Show Full Run Output",
                                    &ToggleFullOutput,
                                    &focus_handle,
                                    cx,
                                )
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_full_output = !this.show_full_output;
                                cx.notify();
                            })),
                    ),
            )
            .child(if text.trim().is_empty() {
                self.render_placeholder(
                    "No output",
                    "The runner reported no output for this selection.",
                )
                .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_h_0()
                    .size_full()
                    .child(self.output_editor.clone())
                    .into_any_element()
            })
            .into_any_element()
    }

    fn render_placeholder(&self, title: &'static str, detail: &'static str) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .p_4()
            .gap_1()
            .child(Label::new(title).color(Color::Muted).size(LabelSize::Small))
            .child(
                Label::new(detail)
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            )
    }

    fn render_splitter(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("test-panel-splitter")
            .h_full()
            .w(px(5.))
            .flex_none()
            .cursor_col_resize()
            .border_l_1()
            .border_color(cx.theme().colors().border)
            .hover(|style| style.bg(cx.theme().colors().border_focused))
            .on_drag(DraggedSplitter, |_, _, _, cx| cx.new(|_| DraggedSplitter))
    }

    fn resize_list(&mut self, event: &DragMoveEvent<DraggedSplitter>, cx: &mut Context<Self>) {
        let available = event.bounds.size.width;
        let max_width = (available - MIN_PANE_WIDTH).max(MIN_PANE_WIDTH);
        let width = (event.event.position.x - event.bounds.left()).clamp(MIN_PANE_WIDTH, max_width);
        if self.list_width != Some(width) {
            self.list_width = Some(width);
            cx.notify();
        }
    }
}

impl Focusable for TestPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TestPanel {}

impl Render for TestPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_width = self.list_width.unwrap_or(DEFAULT_LIST_WIDTH);
        let output = self.render_output(window, cx);

        v_flex()
            .id("test-panel")
            .key_context("TestPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .on_action(
                cx.listener(|this, _: &RunAllTests, window, cx| this.run_all_tests(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RerunAllTests, window, cx| this.rerun_all(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RerunFailedTests, window, cx| this.rerun_failed(window, cx)),
            )
            .on_action(cx.listener(|this, _: &DiscoverTests, _, cx| this.discover_tests(cx)))
            .on_action(cx.listener(|this, _: &ToggleShowPassed, _, cx| this.toggle_show_passed(cx)))
            .on_action(cx.listener(|this, _: &ToggleShowFailed, _, cx| this.toggle_show_failed(cx)))
            .on_action(
                cx.listener(|this, _: &ToggleShowIgnored, _, cx| this.toggle_show_ignored(cx)),
            )
            .on_action(cx.listener(|this, _: &ExpandAllEntries, _, cx| this.expand_all(cx)))
            .on_action(cx.listener(|this, _: &CollapseAllEntries, _, cx| this.collapse_all(cx)))
            .on_action(cx.listener(|this, _: &ToggleFullOutput, _, cx| {
                this.show_full_output = !this.show_full_output;
                cx.notify();
            }))
            .child(self.render_toolbar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .size_full()
                    .on_drag_move(cx.listener(
                        |this, event: &DragMoveEvent<DraggedSplitter>, _, cx| {
                            this.resize_list(event, cx)
                        },
                    ))
                    .child(
                        div()
                            .h_full()
                            .w(list_width)
                            .flex_none()
                            .min_w(MIN_PANE_WIDTH)
                            .child(self.render_test_list(cx)),
                    )
                    .child(self.render_splitter(cx))
                    .child(div().h_full().flex_1().min_w_0().child(output)),
            )
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .anchor(Anchor::TopLeft)
                        .child(menu.clone()),
                )
                .with_priority(3)
            }))
    }
}

impl Panel for TestPanel {
    fn persistent_name() -> &'static str {
        "TestPanel"
    }

    fn panel_key() -> &'static str {
        TEST_PANEL_KEY
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        TestPanelSettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock: DockPositionSetting = position.into();
            settings.test_panel.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = TestPanelSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        TestPanelSettings::get_global(cx)
            .button
            .then_some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Test Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }

    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.schedule_update(cx);
        }
    }

    fn hide_button_setting(&self, _: &App) -> Option<HideStatusItem> {
        Some(HideStatusItem::new(|settings| {
            settings.test_panel.get_or_insert_default().button = Some(false);
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(label: &str, command: &str, args: &[&str], tags: &[&str]) -> TaskTemplate {
        TaskTemplate {
            label: label.to_owned(),
            command: command.to_owned(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            ..TaskTemplate::default()
        }
    }

    fn javascript_templates() -> Vec<(TaskSourceKind, TaskTemplate)> {
        let runner = "$ZED_CUSTOM_TYPESCRIPT_RUNNER";
        [
            template(
                "vitest file test",
                runner,
                &[
                    "exec",
                    "--",
                    "vitest",
                    "run",
                    "--no-file-parallelism",
                    "$ZED_FILE",
                ],
                &[],
            ),
            template(
                "type-check-tests",
                runner,
                &["run", "type-check-tests"],
                &["package-script"],
            ),
            template(
                "vitest test $ZED_SYMBOL",
                runner,
                &[
                    "exec",
                    "--",
                    "vitest",
                    "run",
                    "--testNamePattern",
                    "\"$ZED_CUSTOM_TYPESCRIPT_VITEST_TEST_NAME\"",
                    "$ZED_FILE",
                ],
                &["js-test"],
            ),
            template("node test file test", "node", &["--test", "$ZED_FILE"], &[]),
            template(
                "node test $ZED_SYMBOL",
                "node",
                &[
                    "--test",
                    "--test-name-pattern",
                    "\"$ZED_SYMBOL\"",
                    "$ZED_FILE",
                ],
                &["js-test"],
            ),
        ]
        .into_iter()
        .map(|template| (TaskSourceKind::UserInput, template))
        .collect()
    }

    #[test]
    fn prefers_the_package_manager_runner_for_a_file() {
        let templates = javascript_templates();
        let node_templates: Vec<_> = templates
            .iter()
            .filter(|(_, template)| template.tags.contains(&"js-test".to_owned()))
            .cloned()
            .collect();
        let (_, picked) =
            pick_scoped_template(&templates, &node_templates, RunScope::File).unwrap();
        assert_eq!(picked.label, "vitest file test");
        assert!(mentions_file(&picked));
    }

    #[test]
    fn drops_the_file_argument_for_a_project_run() {
        let templates = javascript_templates();
        let node_templates: Vec<_> = templates
            .iter()
            .filter(|(_, template)| template.tags.contains(&"js-test".to_owned()))
            .cloned()
            .collect();
        let (_, picked) =
            pick_scoped_template(&templates, &node_templates, RunScope::Project).unwrap();
        assert_eq!(picked.args, vec!["exec", "--", "vitest", "run"]);
        assert!(!mentions_file(&picked));
    }

    #[test]
    fn prefers_module_wide_templates_for_a_project_run() {
        let templates: Vec<_> = [
            template("go test", "go", &["test"], &[]),
            template("go test ./...", "go", &["test", "./..."], &[]),
            template(
                "go test $ZED_SYMBOL",
                "go",
                &["test", "-run", "\\^$ZED_SYMBOL\\$"],
                &["go-test"],
            ),
        ]
        .into_iter()
        .map(|template| (TaskSourceKind::UserInput, template))
        .collect();
        let node_templates = vec![templates[2].clone()];
        let (_, project) =
            pick_scoped_template(&templates, &node_templates, RunScope::Project).unwrap();
        assert_eq!(project.label, "go test ./...");
        let (_, file) = pick_scoped_template(&templates, &node_templates, RunScope::File).unwrap();
        assert_eq!(file.label, "go test");
    }
}
