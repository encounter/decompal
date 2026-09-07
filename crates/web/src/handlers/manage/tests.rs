use std::sync::Arc;

use decomp_dev_core::{config::Config, models::Commit};
use decomp_dev_db::Database;
use decomp_dev_github::{
    GitHub,
    graphql::{CurrentUserRepository, CurrentUserResponse},
};
use decomp_dev_jobs::JobStorage;
use objdiff_core::bindings::report::{Measures, REPORT_VERSION, Report};
use time::UtcDateTime;

use super::*;

fn category(id: &str) -> ReportCategory {
    ReportCategory {
        id: id.into(),
        name: format!("Category {id}"),
        measures: Some(Measures {
            total_code: 10000,
            matched_code: 7049,
            matched_code_percent: 70.49,
            ..Default::default()
        }),
    }
}

#[test]
fn category_selection() {
    let categories = vec![category("game"), category("game/engine")];
    assert_eq!(
        validate_category(Some("game/engine"), None, &categories)
            .map_err(|e| e.into_response().status())
            .unwrap()
            .as_deref(),
        Some("game/engine")
    );
    assert_eq!(
        validate_category(Some(""), Some("game"), &categories)
            .map_err(|e| e.into_response().status())
            .unwrap(),
        None
    );
    assert_eq!(
        validate_category(None, None, &[]).map_err(|e| e.into_response().status()).unwrap(),
        None
    );
    assert_eq!(
        validate_category(None, Some("game"), &categories)
            .map_err(|e| e.into_response().status())
            .unwrap()
            .as_deref(),
        Some("game")
    );
    assert!(matches!(
        validate_category(Some("unknown"), None, &categories),
        Err(AppError::Status(StatusCode::BAD_REQUEST))
    ));
    assert_eq!(
        validate_category(Some("game"), Some("game"), &[])
            .map_err(|e| e.into_response().status())
            .unwrap(),
        None
    );
    assert_eq!(
        category_options(&[], Some("gone")).into_string(),
        "<option value=\"\" selected>All</option>"
    );
    let markup = category_options(&categories, Some("game/engine")).into_string();
    assert!(markup.contains("value=\"game/engine\" selected"));
}

