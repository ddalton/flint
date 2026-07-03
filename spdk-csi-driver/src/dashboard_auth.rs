// dashboard_auth.rs - Bearer-token authentication for the dashboard backend.
//
// Sessions live in memory: the dashboard deployment is single-replica, so a
// backend restart simply invalidates all tokens and the SPA re-logs-in.
// Passwords arrive via environment variables wired from a Secret by the Helm
// chart; if no admin password is configured a random one is generated and
// printed to the log so a fresh install is never silently open.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rand::distributions::Alphanumeric;
use rand::{Rng, RngCore};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use warp::http::StatusCode;
use warp::{Filter, Rejection, Reply};

/// Ordering matters: authorization checks are `role >= minimum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Admin,
}

struct Session {
    role: Role,
    expires_at: Instant,
}

pub struct AuthState {
    admin_password: String,
    viewer_password: Option<String>,
    ttl: Duration,
    sessions: RwLock<HashMap<String, Session>>,
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
    role: Role,
    expires_in_secs: u64,
}

#[derive(Debug)]
pub enum AuthReject {
    Unauthorized,
    Forbidden,
}

impl warp::reject::Reject for AuthReject {}

impl AuthState {
    pub fn from_env() -> Arc<Self> {
        let admin_password = match std::env::var("DASHBOARD_ADMIN_PASSWORD") {
            Ok(p) if !p.is_empty() => p,
            _ => {
                let generated: String = rand::thread_rng()
                    .sample_iter(&Alphanumeric)
                    .take(24)
                    .map(char::from)
                    .collect();
                println!(
                    "⚠️ [DASHBOARD_AUTH] DASHBOARD_ADMIN_PASSWORD is not set — generated a one-time admin password: {}",
                    generated
                );
                println!(
                    "   Set the spdk-dashboard-auth Secret (chart value dashboard.auth.adminPassword) for a stable credential."
                );
                generated
            }
        };
        let viewer_password = std::env::var("DASHBOARD_VIEWER_PASSWORD")
            .ok()
            .filter(|p| !p.is_empty());
        let ttl = std::env::var("DASHBOARD_SESSION_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(12 * 3600));
        Arc::new(Self {
            admin_password,
            viewer_password,
            ttl,
            sessions: RwLock::new(HashMap::new()),
        })
    }

    #[cfg(test)]
    pub fn for_tests(admin: &str, viewer: Option<&str>, ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            admin_password: admin.to_string(),
            viewer_password: viewer.map(str::to_string),
            ttl,
            sessions: RwLock::new(HashMap::new()),
        })
    }

    /// Validate credentials and mint a session token.
    pub async fn login(&self, username: &str, password: &str) -> Option<(String, Role, u64)> {
        let role = match username {
            "admin" if constant_time_eq(password, &self.admin_password) => Role::Admin,
            "viewer" => match &self.viewer_password {
                Some(vp) if constant_time_eq(password, vp) => Role::Viewer,
                _ => return None,
            },
            _ => return None,
        };
        let token = generate_token();
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, s| s.expires_at > now);
        sessions.insert(
            token.clone(),
            Session {
                role,
                expires_at: now + self.ttl,
            },
        );
        Some((token, role, self.ttl.as_secs()))
    }

    pub async fn validate(&self, token: &str, minimum: Role) -> Result<Role, AuthReject> {
        let sessions = self.sessions.read().await;
        match sessions.get(token) {
            Some(s) if s.expires_at > Instant::now() => {
                if s.role >= minimum {
                    Ok(s.role)
                } else {
                    Err(AuthReject::Forbidden)
                }
            }
            _ => Err(AuthReject::Unauthorized),
        }
    }
}

/// Filter that rejects the request unless it carries a bearer token whose
/// session meets `minimum`. Extracts nothing, so inserting it into a route
/// chain leaves handler signatures untouched.
pub fn require(
    auth: Arc<AuthState>,
    minimum: Role,
) -> impl Filter<Extract = (), Error = Rejection> + Clone {
    warp::header::optional::<String>("authorization")
        .and(warp::any().map(move || auth.clone()))
        .and_then(move |header: Option<String>, auth: Arc<AuthState>| async move {
            let token = header
                .as_deref()
                .and_then(|h| h.strip_prefix("Bearer ").or_else(|| h.strip_prefix("bearer ")))
                .unwrap_or("");
            if token.is_empty() {
                return Err(warp::reject::custom(AuthReject::Unauthorized));
            }
            auth.validate(token, minimum)
                .await
                .map(|_| ())
                .map_err(warp::reject::custom)
        })
        .untuple_one()
}

