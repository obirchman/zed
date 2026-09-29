//! Code navigation by name, for files no language server answers for: go
//! to definition looks the word under the cursor up in an index of the
//! project's symbols, built with each language's tree-sitter outline query;
//! find references searches the project for the word. The same approach
//! GitHub uses for its code navigation, so it needs no build setup.

use anyhow::Result;
use collections::HashMap;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, AsyncApp, Entity, EntityId, Global, Task, WeakEntity};
use language::{Buffer, Location, Point, Rope, ToOffset as _};
use project::{LocationLink, Project, ProjectPath, WorktreeId, search::SearchQuery, search::SearchResult};
use std::{ops::Range, sync::Arc};
use util::{paths::PathMatcher, rel_path::RelPath};

/// Files larger than this are not indexed.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Files parsed at once while indexing.
const PARALLEL_FILES: usize = 16;

#[derive(Clone, Debug)]
struct Symbol {
    name: String,
    range: Range<Point>,
    /// The outline item has a body: a definition rather than a declaration.
    has_body: bool,
}

#[derive(Default)]
struct IndexedFile {
    mtime: Option<fs::MTime>,
    symbols: Vec<Symbol>,
}

/// Symbols of one project, per worktree and file.
#[derive(Default)]
struct ProjectIndex {
    files: HashMap<(WorktreeId, Arc<RelPath>), IndexedFile>,
    /// A refresh is running; a lookup meanwhile uses the index as it is.
    refreshing: bool,
}

#[derive(Default)]
struct SearchNavigationIndexes(HashMap<EntityId, Arc<parking_lot::Mutex<ProjectIndex>>>);

impl Global for SearchNavigationIndexes {}

fn index_for(project: &Entity<Project>, cx: &mut App) -> Arc<parking_lot::Mutex<ProjectIndex>> {
    cx.default_global::<SearchNavigationIndexes>()
        .0
        .entry(project.entity_id())
        .or_default()
        .clone()
}

/// The word at `position` in `buffer`.
fn word_at(buffer: &Entity<Buffer>, position: language::Anchor, cx: &App) -> Option<String> {
    let snapshot = buffer.read(cx).snapshot();
    let offset = position.to_offset(&snapshot);
    let (range, _) = snapshot.surrounding_word(offset, None);
    let word: String = snapshot.text_for_range(range).collect();
    let is_identifier = !word.is_empty() && word.chars().all(|c| c.is_alphanumeric() || c == '_');
    is_identifier.then_some(word)
}

/// Bring the index up to date: parse files that are new or changed since
/// they were last indexed, and drop files that are gone.
async fn refresh_index(project: WeakEntity<Project>, index: Arc<parking_lot::Mutex<ProjectIndex>>, cx: &mut AsyncApp) -> Result<()> {
    {
        let mut index = index.lock();
        if index.refreshing {
            return Ok(());
        }
        index.refreshing = true;
    }
    let result = refresh_index_inner(project, index.clone(), cx).await;
    index.lock().refreshing = false;
    result
}