async fn fixture() -> (AppState, CurrentUser) {
    let config: Config = serde_yaml::from_str("server:\n  port: 3000\ndb:\n  url: 'sqlite::memory:'\n  jobs_url: 'sqlite::memory:'\ngithub:\n  token: test\n").unwrap();
    let db = Database::new(&config.db).await.unwrap();
    let project = Project {
        id: 1,
        owner: "owner".into(),
        repo: "game".into(),
        platform: Some("win32".into()),
        ..Default::default()
    };
    let commit = Commit { sha: "a".repeat(40), timestamp: UtcDateTime::now(), message: None };
    for (version, categories) in [
        ("v1", vec![category("game"), category("game/engine")]),
        ("v2", vec![category("other"), category("game/engine")]),
    ] {
        db.insert_report(
            &project,
            &commit,
            version,
            Box::new(Report {
                version: REPORT_VERSION,
                measures: Some(Measures {
                    total_code: 10000,
                    matched_code: 4931,
                    matched_code_percent: 49.31,
                    ..Default::default()
                }),
                categories,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    }
    let jobs = JobStorage::setup(&config.db).await.unwrap();
    let state = AppState {
        config: Arc::new(config),
        db,
        jobs,
        github: Arc::new(GitHub {
            client: octocrab::Octocrab::builder().build().unwrap(),
            installations: None,
        }),
    };
    let user = CurrentUser {
        oauth: None,
        super_admin: false,
        data: CurrentUserResponse {
            id: 1,
            login: "owner".into(),
            url: "https://github.com/owner".into(),
            repositories: vec![CurrentUserRepository {
                id: 1,
                owner: "owner".into(),
                name: "game".into(),
                permission: RepositoryPermission::Admin,
            }],
        },
    };
    (state, user)
}

async fn save(
    state: &AppState,
    user: &CurrentUser,
    version: &str,
    category: &str,
) -> Result<Response, AppError> {
    manage_project_save(
        Path(ProjectParams { owner: "owner".into(), repo: "game".into() }),
        State(state.clone()),
        user.clone(),
        TypedMultipart(ProjectForm {
            name: "Game".into(),
            short_name: "".into(),
            platform: "win32".into(),
            default_version: Some(version.into()),
            default_category: Some(category.into()),
            workflow_id: "build.yml".into(),
            enable_pr_comments: None,
            pr_report_style: None,
            header_image: None,
            clear_header_image: None,
            enabled: Some("on".into()),
            permanently_disabled: None,
        }),
    )
    .await
}

#[tokio::test]
async fn saves_validates_and_clears_default_category() {
    let (state, user) = fixture().await;
    assert_eq!(
        save(&state, &user, "v1", "game")
            .await
            .map_err(|e| e.into_response().status())
            .unwrap()
            .status(),
        StatusCode::SEE_OTHER
    );
    let project = state.db.get_project_by_id(1).await.unwrap().unwrap();
    assert_eq!(project.default_category.as_deref(), Some("game"));
    assert_eq!(project.default_version.as_deref(), Some("v1"));
    for (version, category) in [("v1", "unknown"), ("v1", "other"), ("missing", "game")] {
        assert!(matches!(
            save(&state, &user, version, category).await,
            Err(AppError::Status(StatusCode::BAD_REQUEST))
        ));
        assert_eq!(
            state.db.get_project_by_id(1).await.unwrap().unwrap().default_category.as_deref(),
            Some("game")
        );
    }
    // Switching versions without JS also clears an unavailable saved category.
    save(&state, &user, "v2", "game").await.map_err(|e| e.into_response().status()).unwrap();
    assert_eq!(state.db.get_project_by_id(1).await.unwrap().unwrap().default_category, None);
    save(&state, &user, "v2", "other").await.map_err(|e| e.into_response().status()).unwrap();
    save(&state, &user, "v2", "").await.map_err(|e| e.into_response().status()).unwrap();
    assert_eq!(state.db.get_project_by_id(1).await.unwrap().unwrap().default_category, None);

    // A newer report can remove a category while the settings form is open.
    save(&state, &user, "v1", "game").await.map_err(|e| e.into_response().status()).unwrap();
    let project = state.db.get_project_by_id(1).await.unwrap().unwrap();
    let commit = Commit {
        sha: "b".repeat(40),
        timestamp: UtcDateTime::now() + time::Duration::seconds(1),
        message: None,
    };
    state
        .db
        .insert_report(
            &project,
            &commit,
            "v1",
            Box::new(Report {
                version: REPORT_VERSION,
                measures: Some(Measures::default()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    save(&state, &user, "v1", "game").await.map_err(|e| e.into_response().status()).unwrap();
    assert_eq!(state.db.get_project_by_id(1).await.unwrap().unwrap().default_category, None);
}

#[tokio::test]
async fn category_save_requires_project_admin() {
    let (state, mut user) = fixture().await;
    user.data.repositories[0].permission = RepositoryPermission::Write;
    assert!(matches!(
        save(&state, &user, "v1", "game").await,
        Err(AppError::Status(StatusCode::FORBIDDEN))
    ));
    user.data.repositories[0].permission = RepositoryPermission::Admin;
    let mut project = state.db.get_project_by_id(1).await.unwrap().unwrap();
    project.permanently_disabled = true;
    state.db.update_project(&project).await.unwrap();
    assert!(matches!(
        save(&state, &user, "v1", "game").await,
        Err(AppError::Status(StatusCode::FORBIDDEN))
    ));
    assert_eq!(state.db.get_project_by_id(1).await.unwrap().unwrap().default_category, None);
}

#[tokio::test]
async fn saves_project_without_reports() {
    let (state, user) = fixture().await;
    state.db.delete_reports_by_commit(1, &"a".repeat(40)).await.unwrap();
    save(&state, &user, "", "").await.map_err(|e| e.into_response().status()).unwrap();
    let project = state.db.get_project_by_id(1).await.unwrap().unwrap();
    assert_eq!(project.default_version, None);
    assert_eq!(project.default_category, None);
    assert!(matches!(
        save(&state, &user, "", "game").await,
        Err(AppError::Status(StatusCode::BAD_REQUEST))
    ));
}
