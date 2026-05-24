use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Extension,
    extract::{FromRef, FromRequestParts, OptionalFromRequestParts, OriginalUri, Query, State},
    http::{Method, StatusCode, header::ACCEPT, request::Parts},
    response::{IntoResponse, Redirect, Response},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use decomp_dev_core::{
    AppError,
    config::{Config, GitHubConfig},
    models::Project,
};
use decomp_dev_github::graphql::{
    CurrentUserResponse, RepositoryPermission, fetch_current_user, fetch_simple_current_user,
};
use octocrab::{Octocrab, models::Author};
use rand::{TryRngCore, rngs::OsRng};
use time::{Duration, UtcDateTime};
use tower_sessions::Session;
use url::form_urlencoded;

pub const GITHUB_OAUTH_STATE: &str = "github_oauth_state";
pub const CURRENT_USER: &str = "current_user";
pub const RETURN_TO: &str = "return_to";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredOAuth {
    pub access_token: String,
    pub token_type: String,
    pub expires_at: Option<UtcDateTime>,
    pub refresh_token: Option<String>,
    pub refresh_token_expires_at: Option<UtcDateTime>,
}

impl From<StoredOAuth> for octocrab::auth::OAuth {
    fn from(value: StoredOAuth) -> Self {
        octocrab::auth::OAuth {
            access_token: value.access_token.into(),
            token_type: value.token_type,
            scope: Vec::new(),
            expires_in: None,
            refresh_token: None,
            refresh_token_expires_in: None,
        }
    }
}

pub type Profile = Author;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CurrentUser {
    pub oauth: Option<StoredOAuth>,
    pub data: CurrentUserResponse,
    #[serde(skip, default)]
    pub super_admin: bool,
}

impl CurrentUser {
    pub fn client(&self, config: &GitHubConfig) -> Result<Octocrab> {
        if let Some(oauth) = &self.oauth {
            Octocrab::builder()
                .oauth(oauth.clone().into())
                .build()
                .context("Failed to create GitHub client")
        } else {
            Octocrab::builder()
                .personal_token(config.token.clone())
                .build()
                .context("Failed to create GitHub client")
        }
    }

    pub fn permissions_for_repo(&self, id: u64) -> RepositoryPermission {
        if self.super_admin {
            return RepositoryPermission::Admin;
        }
        self.data
            .repositories
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.permission.clone())
            .unwrap_or(RepositoryPermission::None)
    }

    pub fn can_manage_repo(&self, id: u64) -> bool {
        matches!(self.permissions_for_repo(id), RepositoryPermission::Admin)
    }

    pub fn can_manage_project(&self, project: &Project) -> bool {
        self.super_admin || (!project.permanently_disabled && self.can_manage_repo(project.id))
    }
}

pub fn generate_nonce() -> String {
    let mut bytes = [0u8; 16];
    OsRng.try_fill_bytes(&mut bytes).unwrap();
    URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(serde::Deserialize)]
pub struct OAuthQuery {
    pub code: String,
    pub state: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OAuthResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: Option<i64>,
    pub refresh_token: Option<String>,
    pub refresh_token_expires_in: Option<i64>,
}

impl From<OAuthResponse> for octocrab::auth::OAuth {
    fn from(value: OAuthResponse) -> Self {
        octocrab::auth::OAuth {
            access_token: value.access_token.into(),
            token_type: value.token_type,
            scope: Vec::new(),
            expires_in: None,
            refresh_token: None,
            refresh_token_expires_in: None,
        }
    }
}

impl From<OAuthResponse> for StoredOAuth {
    fn from(value: OAuthResponse) -> Self {
        StoredOAuth {
            access_token: value.access_token,
            token_type: value.token_type,
            expires_at: value.expires_in.map(|s| UtcDateTime::now() + Duration::seconds(s)),
            refresh_token: value.refresh_token,
            refresh_token_expires_at: value
                .refresh_token_expires_in
                .map(|s| UtcDateTime::now() + Duration::seconds(s)),
        }
    }
}

#[derive(serde::Serialize)]
struct FetchAccessToken<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    code: &'a str,
}

#[derive(serde::Serialize)]
struct RefreshAccessToken<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    grant_type: &'a str,
    refresh_token: &'a str,
}

pub async fn oauth(
    session: Session,
    Query(OAuthQuery { code, state: oauth_state }): Query<OAuthQuery>,
    State(config): State<Arc<Config>>,
) -> Result<Response, AppError> {
    let Some(existing_state) = session.remove::<String>(GITHUB_OAUTH_STATE).await? else {
        tracing::warn!("No state found in session");
        return Ok((StatusCode::BAD_REQUEST, "No state found").into_response());
    };
    if existing_state != oauth_state {
        tracing::warn!("State mismatch: expected {}, got {}", existing_state, oauth_state);
        return Ok((StatusCode::BAD_REQUEST, "State mismatch").into_response());
    }

    let current_user = fetch_access_token(&config.github, &code).await?;
    session.insert(CURRENT_USER, current_user).await?;

    if let Some(return_to) = session.remove::<String>(RETURN_TO).await? {
        Ok(Redirect::to(&return_to).into_response())
    } else {
        Ok(Redirect::to("/").into_response())
    }
}

fn oauth_client() -> Octocrab {
    Octocrab::builder()
        .base_uri("https://github.com")
        .expect("Failed to create base URI")
        .add_header(ACCEPT, "application/json".to_string())
        .build()
        .expect("Failed to create Octocrab client")
}