/// POST /api/login — exchanges credentials for a bearer token.
pub fn login_route(
    auth: Arc<AuthState>,
) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    warp::path!("api" / "login")
        .and(warp::post())
        .and(warp::body::content_length_limit(4 * 1024))
        .and(warp::body::json())
        .and(warp::any().map(move || auth.clone()))
        .and_then(|req: LoginRequest, auth: Arc<AuthState>| async move {
            match auth.login(&req.username, &req.password).await {
                Some((token, role, expires_in_secs)) => Ok::<_, Rejection>(warp::reply::with_status(
                    warp::reply::json(&LoginResponse {
                        token,
                        role,
                        expires_in_secs,
                    }),
                    StatusCode::OK,
                )),
                None => Ok(warp::reply::with_status(
                    warp::reply::json(&serde_json::json!({"error": "invalid credentials"})),
                    StatusCode::UNAUTHORIZED,
                )),
            }
        })
}

/// Converts auth rejections into 401/403 JSON; everything else passes through.
pub async fn handle_rejection(err: Rejection) -> Result<impl Reply, Rejection> {
    if let Some(auth_err) = err.find::<AuthReject>() {
        let (code, msg) = match auth_err {
            AuthReject::Unauthorized => (StatusCode::UNAUTHORIZED, "authentication required"),
            AuthReject::Forbidden => (StatusCode::FORBIDDEN, "admin role required"),
        };
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({"error": msg})),
            code,
        ));
    }
    Err(err)
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protected(
        auth: Arc<AuthState>,
        minimum: Role,
    ) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
        warp::path!("api" / "guarded")
            .and(warp::get())
            .and(require(auth, minimum))
            .map(|| "ok")
    }

    #[tokio::test]
    async fn login_rejects_bad_credentials_and_unknown_users() {
        let auth = AuthState::for_tests("s3cret", None, Duration::from_secs(60));
        assert!(auth.login("admin", "wrong").await.is_none());
        assert!(auth.login("root", "s3cret").await.is_none());
        // Viewer role is disabled entirely when no viewer password is set.
        assert!(auth.login("viewer", "s3cret").await.is_none());
        assert!(auth.login("admin", "s3cret").await.is_some());
    }

    #[tokio::test]
    async fn tokens_expire_and_roles_gate() {
        let auth = AuthState::for_tests("a", Some("v"), Duration::from_secs(60));
        let (admin_tok, role, _) = auth.login("admin", "a").await.unwrap();
        assert_eq!(role, Role::Admin);
        let (viewer_tok, role, _) = auth.login("viewer", "v").await.unwrap();
        assert_eq!(role, Role::Viewer);

        assert!(auth.validate(&admin_tok, Role::Admin).await.is_ok());
        assert!(auth.validate(&viewer_tok, Role::Viewer).await.is_ok());
        assert!(matches!(
            auth.validate(&viewer_tok, Role::Admin).await,
            Err(AuthReject::Forbidden)
        ));
        assert!(matches!(
            auth.validate("bogus", Role::Viewer).await,
            Err(AuthReject::Unauthorized)
        ));

        let expiring = AuthState::for_tests("a", None, Duration::ZERO);
        let (tok, _, _) = expiring.login("admin", "a").await.unwrap();
        assert!(matches!(
            expiring.validate(&tok, Role::Viewer).await,
            Err(AuthReject::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn unauthenticated_request_gets_401_json() {
        let auth = AuthState::for_tests("a", None, Duration::from_secs(60));
        let routes = protected(auth, Role::Viewer).recover(handle_rejection);
        let res = warp::test::request()
            .method("GET")
            .path("/api/guarded")
            .reply(&routes)
            .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(std::str::from_utf8(res.body()).unwrap().contains("authentication required"));
    }

    #[tokio::test]
    async fn viewer_token_gets_403_on_admin_route() {
        let auth = AuthState::for_tests("a", Some("v"), Duration::from_secs(60));
        let (viewer_tok, _, _) = auth.login("viewer", "v").await.unwrap();
        let routes = protected(auth, Role::Admin).recover(handle_rejection);
        let res = warp::test::request()
            .method("GET")
            .path("/api/guarded")
            .header("authorization", format!("Bearer {}", viewer_tok))
            .reply(&routes)
            .await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admin_token_passes_and_login_endpoint_round_trips() {
        let auth = AuthState::for_tests("s3cret", None, Duration::from_secs(60));
        let routes = login_route(auth.clone())
            .or(protected(auth, Role::Admin))
            .recover(handle_rejection);

        let res = warp::test::request()
            .method("POST")
            .path("/api/login")
            .json(&serde_json::json!({"username": "admin", "password": "s3cret"}))
            .reply(&routes)
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        assert_eq!(body["role"], "admin");
        let token = body["token"].as_str().unwrap();
        assert_eq!(token.len(), 64); // 32 random bytes, hex-encoded

        let res = warp::test::request()
            .method("GET")
            .path("/api/guarded")
            .header("authorization", format!("Bearer {}", token))
            .reply(&routes)
            .await;
        assert_eq!(res.status(), StatusCode::OK);

        let res = warp::test::request()
            .method("POST")
            .path("/api/login")
            .json(&serde_json::json!({"username": "admin", "password": "nope"}))
            .reply(&routes)
            .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