async fn refresh_index_inner(project: WeakEntity<Project>, index: Arc<parking_lot::Mutex<ProjectIndex>>, cx: &mut AsyncApp) -> Result<()> {
    let (files, fs, languages) = project.update(cx, |project, cx| {
        let mut files = Vec::new();
        for worktree in project.visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let (id, root) = (worktree.id(), worktree.abs_path());
            for entry in worktree.snapshot().files(false, 0) {
                if entry.size <= MAX_FILE_BYTES {
                    files.push((id, entry.path.clone(), root.join(entry.path.as_std_path()), entry.mtime));
                }
            }
        }
        (files, project.fs().clone(), project.languages().clone())
    })?;

    let stale: Vec<_> = {
        let mut index = index.lock();
        let present: collections::HashSet<(WorktreeId, Arc<RelPath>)> =
            files.iter().map(|(id, path, _, _)| (*id, path.clone())).collect();
        index.files.retain(|key, _| present.contains(key));
        files
            .into_iter()
            .filter(|(id, path, _, mtime)| {
                index
                    .files
                    .get(&(*id, path.clone()))
                    .is_none_or(|file| file.mtime != *mtime)
            })
            .collect()
    };

    let mut parsed = futures::stream::iter(stale.into_iter().map(|(id, path, abs_path, mtime)| {
        let (fs, languages) = (fs.clone(), languages.clone());
        let cx = cx.clone();
        async move {
            let symbols = async {
                let language = languages.load_language_for_file_path(path.as_std_path()).await.ok()?;
                let text = fs.load(&abs_path).await.ok()?;
                let build = cx.update(|cx| Buffer::build_snapshot(Rope::from(text.as_str()), Some(language), Some(languages), None, cx));
                let snapshot = cx.background_spawn(build).await;
                let items = snapshot.outline_items_as_points_containing(0..snapshot.len(), false, None);
                Some(
                    items
                        .into_iter()
                        .flat_map(|item| {
                            let has_body = item.body_range.is_some();
                            let range = item.selection_range.clone();
                            item.name_ranges
                                .iter()
                                .filter_map(|name| item.text.get(name.clone()))
                                .filter(|name| name.chars().all(|c| c.is_alphanumeric() || c == '_'))
                                .map(|name| Symbol {
                                    name: name.to_string(),
                                    range: range.clone(),
                                    has_body,
                                })
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>(),
                )
            }
            .await
            .unwrap_or_default();
            ((id, path), IndexedFile { mtime, symbols })
        }
    }))
    .buffer_unordered(PARALLEL_FILES);
    while let Some((key, file)) = parsed.next().await {
        index.lock().files.insert(key, file);
    }
    Ok(())
}

/// Definitions of `name` in the project's index, definitions with a body
/// before declarations.
fn lookup(index: &ProjectIndex, name: &str) -> Vec<(WorktreeId, Arc<RelPath>, Range<Point>)> {
    let mut hits: Vec<_> = index
        .files
        .iter()
        .flat_map(|((worktree, path), file)| {
            file.symbols
                .iter()
                .filter(|s| s.name == name)
                .map(move |s| (s.has_body, worktree, path, s.range.clone()))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(b.2)).then_with(|| a.3.start.cmp(&b.3.start)));
    hits.into_iter().map(|(_, worktree, path, range)| (*worktree, path.clone(), range)).collect()
}

/// Definitions of the word at `position`, found by name in the project's
/// symbol index.
pub(crate) fn definitions_by_name(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    position: language::Anchor,
    cx: &mut App,
) -> Task<Result<Option<Vec<LocationLink>>>> {
    let Some(name) = word_at(buffer, position, cx) else {
        return Task::ready(Ok(None));
    };
    let index = index_for(project, cx);
    let weak_project = project.downgrade();
    cx.spawn(async move |cx| {
        refresh_index(weak_project.clone(), index.clone(), cx).await?;
        let hits = lookup(&index.lock(), &name);
        let mut links = Vec::new();
        for (worktree_id, path, range) in hits.into_iter().take(50) {
            let open = weak_project.update(cx, |project, cx| {
                project.open_buffer(ProjectPath { worktree_id, path }, cx)
            })?;
            let Ok(target) = open.await else {
                continue;
            };
            let range = target.read_with(cx, |buffer, _| {
                buffer.anchor_before(buffer.clip_point(range.start, text::Bias::Left))
                    ..buffer.anchor_after(buffer.clip_point(range.end, text::Bias::Left))
            });
            links.push(LocationLink {
                origin: None,
                target: Location { buffer: target, range },
            });
        }
        Ok((!links.is_empty()).then_some(links))
    })
}

/// Every whole-word, case-sensitive occurrence of the word at `position` in
/// the project.
pub(crate) fn references_by_name(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    position: language::Anchor,
    cx: &mut App,
) -> Task<Result<Option<Vec<Location>>>> {
    let Some(name) = word_at(buffer, position, cx) else {
        return Task::ready(Ok(None));
    };
    let query = match SearchQuery::text(
        name,
        true,
        true,
        false,
        PathMatcher::default(),
        PathMatcher::default(),
        false,
        None,
    ) {
        Ok(query) => query,
        Err(e) => return Task::ready(Err(e)),
    };
    let results = project.update(cx, |project, cx| project.search(query, cx));
    cx.spawn(async move |_| {
        let _search = results.task_handle;
        let mut locations = Vec::new();
        let rx = results.rx;
        while let Ok(result) = rx.recv().await {
            if let SearchResult::Buffer { buffer, ranges } = result {
                locations.extend(ranges.into_iter().map(|range| Location {
                    buffer: buffer.clone(),
                    range,
                }));
            }
        }
        Ok((!locations.is_empty()).then_some(locations))
    })
}

/// Whether search-based navigation is the fallback for missing language
/// server answers.
pub(crate) fn enabled(cx: &App) -> bool {
    use settings::Settings as _;
    crate::EditorSettings::get_global(cx).go_to_definition_fallback
        == crate::editor_settings::GoToDefinitionFallback::SymbolSearch
}

/// `lsp` unless it found nothing, else the search-based `fallback`.
pub(crate) fn or_by_name<T: 'static>(
    lsp: Option<Task<Result<Option<Vec<T>>>>>,
    fallback: impl FnOnce(&mut App) -> Task<Result<Option<Vec<T>>>> + 'static,
    cx: &mut App,
) -> Task<Result<Option<Vec<T>>>> {
    if !enabled(cx) {
        return lsp.unwrap_or_else(|| Task::ready(Ok(None)));
    }
    cx.spawn(async move |cx| {
        if let Some(lsp) = lsp
            && let Ok(Some(found)) = lsp.await
            && !found.is_empty()
        {
            return Ok(Some(found));
        }
        cx.update(fallback).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tests::init_test;
    use gpui::TestAppContext;
    use language::{Language, LanguageConfig, LanguageMatcher, ToPoint as _};
    use project::FakeFs;
    use serde_json::json;
    use util::path;

    fn rust_with_outline() -> Arc<Language> {
        Arc::new(
            Language::new(
                LanguageConfig {
                    name: "Rust".into(),
                    matcher: Arc::new(LanguageMatcher {
                        path_suffixes: vec!["rs".to_string()],
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Some(tree_sitter_rust::LANGUAGE.into()),
            )
            .with_outline_query(r#"(function_item "fn" @context name: (_) @name) @item"#)
            .unwrap(),
        )
    }

    #[gpui::test]
    async fn finds_definitions_and_references_by_name(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                "src": {
                    "lib.rs": "fn helper() {}\nfn other() {}\n",
                    "main.rs": "fn main() {\n    helper();\n    helper();\n}\n",
                }
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        project.read_with(cx, |project, _| project.languages().add(rust_with_outline()));
        let worktree_id = project.read_with(cx, |project, cx| project.worktrees(cx).next().unwrap().read(cx).id());
        let main = project
            .update(cx, |project, cx| {
                project.open_buffer((worktree_id, util::rel_path::rel_path("src/main.rs")), cx)
            })
            .await
            .unwrap();
        let position = main.read_with(cx, |buffer, _| buffer.anchor_before(Point::new(1, 6)));

        let definitions = cx
            .update(|cx| definitions_by_name(&project, &main, position, cx))
            .await
            .unwrap()
            .expect("a definition by name");
        assert_eq!(definitions.len(), 1);
        definitions[0].target.buffer.read_with(cx, |buffer, _| {
            assert!(buffer.file().unwrap().path().as_unix_str().ends_with("src/lib.rs"));
            assert_eq!(definitions[0].target.range.start.to_point(buffer).row, 0);
        });

        let references = cx
            .update(|cx| references_by_name(&project, &main, position, cx))
            .await
            .unwrap()
            .expect("references by name");
        assert_eq!(references.len(), 3, "the definition and both calls");
    }
}
