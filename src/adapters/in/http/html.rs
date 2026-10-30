// The browsable API: the same routes answering a page instead of JSON when a
// browser asks, with each route's help, links and action buttons read from the
// OpenAPI document. The rendering and the negotiation are web-rs's (the `web`
// repository), shared with the sibling service; what is this service's own is
// its Site below: its name, its cookies, and the routes whose body is its
// product rather than one of its resources.
pub use web_rs::pages::{CSP, Html, PageContext, THEMES, palette_css};

use crate::adapters::r#in::http::session::COOKIE_NAME;
use crate::adapters::r#in::http::spec;

pub fn site() -> web_rs::Site {
    web_rs::Site {
        product: "unified-api",
        spec: spec::spec(),
        session_cookie: COOKIE_NAME,
        login_cookie: "uapi_login",
        // Shared with the sibling service on purpose: one browser, one choice
        theme_cookie: "uapi_theme",
        raw_routes: &["/api/v1/endpoints/{id}"],
    }
}
