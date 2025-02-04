use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use collections::{BTreeMap, HashMap};
use gpui::{AppContext, Model, ModelContext, SharedString, Task, WeakView};
use language::Buffer;
use project::{ProjectPath, Worktree};
use text::BufferId;
use workspace::Workspace;

use crate::context::{
    Context, ContextId, ContextKind, ContextSnapshot, DirectoryContext, FetchedUrlContext,
    FileContext, ThreadContext,
};
use crate::thread::{Thread, ThreadId};

pub struct ContextStore {
    workspace: WeakView<Workspace>,
    context: Vec<Context>,
    // TODO: If an EntityId is used for all context types (like BufferId), can remove ContextId.
    next_context_id: ContextId,
    files: BTreeMap<BufferId, ContextId>,
    directories: HashMap<PathBuf, ContextId>,
    threads: HashMap<ThreadId, ContextId>,
    fetched_urls: HashMap<String, ContextId>,
}

impl ContextStore {
    pub fn new(workspace: WeakView<Workspace>) -> Self {
        Self {
            workspace,
            context: Vec::new(),
            next_context_id: ContextId(0),
            files: BTreeMap::default(),
            directories: HashMap::default(),
            threads: HashMap::default(),
            fetched_urls: HashMap::default(),
        }
    }

    pub fn snapshot<'a>(
        &'a self,
        cx: &'a AppContext,
    ) -> impl Iterator<Item = ContextSnapshot> + 'a {
        self.context()
            .iter()
            .flat_map(|context| context.snapshot(cx))
    }

    pub fn context(&self) -> &Vec<Context> {
        &self.context
    }

    pub fn clear(&mut self) {
        self.context.clear();
        self.files.clear();
        self.directories.clear();
        self.threads.clear();
        self.fetched_urls.clear();
    }

    pub fn add_file(
        &mut self,
        project_path: ProjectPath,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<()>> {
        let workspace = self.workspace.clone();
        let Some(project) = workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().clone())
        else {
            return Task::ready(Err(anyhow!("failed to read project")));
        };

        cx.spawn(|this, mut cx| async move {
            let open_buffer_task = project.update(&mut cx, |project, cx| {
                project.open_buffer(project_path.clone(), cx)
            })?;

            let buffer = open_buffer_task.await?;
            let buffer_id = buffer.update(&mut cx, |buffer, _cx| buffer.remote_id())?;

            let already_included = this.update(&mut cx, |this, _cx| {
                match this.will_include_buffer(buffer_id, &project_path.path) {
                    Some(FileInclusion::Direct(context_id)) => {
                        this.remove_context(context_id);
                        true
                    }
                    Some(FileInclusion::InDirectory(_)) => true,
                    None => false,
                }
            })?;

            if already_included {
                return anyhow::Ok(());
            }

            this.update(&mut cx, |this, cx| {
                this.insert_file(buffer, cx);
            })?;

            anyhow::Ok(())
        })
    }

    pub fn insert_file(&mut self, buffer_model: Model<Buffer>, cx: &AppContext) {
        let buffer = buffer_model.read(cx);
        let Some(file) = buffer.file() else {
            return;
        };

        let mut text = String::new();
        push_fenced_codeblock(file.path(), buffer.text(), &mut text);

        let id = self.next_context_id.post_inc();
        self.files.insert(buffer.remote_id(), id);
        self.context.push(Context::File(FileContext {
            id,
            buffer: buffer_model,
            version: buffer.version.clone(),
            text: text.into(),
        }));
    }

    pub fn add_directory(
        &mut self,
        project_path: ProjectPath,
        cx: &mut ModelContext<Self>,
    ) -> Task<Result<()>> {
        let workspace = self.workspace.clone();
        let Some(project) = workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().clone())
        else {
            return Task::ready(Err(anyhow!("failed to read project")));
        };

        let already_included = if let Some(context_id) = self.includes_directory(&project_path.path)
        {
            self.remove_context(context_id);
            true
        } else {
            false
        };
        if already_included {
            return Task::ready(Ok(()));
        }

        let worktree_id = project_path.worktree_id;
        cx.spawn(|this, mut cx| async move {
            let worktree = project.update(&mut cx, |project, cx| {
                project
                    .worktree_for_id(worktree_id, cx)
                    .ok_or_else(|| anyhow!("no worktree found for {worktree_id:?}"))
            })??;

            let files = worktree.update(&mut cx, |worktree, _cx| {
                collect_files_in_path(worktree, &project_path.path)
            })?;

            let open_buffer_tasks = project.update(&mut cx, |project, cx| {
                files
                    .into_iter()
                    .map(|file_path| {
                        project.open_buffer(
                            ProjectPath {
                                worktree_id,
                                path: file_path.clone(),
                            },
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            })?;

            let buffers = futures::future::join_all(open_buffer_tasks).await;

            this.update(&mut cx, |this, cx| {
                let mut text = String::new();
                let mut directory_buffers = BTreeMap::new();
                for buffer_model in buffers {
                    let buffer_model = buffer_model?;
                    let buffer = buffer_model.read(cx);
                    let path = buffer.file().map_or(&project_path.path, |file| file.path());
                    push_fenced_codeblock(&path, buffer.text(), &mut text);
                    directory_buffers
                        .insert(buffer.remote_id(), (buffer_model, buffer.version.clone()));
                }

                if directory_buffers.is_empty() {
                    bail!(
                        "could not read any text files from {}",
                        &project_path.path.display()
                    );
                }

                this.insert_directory(&project_path.path, directory_buffers, text);

                anyhow::Ok(())
            })??;

            anyhow::Ok(())
        })
    }

    pub fn insert_directory(
        &mut self,
        path: &Path,
        buffers: BTreeMap<BufferId, (Model<Buffer>, clock::Global)>,
        text: impl Into<SharedString>,
    ) {
        let id = self.next_context_id.post_inc();
        self.directories.insert(path.to_path_buf(), id);

        let full_path: SharedString = path.to_string_lossy().into_owned().into();

        let name = match path.file_name() {
            Some(name) => name.to_string_lossy().into_owned().into(),
            None => full_path.clone(),
        };

        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|p| p.to_string_lossy().into_owned().into());

        self.context.push(Context::Directory(DirectoryContext {
            path: path.into(),
            buffers,
            snapshot: ContextSnapshot {
                id,
                name,
                parent,
                tooltip: Some(full_path),
                kind: ContextKind::Directory,
                text: text.into(),
            },
        }));
    }

    pub fn add_thread(&mut self, thread: Model<Thread>, cx: &mut ModelContext<Self>) {
        if let Some(context_id) = self.includes_thread(&thread.read(cx).id()) {
            self.remove_context(context_id);
        } else {
            self.insert_thread(thread, cx);
        }
    }

    pub fn insert_thread(&mut self, thread: Model<Thread>, cx: &AppContext) {
        let id = self.next_context_id.post_inc();
        let thread_ref = thread.read(cx);
        let text = thread_ref.text().into();

        self.threads.insert(thread_ref.id().clone(), id);
        self.context
            .push(Context::Thread(ThreadContext { id, thread, text }));
    }

    pub fn insert_fetched_url(&mut self, url: String, text: impl Into<SharedString>) {
        let id = self.next_context_id.post_inc();

        self.fetched_urls.insert(url.clone(), id);
        self.context.push(Context::FetchedUrl(FetchedUrlContext {
            id,
            url: url.into(),
            text: text.into(),
        }));
    }

    pub fn remove_context(&mut self, id: ContextId) {
        let Some(ix) = self.context.iter().position(|context| context.id() == id) else {
            return;
        };

        match self.context.remove(ix) {
            Context::File(_) => {
                self.files.retain(|_, context_id| *context_id != id);
            }
            Context::Directory(_) => {
                self.directories.retain(|_, context_id| *context_id != id);
            }
            Context::FetchedUrl(_) => {
                self.fetched_urls.retain(|_, context_id| *context_id != id);
            }
            Context::Thread(_) => {
                self.threads.retain(|_, context_id| *context_id != id);
            }
        }
    }

    /// Returns whether the buffer is already included directly in the context, or if it will be
    /// included in the context via a directory. Directory inclusion is based on paths rather than
    /// buffer IDs as the directory will be re-scanned.
    pub fn will_include_buffer(&self, buffer_id: BufferId, path: &Path) -> Option<FileInclusion> {
        if let Some(context_id) = self.files.get(&buffer_id) {
            return Some(FileInclusion::Direct(*context_id));
        }

        self.will_include_file_path_via_directory(path)
    }

    /// Returns whether this file path is already included directly in the context, or if it will be
    /// included in the context via a directory.
    pub fn will_include_file_path(&self, path: &Path, cx: &AppContext) -> Option<FileInclusion> {
        if !self.files.is_empty() {
            let found_file_context = self.context.iter().find(|context| match &context {
                Context::File(file_context) => {
                    if let Some(file_path) = file_context.path(cx) {
                        *file_path == *path
                    } else {
                        false
                    }
                }
                _ => false,
            });
            if let Some(context) = found_file_context {
                return Some(FileInclusion::Direct(context.id()));
            }
        }

        self.will_include_file_path_via_directory(path)
    }

    fn will_include_file_path_via_directory(&self, path: &Path) -> Option<FileInclusion> {
        if self.directories.is_empty() {
            return None;
        }

        let mut buf = path.to_path_buf();

        while buf.pop() {
            if let Some(_) = self.directories.get(&buf) {
                return Some(FileInclusion::InDirectory(buf));
            }
        }

        None
    }

    pub fn includes_directory(&self, path: &Path) -> Option<ContextId> {
        self.directories.get(path).copied()
    }

    pub fn includes_thread(&self, thread_id: &ThreadId) -> Option<ContextId> {
        self.threads.get(thread_id).copied()
    }

    pub fn includes_url(&self, url: &str) -> Option<ContextId> {
        self.fetched_urls.get(url).copied()
    }
}

pub enum FileInclusion {
    Direct(ContextId),
    InDirectory(PathBuf),
}

pub(crate) fn push_fenced_codeblock(path: &Path, content: String, buffer: &mut String) {
    buffer.reserve(content.len() + 64);

    write!(buffer, "```").unwrap();

    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        write!(buffer, "{} ", extension).unwrap();
    }

    write!(buffer, "{}", path.display()).unwrap();

    buffer.push('\n');
    buffer.push_str(&content);

    if !buffer.ends_with('\n') {
        buffer.push('\n');
    }

    buffer.push_str("```\n");
}

fn collect_files_in_path(worktree: &Worktree, path: &Path) -> Vec<Arc<Path>> {
    let mut files = Vec::new();

    for entry in worktree.child_entries(path) {
        if entry.is_dir() {
            files.extend(collect_files_in_path(worktree, &entry.path));
        } else if entry.is_file() {
            files.push(entry.path.clone());
        }
    }

    files
}
