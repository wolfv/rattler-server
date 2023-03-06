mod available_packages_cache;
mod dto;
mod error;
mod fetch;
mod generic_cache;

use crate::dto::{SolveEnvironment, SolveEnvironmentErr, SolveEnvironmentOk};
use crate::error::{ApiError, ParseError, ParseErrors, ValidationError};
use anyhow::Context;
use available_packages_cache::AvailablePackagesCache;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{routing::post, Json, Router};
use rattler_conda_types::{
    Channel, ChannelConfig, GenericVirtualPackage, MatchSpec, Platform, RepoDataRecord,
};
use rattler_solve::{LibsolvBackend, SolverBackend, SolverProblem};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{event, span, Instrument, Level};
use tracing_subscriber::fmt::format;
use tracing_subscriber::fmt::format::FmtSpan;

struct AppState {
    available_packages: AvailablePackagesCache,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .event_format(format().pretty())
        .with_span_events(FmtSpan::CLOSE)
        .with_env_filter("rattler_server=trace")
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let state = AppState {
        available_packages: AvailablePackagesCache::new(),
    };

    let app = Router::new()
        .route("/solve", post(solve_environment))
        .with_state(Arc::new(state));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

#[tracing::instrument(level = "info", skip(state))]
async fn solve_environment(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SolveEnvironment>,
) -> Response {
    let result = solve_environment_inner(state, payload).await;
    match result {
        Ok(packages) => Json(SolveEnvironmentOk { packages }).into_response(),
        Err(e) => api_error_to_response(e),
    }
}

fn api_error_to_response(api_error: ApiError) -> Response {
    match api_error {
        ApiError::Internal(e) => {
            event!(Level::ERROR, "Internal server error: {}", e.to_string());
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(SolveEnvironmentErr::<()> {
                    error_kind: "internal".to_string(),
                    message: None,
                    additional_info: None,
                }),
            )
                .into_response()
        }
        ApiError::Validation(e) => (
            StatusCode::BAD_REQUEST,
            Json(SolveEnvironmentErr {
                error_kind: "validation".to_string(),
                message: Some(e.to_string()),
                additional_info: Some(e),
            }),
        )
            .into_response(),
        ApiError::Solver(e) => (
            StatusCode::CONFLICT,
            Json(SolveEnvironmentErr::<()> {
                error_kind: "solver".to_string(),
                message: Some(e.to_string()),
                additional_info: None,
            }),
        )
            .into_response(),
    }
}

async fn solve_environment_inner(
    state: Arc<AppState>,
    payload: SolveEnvironment,
) -> Result<Vec<RepoDataRecord>, ApiError> {
    let root_span = span!(Level::TRACE, "solve_environment");
    let _enter = root_span.enter();

    let channel_config = ChannelConfig::default();

    // Get match specs
    let mut matchspecs = Vec::with_capacity(payload.specs.len());
    let mut invalid_matchspecs = Vec::new();
    for spec in &payload.specs {
        match MatchSpec::from_str(spec, &channel_config) {
            Ok(spec) => matchspecs.push(spec),
            Err(e) => invalid_matchspecs.push(ParseError {
                input: spec.to_string(),
                error: e.to_string(),
            }),
        }
    }

    // Forbid invalid matchspecs
    if !invalid_matchspecs.is_empty() {
        return Err(ApiError::Validation(ValidationError::MatchSpecs(
            ParseErrors(invalid_matchspecs),
        )));
    }

    // Get the virtual packages
    let mut virtual_packages = Vec::with_capacity(payload.virtual_packages.len());
    for spec in &payload.virtual_packages {
        virtual_packages
            .push(parse_virtual_package(spec.as_str()).map_err(ValidationError::VirtualPackage)?);
    }

    // Parse channels
    let mut channels = Vec::new();
    let mut invalid_channels = Vec::new();
    for channel in &payload.channels {
        match Channel::from_str(channel, &channel_config) {
            Ok(c) => channels.push(c),
            Err(e) => invalid_channels.push(ParseError {
                input: channel.to_string(),
                error: e.to_string(),
            }),
        }
    }

    // Forbid invalid channels
    if !invalid_channels.is_empty() {
        return Err(ApiError::Validation(ValidationError::Channels(
            ParseErrors(invalid_channels),
        )));
    }

    // Each channel contains multiple subdirectories. Users can specify the subdirectories they want
    // to use when specifying their channels. If the user didn't specify the default subdirectories
    // we use defaults based on the current platform.
    let target_platform = match Platform::from_str(&payload.platform) {
        Ok(p) => p,
        Err(e) => {
            return Err(ApiError::Validation(ValidationError::Platform(
                ParseError {
                    input: payload.platform.to_string(),
                    error: e.to_string(),
                },
            )));
        }
    };

    let mut available_packages = Vec::new();
    let default_platforms = &[target_platform, Platform::NoArch];

    // TODO: do this in parallel
    for channel in channels {
        let platforms = channel
            .platforms
            .as_ref()
            .map(|p| p.as_slice())
            .unwrap_or(default_platforms);
        for &platform in platforms {
            let repo_data = state.available_packages.get(&channel, platform).await?;
            available_packages.push(repo_data);
        }
    }

    // This call will block for hundreds of milliseconds, or longer
    let result = tokio::task::spawn_blocking(move || {
        let available_packages: Vec<_> = available_packages
            .iter()
            .map(|repodata| repodata.as_libsolv_repo_data())
            .collect();
        let problem = SolverProblem {
            available_packages: available_packages.into_iter(),
            virtual_packages,
            specs: matchspecs,
            locked_packages: Vec::new(),
            pinned_packages: Vec::new(),
        };

        LibsolvBackend.solve(problem)
    })
    .instrument(span!(Level::DEBUG, "solve"))
    .await
    .context("solver thread panicked")
    .map_err(ApiError::Internal)?;

    Ok(result?)
}

fn parse_virtual_package(virtual_package: &str) -> Result<GenericVirtualPackage, ParseError> {
    let mut split = virtual_package.split('=');

    // Can unwrap first because split will always return at least one element
    let name = split.next().unwrap().to_string();
    let version = split
        .next()
        .unwrap_or("0")
        .parse()
        .map_err(|e| ParseError {
            input: virtual_package.to_string(),
            error: format!("invalid version - {e}"),
        })?;
    let build_string = split.next().unwrap_or("0").to_string();

    if split.next().is_some() {
        return Err(ParseError {
            input: virtual_package.to_string(),
            error: "too many equals signs".to_string(),
        });
    }

    Ok(GenericVirtualPackage {
        name,
        version,
        build_string,
    })
}