async fn fetch_access_token(config: &GitHubConfig, code: &str) -> Result<CurrentUser, AppError> {
    let Some(oauth_config) = &config.oauth else {
        tracing::warn!("No GitHub OAuth config found");
        return Err(AppError::Internal(anyhow!("No GitHub OAuth config")));
    };
    let base_client = oauth_client();
    let oauth: OAuthResponse = base_client
        .post(
            "/login/oauth/access_token",
            Some(&FetchAccessToken {
                client_id: &oauth_config.client_id,
                client_secret: &oauth_config.client_secret,
                code,
            }),
        )
        .await?;
    let oauth = StoredOAuth::from(oauth);
    let client = Octocrab::builder().oauth(oauth.clone().into()).build()?;
    let data = fetch_current_user(&client).await?;
    let super_admin = config.super_admin_ids.contains(&data.id);
    if super_admin {
        tracing::info!(
            "Logged in as @{} [super admin] ({} repos)",
            data.login,
            data.repositories.len()
        );
    } else {
        tracing::info!("Logged in as @{} ({} repos)", data.login, data.repositories.len());
    }
    Ok(CurrentUser { oauth: Some(oauth), data, super_admin })
}

async fn refresh_access_token(
    config: &GitHubConfig,
    refresh_token: &str,
    prev_auth: &CurrentUser,
) -> Result<CurrentUser, anyhow::Error> {
    let Some(oauth_config) = &config.oauth else {
        tracing::warn!("No GitHub OAuth config found");
        bail!("No GitHub OAuth config found");
    };
    let base_client = oauth_client();
    let oauth: OAuthResponse = base_client
        .post(
            "/login/oauth/access_token",
            Some(&RefreshAccessToken {
                client_id: &oauth_config.client_id,
                client_secret: &oauth_config.client_secret,
                grant_type: "refresh_token",
                refresh_token,
            }),
        )
        .await?;
    let oauth = StoredOAuth::from(oauth);
    let super_admin = config.super_admin_ids.contains(&prev_auth.data.id);
    tracing::info!("Refreshed token for @{}", prev_auth.data.login);
    Ok(CurrentUser { oauth: Some(oauth), data: prev_auth.data.clone(), super_admin })
}

impl<S> FromRequestParts<S> for CurrentUser
where
    Arc<Config>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match <CurrentUser as OptionalFromRequestParts<S>>::from_request_parts(parts, state).await {
            Ok(Some(user)) => Ok(user),
            Ok(None) => {
                let method =
                    Method::from_request_parts(parts, state).await.unwrap_or(Method::OPTIONS);
                if method != Method::GET {
                    return Err((StatusCode::UNAUTHORIZED, "Unauthorized").into_response());
                }
                let path_and_query =
                    <Extension<OriginalUri> as FromRequestParts<S>>::from_request_parts(
                        parts, state,
                    )
                    .await
                    .ok()
                    .and_then(|uri| uri.path_and_query().cloned())
                    .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Unauthorized").into_response())?;
                let mut redirect_uri = "/login?return_to=".to_string();
                redirect_uri
                    .extend(form_urlencoded::byte_serialize(path_and_query.as_str().as_bytes()));
                Err(Redirect::to(&redirect_uri).into_response())
            }
            Err(e) => Err(e.into_response()),
        }
    }
}

impl<S> OptionalFromRequestParts<S> for CurrentUser
where
    Arc<Config>: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        let session = Session::from_request_parts(parts, state).await?;
        let config = Arc::<Config>::from_ref(state);
        let mut user = match session.get::<CurrentUser>(CURRENT_USER).await {
            Ok(Some(user)) => user,
            Ok(None) => return Ok(None),
            Err(e) => {
                tracing::warn!("Failed to fetch user from session: {}", e);
                return Ok(None);
            }
        };
        // Refresh user data if the ID is 0
        if user.data.id == 0 {
            match user.client(&config.github) {
                Ok(client) => match fetch_simple_current_user(&client).await {
                    Ok(data) => {
                        user.data =
                            CurrentUserResponse { repositories: user.data.repositories, ..data };
                        if let Err(e) = session.insert(CURRENT_USER, user.clone()).await {
                            tracing::error!("Failed to insert user into session: {}", e);
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to fetch user data: {}", e);
                        if let Err(e) = session.remove_value(CURRENT_USER).await {
                            tracing::error!("Failed to remove user from session: {}", e);
                        }
                        return Ok(None);
                    }
                },
                Err(e) => {
                    tracing::error!("Failed to create GitHub client: {}", e);
                    if let Err(e) = session.remove_value(CURRENT_USER).await {
                        tracing::error!("Failed to remove user from session: {}", e);
                    }
                    return Ok(None);
                }
            }
        }
        user.super_admin = (config.server.dev_mode && user.data.id == u64::MAX)
            || config.github.super_admin_ids.contains(&user.data.id);
        if let Some(oauth) = &user.oauth
            && let Some(expires_at) = oauth.expires_at
            && (UtcDateTime::now() + Duration::seconds(30)) > expires_at
        {
            // Access token expired, attempt to refresh
            if let Err(e) = session.remove_value(CURRENT_USER).await {
                tracing::error!("Failed to remove user from session: {}", e);
            };
            if let Some(refresh_token) = &oauth.refresh_token {
                if let Some(refresh_token_expires_at) = oauth.refresh_token_expires_at
                    && UtcDateTime::now() >= refresh_token_expires_at
                {
                    // Refresh token expired
                    return Ok(None);
                }
                let current_user =
                    match refresh_access_token(&config.github, refresh_token, &user).await {
                        Ok(current_user) => current_user,
                        Err(e) => {
                            tracing::error!("Failed to refresh access token: {:?}", e);
                            return Ok(None);
                        }
                    };
                if let Err(e) = session.insert(CURRENT_USER, current_user.clone()).await {
                    tracing::error!("Failed to insert user into session: {}", e);
                }
                return Ok(Some(current_user));
            }
            return Ok(None);
        }
        Ok(Some(user))
    }
}
