// The built web UI, served at "/" when `server.ui.assets_dir` names one: the
// UI and the API on one address, behind the same sign-in. The serving is
// web-rs's (the `web` repository); this module fixes it to this service's
// permissions type, the one thing a generic handler needs before axum can
// mount it.
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::adapters::r#in::http::auth::Permissions;

pub type UiAssets = web_rs::UiAssets<Permissions>;

pub async fn serve(state: State<UiAssets>, request: Request) -> Response {
    web_rs::ui_assets::serve(state, request).await
}

pub async fn page_for_browsers(state: State<UiAssets>, request: Request, next: Next) -> Response {
    web_rs::ui_assets::page_for_browsers(state, request, next).await
}
