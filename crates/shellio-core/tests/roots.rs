//! Файловое разрешение корней на временных каталогах и настоящем `git`, подготовка
//! `project.add` через `CoreHandle`.
//!
//! `git` запускается только локально во временном каталоге, его конфигурация отрезана
//! переменными самой команды. Сравнение идёт с каноническими путями: временный каталог
//! macOS лежит за символической ссылкой `/var` → `/private/var`.

use std::error::Error;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::Arc;

use shellio_core::test_support::{
    Faults, FaultyPersist, ManualClock, MemoryPersist, SequentialIds, registry,
};
use shellio_core::{
    Command, CommandValue, Core, CoreError, CoreHandle, ErrorCode, FsRootResolver, ProjectId,
    QUEUE_CAPACITY, ReplyValue, ResolvedRoot, Revision, RootError, RootKind, RootResolver,
    UnixMillis, WorkspaceId,
};
use tempfile::TempDir;
use tokio::runtime::Handle;

type TestResult<T> = Result<T, Box<dyn Error>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);

/// Переменные, которыми окружение вызывающего (например, хук git) перенаправило бы `git`.
const GIT_REDIRECTS: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_CEILING_DIRECTORIES",
];

fn resolve(path: &Path) -> Result<ResolvedRoot, RootError> {
    FsRootResolver.resolve(path)
}

/// Временный каталог и его канонический путь.
fn temp() -> TestResult<(TempDir, PathBuf)> {
    let dir = TempDir::new()?;
    let canonical = fs::canonicalize(dir.path())?;
    Ok((dir, canonical))
}

fn mkdir(path: &Path) -> TestResult<PathBuf> {
    fs::create_dir_all(path)?;
    Ok(path.to_path_buf())
}

/// `git <args>` в каталоге `dir` без глобальной и системной конфигурации.
fn git(dir: &Path, args: &[&str]) -> TestResult<()> {
    let mut process = Process::new("git");
    process
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=shellio",
            "-c",
            "user.email=shellio@example.invalid",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for name in GIT_REDIRECTS {
        process.env_remove(name);
    }
    let output = process
        .output()
        .map_err(|error| format!("git не запустился (нужен git в PATH): {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} в {}: {}\n{}",
            dir.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

/// Репозиторий с одним коммитом, без которого нельзя завести worktree.
fn init_repo(base: &Path, name: &str) -> TestResult<PathBuf> {
    let repo = base.join(name);
    git(base, &["init", "-q", name])?;
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"])?;
    Ok(repo)
}

/// Связанная worktree репозитория `repo` в `path`.
fn add_worktree(repo: &Path, path: &Path) -> TestResult<PathBuf> {
    let target = path.to_str().ok_or("путь worktree не в UTF-8")?.to_owned();
    git(repo, &["worktree", "add", "-q", "--detach", &target])?;
    Ok(path.to_path_buf())
}

fn start(persist: FaultyPersist) -> TestResult<CoreHandle> {
    Ok(Core::start(
        &Handle::current(),
        Box::new(persist),
        Box::new(ManualClock::new(NOW)),
        Box::new(SequentialIds::new()),
        Arc::new(FsRootResolver),
        registry(),
    )?)
}

/// Ядро на файловом разрешении корней и пульт отказов его хранилища.
fn start_on(memory: &MemoryPersist) -> TestResult<(CoreHandle, Faults)> {
    let (persist, faults) = FaultyPersist::new(memory.clone());
    Ok((start(persist)?, faults))
}

async fn active(core: &CoreHandle) -> TestResult<WorkspaceId> {
    core.subscribe()
        .await?
        .0
        .state
        .window
        .active_workspace
        .ok_or_else(|| "нет активного воркспейса".into())
}

fn add(workspace: WorkspaceId, path: &Path) -> Command {
    Command::ProjectAdd {
        workspace,
        path: path.to_path_buf(),
        title: None,
    }
}

async fn add_project(
    core: &CoreHandle,
    workspace: WorkspaceId,
    path: &Path,
) -> TestResult<ProjectId> {
    let reply = core.request(add(workspace, path)).await?;
    match reply.value {
        ReplyValue::Command(CommandValue::Project { project, .. }) => Ok(project),
        other => Err(format!("ожидался ответ project.add: {other:?}").into()),
    }
}

fn assert_root_taken(error: &CoreError, owner: ProjectId, workspace: WorkspaceId) {
    assert_eq!(error.code(), ErrorCode::RootTaken, "{error}");
    assert_eq!(
        error.detail_str("project"),
        Some(owner.to_string().as_str())
    );
    assert_eq!(
        error.detail_str("workspace"),
        Some(workspace.to_string().as_str())
    );
}

fn key(root: &ResolvedRoot) -> &str {
    root.key.as_str()
}

#[test]
fn relative_path_is_not_absolute() {
    for path in ["relative/dir", ".", ""] {
        let error = resolve(Path::new(path)).unwrap_err();
        assert_eq!(error.reason(), "path_not_absolute", "{path:?}");
        assert_eq!(error.path(), Path::new(path));
    }
}

#[test]
fn missing_path_is_not_found() {
    let (_dir, base) = temp().unwrap();
    fs::write(base.join("file.txt"), "x").unwrap();
    symlink(base.join("nowhere"), base.join("dangling")).unwrap();

    for path in [
        base.join("missing"),
        base.join("missing/inner"),
        base.join("file.txt/inner"),
        base.join("dangling"),
    ] {
        let error = resolve(&path).unwrap_err();
        assert_eq!(error.reason(), "path_not_found", "{}", path.display());
        assert_eq!(error.path(), path);
    }
}

#[test]
fn file_is_not_a_directory() {
    let (_dir, base) = temp().unwrap();
    let file = base.join("file.txt");
    fs::write(&file, "x").unwrap();
    symlink(&file, base.join("link-to-file")).unwrap();

    for path in [file, base.join("link-to-file")] {
        let error = resolve(&path).unwrap_err();
        assert_eq!(error.reason(), "not_a_directory", "{}", path.display());
        assert_eq!(error.path(), path);
    }
}

#[test]
fn folder_resolves_to_its_canonical_path() {
    let (dir, base) = temp().unwrap();
    let notes = mkdir(&base.join("notes")).unwrap();

    let root = resolve(&dir.path().join("notes")).unwrap();
    assert_eq!(root.kind, RootKind::Folder);
    assert_eq!(root.path, notes);
    assert_eq!(key(&root), notes.to_str().unwrap());

    let detour = resolve(&base.join("notes/../notes/.")).unwrap();
    assert_eq!(detour, root);
}

#[test]
fn symlink_to_folder_gives_same_root() {
    let (_dir, base) = temp().unwrap();
    let notes = mkdir(&base.join("notes")).unwrap();
    symlink(&notes, base.join("link")).unwrap();
    symlink(base.join("link"), base.join("link-to-link")).unwrap();

    let direct = resolve(&notes).unwrap();
    assert_eq!(resolve(&base.join("link")).unwrap(), direct);
    assert_eq!(resolve(&base.join("link-to-link")).unwrap(), direct);
}

#[test]
fn repository_subdirectory_is_folder() {
    let (_dir, base) = temp().unwrap();
    let repo = mkdir(&base.join("repo")).unwrap();
    mkdir(&repo.join(".git")).unwrap();
    let src = mkdir(&repo.join("src")).unwrap();

    let root = resolve(&repo).unwrap();
    assert_eq!(root.kind, RootKind::Git);
    assert_eq!(key(&root), repo.join(".git").to_str().unwrap());

    let sub = resolve(&src).unwrap();
    assert_eq!(sub.kind, RootKind::Folder);
    assert_eq!(sub.path, src);
    assert_eq!(key(&sub), src.to_str().unwrap());
}

#[test]
fn git_file_follows_relative_gitdir_and_commondir() {
    let (_dir, base) = temp().unwrap();
    let common = mkdir(&base.join("main/.git")).unwrap();
    let linked = mkdir(&common.join("worktrees/wt")).unwrap();
    fs::write(linked.join("commondir"), "../..\n").unwrap();
    let wt = mkdir(&base.join("wt")).unwrap();
    fs::write(wt.join(".git"), "gitdir: ../main/.git/worktrees/wt\n").unwrap();

    let root = resolve(&wt).unwrap();
    assert_eq!(root.kind, RootKind::Git);
    assert_eq!(root.path, wt);
    assert_eq!(key(&root), common.to_str().unwrap());

    // Без `commondir` ключ — сам каталог из `gitdir`.
    fs::remove_file(linked.join("commondir")).unwrap();
    assert_eq!(key(&resolve(&wt).unwrap()), linked.to_str().unwrap());
}

#[test]
fn gitdir_keeps_trailing_spaces_of_its_target() {
    let (_dir, base) = temp().unwrap();
    mkdir(&base.join("repo.git")).unwrap();
    let spaced = mkdir(&base.join("repo.git ")).unwrap();
    let wt = mkdir(&base.join("wt")).unwrap();
    fs::write(wt.join(".git"), "gitdir: ../repo.git \r\n").unwrap();

    let root = resolve(&wt).unwrap();
    assert_eq!(root.kind, RootKind::Git);
    assert_eq!(key(&root), spaced.to_str().unwrap());
}

#[test]
fn broken_git_link_is_git_link_invalid() {
    let (_dir, base) = temp().unwrap();
    let store = mkdir(&base.join("store")).unwrap();
    fs::write(base.join("plain.txt"), "x").unwrap();
    let cases = [
        ("garbage", "not a link\n", None),
        ("no-space", "gitdir:../store\n", None),
        ("empty-target", "gitdir: \n", None),
        ("missing-target", "gitdir: ../nowhere\n", None),
        ("file-target", "gitdir: ../plain.txt\n", None),
        ("bad-commondir", "gitdir: ../store\n", Some("../nowhere\n")),
        ("empty-commondir", "gitdir: ../store\n", Some("\n")),
    ];
    for (name, link, commondir) in cases {
        let wt = mkdir(&base.join(name)).unwrap();
        fs::write(wt.join(".git"), link).unwrap();
        let commondir_file = store.join("commondir");
        match commondir {
            Some(text) => fs::write(&commondir_file, text).unwrap(),
            None => {
                let _ = fs::remove_file(&commondir_file);
            }
        }
        let error = resolve(&wt).unwrap_err();
        assert_eq!(error.reason(), "git_link_invalid", "{name}");
        let broken = if commondir.is_some() {
            commondir_file
        } else {
            wt.join(".git")
        };
        assert_eq!(error.path(), broken, "{name}");
    }
}

#[test]
fn link_file_not_in_utf8_or_not_a_file_is_refused() {
    let (_dir, base) = temp().unwrap();
    let store = mkdir(&base.join("store")).unwrap();

    let wt = mkdir(&base.join("bytes")).unwrap();
    fs::write(wt.join(".git"), b"gitdir: ../st\xffore\n").unwrap();
    let error = resolve(&wt).unwrap_err();
    assert_eq!(error.reason(), "path_not_utf8");
    assert_eq!(error.path(), wt.join(".git"));

    // `commondir` — каталог, а не файл ссылки.
    mkdir(&store.join("commondir")).unwrap();
    let wt = mkdir(&base.join("dir-commondir")).unwrap();
    fs::write(wt.join(".git"), "gitdir: ../store\n").unwrap();
    let error = resolve(&wt).unwrap_err();
    assert_eq!(error.reason(), "git_link_invalid");
    assert_eq!(error.path(), store.join("commondir"));
}

#[test]
fn unreadable_directory_is_path_unreadable() {
    let (_dir, base) = temp().unwrap();
    let locked = mkdir(&base.join("locked")).unwrap();
    mkdir(&locked.join("inner")).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let own = resolve(&locked);
    let inner = resolve(&locked.join("inner"));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();

    let own = own.unwrap_err();
    assert_eq!(own.reason(), "path_unreadable");
    assert_eq!(own.path(), locked.join(".git"));
    let inner = inner.unwrap_err();
    assert_eq!(inner.reason(), "path_unreadable");
    assert_eq!(inner.path(), locked.join("inner"));
}

#[test]
fn real_repository_is_keyed_by_its_git_dir() {
    let (_dir, base) = temp().unwrap();
    let repo = init_repo(&base, "repo").unwrap();

    let root = resolve(&repo).unwrap();
    assert_eq!(root.kind, RootKind::Git);
    assert_eq!(root.path, repo);
    assert_eq!(key(&root), repo.join(".git").to_str().unwrap());
}

#[test]
fn linked_worktree_is_keyed_by_main_repository() {
    let (_dir, base) = temp().unwrap();
    let repo = init_repo(&base, "repo").unwrap();
    let wt = add_worktree(&repo, &base.join("wt")).unwrap();
    assert!(wt.join(".git").is_file(), "у worktree .git — файл");

    let main = resolve(&repo).unwrap();
    let linked = resolve(&wt).unwrap();
    assert_eq!(linked.kind, RootKind::Git);
    assert_eq!(linked.path, wt);
    assert_eq!(linked.key, main.key);
}

#[test]
fn git_file_without_commondir_is_keyed_by_its_gitdir() {
    let (_dir, base) = temp().unwrap();
    git(
        &base,
        &["init", "-q", "--separate-git-dir", "store.git", "work"],
    )
    .unwrap();
    let work = base.join("work");
    let store = base.join("store.git");
    assert!(
        work.join(".git").is_file(),
        "у рабочего каталога .git — файл"
    );
    assert!(!store.join("commondir").exists(), "commondir не ожидался");

    let root = resolve(&work).unwrap();
    assert_eq!(root.kind, RootKind::Git);
    assert_eq!(root.path, work);
    assert_eq!(key(&root), store.to_str().unwrap());
}

#[tokio::test]
async fn second_add_of_worktree_or_main_repository_is_root_taken() {
    let (_dir, base) = temp().unwrap();
    let repo = init_repo(&base, "repo").unwrap();
    let wt = add_worktree(&repo, &base.join("wt")).unwrap();
    let memory = MemoryPersist::new();
    let (core, _) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();

    let owner = add_project(&core, workspace, &wt).await.unwrap();
    let state = memory.state();
    let root = state.roots.values().next().unwrap();
    assert_eq!(root.path, wt);
    assert_eq!(root.kind, RootKind::Git);
    assert_eq!(state.projects[&owner].title.as_str(), "wt");

    let other = match core
        .request(Command::WorkspaceCreate {
            title: "Другой".into(),
        })
        .await
        .unwrap()
        .value
    {
        ReplyValue::Command(CommandValue::Workspace { workspace }) => workspace,
        other => panic!("ожидался ответ workspace.create: {other:?}"),
    };
    let revision = memory.revision();
    for (target, path) in [(workspace, &wt), (workspace, &repo), (other, &repo)] {
        let error = core.request(add(target, path)).await.unwrap_err();
        assert_root_taken(&error, owner, workspace);
    }
    assert_eq!(memory.revision(), revision);
    assert_eq!(memory.state().projects.len(), 1);
}

#[tokio::test]
async fn resolution_errors_come_back_as_invalid_argument() {
    let (_dir, base) = temp().unwrap();
    fs::write(base.join("file.txt"), "x").unwrap();
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();

    for (path, reason) in [
        (PathBuf::from("relative"), "path_not_absolute"),
        (base.join("missing"), "path_not_found"),
        (base.join("file.txt"), "not_a_directory"),
    ] {
        let direct = core.request(add(workspace, &path)).await.unwrap_err();
        let receipt = core.try_request(add(workspace, &path)).unwrap();
        let through_receipt = receipt.await.unwrap().unwrap_err();
        for error in [direct, through_receipt] {
            assert_eq!(error.code(), ErrorCode::InvalidArgument);
            assert_eq!(error.detail_str("field"), Some("path"));
            assert_eq!(error.detail_str("reason"), Some(reason));
            assert_eq!(error.detail_str("path"), path.to_str());
        }
    }
    assert_eq!(
        faults.commits(),
        1,
        "отказ разрешения не доходит до хранилища"
    );
    assert!(memory.state().projects.is_empty());
}

#[tokio::test]
async fn try_request_project_add_on_full_queue_is_busy_through_receipt() {
    let (_dir, base) = temp().unwrap();
    let notes = mkdir(&base.join("notes")).unwrap();
    let memory = MemoryPersist::new();
    let (core, faults) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();

    let mut barrier = faults.barrier_next_commit();
    let mut receipts = vec![
        core.try_request(Command::WorkspaceCreate {
            title: "В записи".into(),
        })
        .unwrap(),
    ];
    barrier.entered().await;
    for index in 0..QUEUE_CAPACITY {
        receipts.push(
            core.try_request(Command::WorkspaceCreate {
                title: format!("В очереди {index}"),
            })
            .unwrap(),
        );
    }
    let receipt = core.try_request(add(workspace, &notes)).unwrap();
    let error = receipt.await.unwrap().unwrap_err();
    assert_eq!(error.code(), ErrorCode::Busy);
    assert!(error.retryable());

    barrier.release();
    for receipt in receipts {
        receipt.await.unwrap().unwrap();
    }
    assert!(memory.state().projects.is_empty());
    assert_eq!(memory.revision(), Revision(2 + QUEUE_CAPACITY as u64));

    let receipt = core.try_request(add(workspace, &notes)).unwrap();
    receipt.await.unwrap().unwrap();
    assert_eq!(memory.state().projects.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_adds_of_one_root_give_one_success_and_one_root_taken() {
    let (_dir, base) = temp().unwrap();
    let memory = MemoryPersist::new();
    let (core, _) = start_on(&memory).unwrap();
    let workspace = active(&core).await.unwrap();

    for round in 0..20 {
        let folder = mkdir(&base.join(format!("root-{round}"))).unwrap();
        let link = base.join(format!("link-{round}"));
        symlink(&folder, &link).unwrap();
        let tasks = [folder.clone(), link].map(|path| {
            let core = core.clone();
            tokio::spawn(async move { core.request(add(workspace, &path)).await })
        });
        let mut results = Vec::new();
        for task in tasks {
            results.push(task.await.unwrap());
        }

        let winners: Vec<ProjectId> = results
            .iter()
            .filter_map(|result| match result {
                Ok(reply) => match reply.value {
                    ReplyValue::Command(CommandValue::Project { project, .. }) => Some(project),
                    _ => None,
                },
                Err(_) => None,
            })
            .collect();
        assert_eq!(winners.len(), 1, "раунд {round}: {results:?}");
        let refused: Vec<&CoreError> = results.iter().filter_map(|r| r.as_ref().err()).collect();
        assert_eq!(refused.len(), 1, "раунд {round}: {results:?}");
        assert_root_taken(refused[0], winners[0], workspace);

        let state = memory.state();
        let roots: Vec<_> = state
            .roots
            .values()
            .filter(|root| root.path == folder)
            .collect();
        assert_eq!(roots.len(), 1, "раунд {round}");
        assert_eq!(roots[0].project, winners[0]);
    }
    let state = memory.state();
    assert_eq!(state.projects.len(), 20);
    assert_eq!(state.roots.len(), 20);
}
