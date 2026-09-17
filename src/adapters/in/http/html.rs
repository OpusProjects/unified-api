use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::adapters::r#in::http::spec;

// Anything bigger than this is not rendered as a tree. A dataset is megabytes
// of JSON and the indented markup for it is several times that, which is a hung
// tab rather than a page. The browser gets a page that says so and links to the
// bytes; `?limit=` is how you actually look at a big source.
const MAX_RENDERED_BYTES: usize = 1024 * 1024;

// Big enough for any response this API produces as JSON (the body is already in
// memory by the time it reaches here — handlers serialize into a Vec).
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

// The policy every page this module serves is sent under.
//
// `default-src 'none'` means script-src falls back to none, so even an injected
// tag could not run. `frame-ancestors 'none'` is separate and NOT covered by
// that fallback, and it is the one that protects the action buttons: without
// it another site can put this page in an invisible iframe, line its Evict
// button up under a decoy, and collect a click. The form inside that frame is
// ours, submitting to our origin, carrying the victim's real CSRF token — so
// the token model is bypassed entirely and only this directive stops it.
pub const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; \
     form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

// The window a browser is sent to when it asked for something too big without
// saying how much it wanted.
const DEFAULT_PAGE: usize = 200;

// Did the caller choose their own window? Any of the paging parameters counts:
// naming one means the size of the response is their decision, not ours.
fn asked_for_a_window(query: Option<&str>) -> bool {
    query.is_some_and(|q| {
        q.split('&')
            .any(|p| p.starts_with("limit=") || p.starts_with("offset="))
    })
}

// Rendering the API's own JSON as a browsable page.
//
// The whole module rests on one type. `Html` is a String that is known to be
// safe to put in a page, and the ONLY way to make one out of arbitrary text is
// `Html::text`, which escapes. A `&str` from a hostvar cannot reach the output
// by accident, because a `&str` is not an `Html` and the compiler says so.
//
// This matters more here than it looks. Swagger UI is safe by construction: it
// hands a response to the DOM through `textContent`, so a hostvar containing
// `<script>` is DISPLAYED as those characters and never parsed as markup. A
// server-rendered page is the opposite — `format!("<td>{}</td>", value)` with
// the same hostvar produces a real script tag. And the value did not come from
// an attacker in the abstract; it came from a CMDB description field, a VM
// annotation, a connector's stdout. Someone types angle brackets into a Device42
// note without malice and the next operator to open that source executes it.
//
// So: escaping is not a discipline anyone has to remember. `Html::raw` exists
// for the markup this module writes itself, is one identifier long, and the
// audit is `grep raw(`.
pub struct Html(String);

impl Html {
    pub fn text(s: &str) -> Html {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                _ => out.push(c),
            }
        }
        Html(out)
    }

    // Markup this module wrote. Never call it with anything that came from a
    // request, a dataset or a connector.
    pub fn raw(s: String) -> Html {
        Html(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn esc(s: &str) -> String {
    Html::text(s).0
}

// ----------------------------------------------------------------- themes

// Colour schemes, chosen with a link rather than a dropdown.
//
// A <select> would need JavaScript to do anything, and the pages deliberately
// carry none — which is what lets the CSP forbid scripts outright. A link that
// sets a cookie does the same job with the machinery already here: the server
// renders the palette it was asked for, and remembers the choice per browser.
pub const THEMES: [&str; 6] = ["auto", "light", "dark", "dracula", "nord", "solarized"];
const THEME_COOKIE: &str = "uapi_theme";

pub fn palette_css(theme: &str) -> String {
    // bg, fg, dim, key, str, num, link, rule, panel
    let light = "--bg:#fbfbfa;--fg:#24292f;--dim:#6e7781;--key:#0550ae;--str:#0a6847;--num:#953800;--link:#0969da;--rule:#d8dee4;--panel:#fff;";
    let dark = "--bg:#0d1117;--fg:#e6edf3;--dim:#8b949e;--key:#79c0ff;--str:#7ee787;--num:#ffa657;--link:#58a6ff;--rule:#30363d;--panel:#161b22;";
    let dracula = "--bg:#282a36;--fg:#f8f8f2;--dim:#6272a4;--key:#8be9fd;--str:#50fa7b;--num:#bd93f9;--link:#ff79c6;--rule:#44475a;--panel:#21222c;";
    let nord = "--bg:#2e3440;--fg:#eceff4;--dim:#7b88a1;--key:#88c0d0;--str:#a3be8c;--num:#d08770;--link:#81a1c1;--rule:#434c5e;--panel:#3b4252;";
    let solarized = "--bg:#002b36;--fg:#93a1a1;--dim:#586e75;--key:#268bd2;--str:#859900;--num:#cb4b16;--link:#2aa198;--rule:#073642;--panel:#073642;";

    match theme {
        "light" => format!(":root{{{light}}}"),
        "dark" => format!(":root{{{dark}}}"),
        "dracula" => format!(":root{{{dracula}}}"),
        "nord" => format!(":root{{{nord}}}"),
        "solarized" => format!(":root{{{solarized}}}"),
        // "auto" and anything unrecognised: follow the browser.
        _ => format!(":root{{{light}}}@media(prefers-color-scheme:dark){{:root{{{dark}}}}}"),
    }
}

// The theme links, each preserving the query the page was asked with so that
// changing the colours does not also change the page.
fn theme_picker(path: &str, query: Option<&str>, current: &str) -> String {
    // Where a theme link goes.
    //
    // Normally: back to this page, same query, new colours. But this page may
    // be the RESULT of a POST — press Sync now and the answer is rendered at
    // /sources/{id}/sync, which only answers POST. A theme link there is a GET
    // at a POST-only route: 405, and the colour change loses the page as well.
    // So from a result page the links go to the nearest ancestor that can be
    // read, which is the resource the action was performed on.
    let target = nearest_readable(path);
    let query = if target == path { query } else { None };

    let mut out = String::from("<span class=\"themes\">");
    for theme in THEMES {
        let href = with_param(target, query, "theme", theme);
        let class = if theme == current {
            "theme on"
        } else {
            "theme"
        };
        out.push_str(&format!(
            "<a class=\"{class}\" href=\"{}\">{}</a>",
            esc(&href),
            esc(theme)
        ));
    }
    out.push_str("</span>");
    out
}

// The closest path at or above this one that answers GET. Used by anything in
// the chrome that has to produce a link from a page which may itself not be
// readable (an action result).
fn nearest_readable(path: &str) -> &str {
    if spec::is_gettable(path) {
        return path;
    }
    let mut candidate = path;
    while let Some(cut) = candidate.rfind('/') {
        if cut == 0 {
            break;
        }
        candidate = &candidate[..cut];
        if spec::is_gettable(candidate) {
            return candidate;
        }
    }
    "/api/v1/"
}

// Rebuild a URL with one query parameter set, dropping any previous copy of it.
fn with_param(path: &str, query: Option<&str>, name: &str, value: &str) -> String {
    let mut parts: Vec<String> = query
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with(&format!("{name}=")))
        .map(str::to_string)
        .collect();
    parts.push(format!("{name}={value}"));
    format!("{path}?{}", parts.join("&"))
}

// ------------------------------------------------------------------ links

// Which strings become links.
//
// ONLY paths this API serves. Not "anything that looks like a URL": a hostvar
// is free to contain `javascript:alert(1)` or a link to somewhere unpleasant,
// and turning arbitrary values into anchors would put both in the page over our
// own name. A value is a link when it is one of our routes, and otherwise it is
// text, however URL-shaped it looks.
fn is_internal_path(s: &str) -> bool {
    matches!(s, "/healthz" | "/readyz" | "/metrics" | "/swagger-ui/")
        || (s.starts_with("/api/v1")
            // No scheme, no authority, no traversal: a path, and nothing that
            // could be read as one by a browser's URL parser.
            && !s.contains("://")
            && !s.contains("..")
            && !s.starts_with("//"))
}

// Where the page can go from here, by hand: each ancestor of the current path.
// A tree you can only descend is half a tree.
//
// An ancestor is a LINK only when it is really a GET route. `/api/v1/enrichers/
// {id}` is not a resource — only its `/run` is, and that is a POST — so linking
// every ancestor blindly offered clicks that could only 405. The segment is
// still shown, so the trail stays readable; it just is not clickable.
fn breadcrumbs(path: &str) -> String {
    let mut out = String::from("<nav class=\"crumbs\"><a href=\"/api/v1/\">/api/v1</a>");
    let rest = path.strip_prefix("/api/v1").unwrap_or("");
    let mut built = String::from("/api/v1");
    for segment in rest.split('/').filter(|s| !s.is_empty()) {
        built.push('/');
        built.push_str(segment);
        out.push_str("<span class=\"sep\">/</span>");
        if spec::is_gettable(&built) {
            out.push_str(&format!("<a href=\"{}\">{}</a>", esc(&built), esc(segment)));
        } else {
            out.push_str(&format!("<span class=\"dead\">{}</span>", esc(segment)));
        }
    }
    out.push_str("</nav>");
    out
}

// ------------------------------------------------------------- json tree

// The generic view: a JSON tree, indented, with our own paths as anchors.
//
// Generic on purpose. A route added next year is browsable the day it is added,
// with no second place to register it — which is the property that keeps this
// from rotting the first time someone is in a hurry.
fn render_value(value: &Value, depth: usize, out: &mut String) {
    let pad = "  ".repeat(depth);
    let pad_inner = "  ".repeat(depth + 1);

    match value {
        Value::Null => out.push_str("<span class=\"null\">null</span>"),
        Value::Bool(b) => out.push_str(&format!("<span class=\"bool\">{b}</span>")),
        Value::Number(n) => out.push_str(&format!("<span class=\"num\">{n}</span>")),
        Value::String(s) => out.push_str(&render_string(s)),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("<span class=\"punct\">[]</span>");
                return;
            }
            out.push_str("<span class=\"punct\">[</span>\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad_inner);
                render_value(item, depth + 1, out);
                if i + 1 < items.len() {
                    out.push_str("<span class=\"punct\">,</span>");
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push_str("<span class=\"punct\">]</span>");
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("<span class=\"punct\">{}</span>");
                return;
            }
            out.push_str("<span class=\"punct\">{</span>\n");
            let len = map.len();
            for (i, (key, val)) in map.iter().enumerate() {
                out.push_str(&pad_inner);
                out.push_str(&format!(
                    "<span class=\"key\">\"{}\"</span><span class=\"punct\">: </span>",
                    esc(key)
                ));
                render_value(val, depth + 1, out);
                if i + 1 < len {
                    out.push_str("<span class=\"punct\">,</span>");
                }
                out.push('\n');
            }
            out.push_str(&pad);
            out.push_str("<span class=\"punct\">}</span>");
        }
    }
}

// One string value: a link when it can actually be followed, plain text
// otherwise.
//
// "Can be followed" means more than "is one of our paths". `related` carries
// `sync`, which is a POST — the way AWX lists `launch` on a job template — and
// rendering it as an anchor invited a click that could only ever 405, because
// clicking a link is always a GET. It stays visible (a consumer reading the
// JSON needs to know the route exists) but it is not a destination; its button
// is in the action bar above, which is the thing that can really POST.
fn render_string(s: &str) -> String {
    if !is_internal_path(s) {
        return format!("<span class=\"str\">\"{}\"</span>", esc(s));
    }

    if spec::is_gettable(s) {
        return format!("<a class=\"link\" href=\"{}\">\"{}\"</a>", esc(s), esc(s));
    }

    // Ours, but not readable. Say which verbs it does take, so the reason is
    // discoverable rather than a mystery.
    let verbs = spec::lookup(s, "GET")
        .map(|doc| doc.methods.join(", "))
        .unwrap_or_default();
    format!(
        "<span class=\"str notlink\" title=\"{}\">\"{}\"</span>",
        esc(&format!("{verbs} only — not a link; use the button above")),
        esc(s)
    )
}

// ------------------------------------------------------------------- page

pub struct PageContext<'a> {
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub method: &'a str,
    pub status: u16,
    pub headers: &'a HeaderMap,
    pub theme: &'a str,
    /// Who the browser is logged in as, when it is
    pub who: Option<&'a str>,
    /// The session's CSRF token, present only for a cookie-authenticated page.
    /// Its absence is what stops the action forms from being rendered at all.
    pub csrf: Option<&'a str>,
}

// The response headers worth showing, the way AWX's browsable API prints them
// above the body. `Allow` is the one that matters most: it is the answer to
// "why did that link 405 at me", available before clicking rather than after.
fn header_block(ctx: &PageContext, doc: Option<&spec::RouteDoc>) -> String {
    let mut lines = vec![format!(
        "<span class=\"hk\">HTTP</span> <span class=\"hv\">{} {}</span>",
        ctx.status,
        esc(status_text(ctx.status))
    )];

    if let Some(doc) = doc {
        lines.push(format!(
            "<span class=\"hk\">Allow:</span> <span class=\"hv\">{}</span>",
            esc(&doc.methods.join(", "))
        ));
    }

    for name in [header::CONTENT_TYPE, header::ETAG, header::VARY] {
        if let Some(value) = ctx.headers.get(&name).and_then(|v| v.to_str().ok()) {
            lines.push(format!(
                "<span class=\"hk\">{}:</span> <span class=\"hv\">{}</span>",
                esc(name.as_str()),
                esc(value)
            ));
        }
    }

    format!("<pre class=\"headers\">{}</pre>", lines.join("\n"))
}

fn status_text(status: u16) -> &'static str {
    StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("")
}

// The help panel: everything the route's own #[utoipa::path] attribute says.
//
// Collapsed by default behind a <details>, which is an element rather than a
// script — the same reason the theme picker is a link. Open it and the page
// explains itself with the text that also feeds Swagger.
fn help_panel(doc: &spec::RouteDoc, method: &str) -> String {
    let mut body = String::new();

    body.push_str(&format!(
        "<p class=\"route\"><span class=\"verb\">{}</span> <code>{}</code></p>",
        esc(method),
        esc(&doc.template)
    ));

    if let Some(description) = &doc.description {
        body.push_str(&format!("<p>{}</p>", esc(description)));
    }

    if !doc.parameters.is_empty() {
        body.push_str("<h4>Parameters</h4><table>");
        for param in &doc.parameters {
            body.push_str(&format!(
                "<tr><td><code>{}</code></td><td class=\"dimcell\">{}{}</td><td>{}</td></tr>",
                esc(&param.name),
                esc(param.location),
                if param.required { ", required" } else { "" },
                esc(param.description.as_deref().unwrap_or(""))
            ));
        }
        body.push_str("</table>");
    }

    if !doc.responses.is_empty() {
        body.push_str("<h4>Responses</h4><table>");
        for (code, description) in &doc.responses {
            body.push_str(&format!(
                "<tr><td><code>{}</code></td><td>{}</td></tr>",
                esc(code),
                esc(description)
            ));
        }
        body.push_str("</table>");
    }

    format!(
        "<details class=\"help\"><summary>?&nbsp;&nbsp;what is this route</summary>\
         <div class=\"helpbody\">{body}</div></details>"
    )
}

// The action bar: what can be done here, as buttons.
//
// A link cannot POST — clicking one is always a GET, which is why the `sync`
// entry in `related` answered 405 when it was treated as a destination. These
// are real forms.
//
// Rendered ONLY for a cookie-authenticated page. A header-authenticated client
// is a script and has no use for a button, and without a session there is no
// CSRF token to put in the form, which is precisely the credential that makes
// the write safe.
fn action_bar(ctx: &PageContext, template: &str, value: &Value) -> String {
    let Some(csrf) = ctx.csrf else {
        return String::new();
    };

    // What the ROUTE could accept, narrowed to what THIS OBJECT says it does.
    //
    // A view and a source answer at the same `/api/v1/sources/{id}`, so the
    // template alone offered a view the sync and evict buttons — both of which
    // can only ever answer 400, because a view holds no cache entry and gathers
    // nothing. The object already declares its own routes in `related`, and a
    // view's omits the writes; reading that is what makes the buttons honest,
    // for views today and for whatever else turns out to be read-only later.
    let declared: std::collections::HashSet<&str> = value
        .get("related")
        .and_then(Value::as_object)
        .map(|related| related.values().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let actions: Vec<spec::Action> = spec::actions(template)
        .into_iter()
        .filter(|action| declared.contains(format!("{}{}", ctx.path, action.suffix).as_str()))
        .collect();

    if actions.is_empty() {
        return String::new();
    }

    let mut out = String::from("<div class=\"actions\">");
    for action in actions {
        let target = format!("{}{}", ctx.path, action.suffix);
        let label = match (action.method, action.suffix.as_str()) {
            ("DELETE", "") => "Evict cache".to_string(),
            ("POST", "/sync") => "Sync now".to_string(),
            ("POST", suffix) => {
                // Title-cased by character, not by byte: `name[..1]` panics on
                // an empty suffix and on any multi-byte first character, and
                // this runs inside a middleware where a panic is a dropped
                // connection.
                let name = suffix.trim_start_matches('/');
                let mut chars = name.chars();
                match chars.next() {
                    Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                    None => "POST".to_string(),
                }
            }
            (method, suffix) => format!("{method} {suffix}"),
        };

        let mut fields = String::new();
        for param in &action.parameters {
            fields.push_str(&format!(
                "<label class=\"field\"><span>{}</span>\
                 <input name=\"{}\" placeholder=\"{}\"></label>",
                esc(&param.name),
                esc(&param.name),
                esc(param.description.as_deref().unwrap_or("")),
            ));
        }

        // A browser form speaks GET and POST only, so DELETE travels as a POST
        // with _method — translated back by the form middleware, and nowhere
        // else in the API.
        let method_field = if action.method == "DELETE" {
            "<input type=\"hidden\" name=\"_method\" value=\"DELETE\">"
        } else {
            ""
        };

        let danger = if action.method == "DELETE" {
            " danger"
        } else {
            ""
        };

        out.push_str(&format!(
            "<form class=\"action\" method=\"post\" action=\"{}\">\
             <input type=\"hidden\" name=\"_csrf\" value=\"{}\">{}\
             <button class=\"btn{}\" type=\"submit\">{}</button>{}</form>",
            esc(&target),
            esc(csrf),
            method_field,
            danger,
            esc(&label),
            fields,
        ));
    }
    out.push_str("</div>");
    out
}

pub fn page(ctx: &PageContext, value: &Value) -> String {
    let mut tree = String::new();
    render_value(value, 0, &mut tree);

    let doc = spec::lookup(ctx.path, ctx.method);
    let status_class = if (200..300).contains(&ctx.status) {
        "ok"
    } else {
        "bad"
    };

    let help = doc
        .as_ref()
        .map(|doc| help_panel(doc, ctx.method))
        .unwrap_or_default();

    let actions = doc
        .as_ref()
        .map(|doc| action_bar(ctx, &doc.template, value))
        .unwrap_or_default();

    let who = match ctx.who {
        Some(name) => format!(
            "<span class=\"who\">{}</span> <a class=\"auth\" href=\"/api/v1/logout\">log out</a>",
            esc(name)
        ),
        None => String::from("<a class=\"auth\" href=\"/api/v1/login\">log in</a>"),
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — unified-api</title>
<style>
{palette}
* {{ box-sizing: border-box; }}
body {{
  margin: 0; background: var(--bg); color: var(--fg);
  font: 14px/1.5 ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace;
}}
header {{
  padding: 12px 20px; border-bottom: 1px solid var(--rule); background: var(--panel);
  display: flex; flex-wrap: wrap; gap: 12px; align-items: center;
}}
.brand {{ font-weight: 700; letter-spacing: .02em; }}
.crumbs a {{ color: var(--link); text-decoration: none; }}
.crumbs a:hover {{ text-decoration: underline; }}
.crumbs .sep {{ color: var(--dim); padding: 0 2px; }}
.crumbs .dead {{ color: var(--dim); }}
.right {{ margin-left: auto; display: flex; gap: 10px; align-items: center; }}
.themes {{ display: flex; gap: 6px; }}
.theme {{
  color: var(--dim); text-decoration: none; font-size: 11px;
  padding: 1px 6px; border: 1px solid var(--rule); border-radius: 10px;
}}
.theme.on {{ color: var(--bg); background: var(--link); border-color: var(--link); }}
.who {{ font-size: 12px; color: var(--dim); }}
.auth {{ font-size: 12px; color: var(--link); text-decoration: none; }}
.status {{
  font-size: 12px; padding: 2px 8px; border-radius: 10px;
  border: 1px solid var(--rule); color: var(--dim);
}}
.status.ok {{ color: var(--str); }}
.status.bad {{ color: #ff7b72; }}
main {{ padding: 20px; display: flex; flex-direction: column; gap: 14px; }}
pre {{
  margin: 0; padding: 16px 20px; background: var(--panel);
  border: 1px solid var(--rule); border-radius: 8px;
  overflow-x: auto; white-space: pre; tab-size: 2;
}}
pre.headers {{ color: var(--dim); font-size: 12px; padding: 12px 20px; }}
.hk {{ color: var(--dim); }}
.hv {{ color: var(--str); }}
.key {{ color: var(--key); }}
.str {{ color: var(--str); }}
.num, .bool {{ color: var(--num); }}
.null, .punct {{ color: var(--dim); }}
.notlink {{ text-decoration: underline dotted var(--dim); text-underline-offset: 3px; cursor: help; }}
a.link {{ color: var(--link); text-decoration: none; border-bottom: 1px solid currentColor; }}
a.link:hover {{ background: color-mix(in srgb, var(--link) 15%, transparent); }}
details.help {{
  background: var(--panel); border: 1px solid var(--rule); border-radius: 8px;
}}
details.help summary {{
  cursor: pointer; padding: 10px 20px; color: var(--link); font-weight: 600;
}}
.helpbody {{ padding: 0 20px 16px; font-family: system-ui, sans-serif; }}
.helpbody h4 {{ margin: 16px 0 6px; font-size: 13px; color: var(--dim);
  text-transform: uppercase; letter-spacing: .06em; }}
.helpbody table {{ border-collapse: collapse; width: 100%; }}
.helpbody td {{
  padding: 4px 10px 4px 0; vertical-align: top; font-size: 13px;
  border-top: 1px solid var(--rule);
}}
.helpbody code {{ color: var(--key); font-family: ui-monospace, monospace; }}
.dimcell {{ color: var(--dim); white-space: nowrap; }}
.route .verb {{ color: var(--num); font-weight: 700; }}
.actions {{ display: flex; flex-wrap: wrap; gap: 10px; align-items: flex-end; }}
form.action {{
  display: flex; gap: 8px; align-items: flex-end; padding: 10px 12px;
  background: var(--panel); border: 1px solid var(--rule); border-radius: 8px;
}}
.btn {{
  padding: 7px 14px; border: 0; border-radius: 6px; cursor: pointer;
  background: var(--link); color: var(--bg); font-family: inherit;
  font-size: 13px; font-weight: 700;
}}
.btn.danger {{ background: #cf4b3c; color: #fff; }}
.field {{ display: flex; flex-direction: column; gap: 2px; font-size: 11px; color: var(--dim); }}
.field input {{
  padding: 6px 8px; border-radius: 5px; border: 1px solid var(--rule);
  background: var(--bg); color: var(--fg); font-family: inherit; font-size: 12px;
  min-width: 170px;
}}
footer {{ padding: 0 20px 24px; color: var(--dim); font-size: 12px; }}
footer a {{ color: var(--link); }}
</style>
</head>
<body>
<header>
  <span class="brand">unified-api</span>
  {crumbs}
  <span class="right">
    <span class="status {status_class}">HTTP {status}</span>
    {themes}
    {who}
  </span>
</header>
<main>
{actions}
{help}
{headers}
<pre>{tree}</pre>
</main>
<footer>
  <a href="{raw}">raw JSON</a> · <a href="/swagger-ui/">OpenAPI</a>
</footer>
</body>
</html>"#,
        title = esc(ctx.path),
        palette = palette_css(ctx.theme),
        crumbs = breadcrumbs(ctx.path),
        status = ctx.status,
        status_class = status_class,
        themes = theme_picker(ctx.path, ctx.query, ctx.theme),
        who = who,
        help = help,
        actions = actions,
        headers = header_block(ctx, doc.as_ref()),
        tree = tree,
        raw = esc(&with_param(ctx.path, ctx.query, "format", "json")),
    )
}

// Routes whose body belongs to somebody else. Matched by route template rather
// than by prefix, so a future `/api/v1/endpoints/{id}/something` of ours is not
// swept up by accident.
fn is_product_route(path: &str) -> bool {
    spec::lookup(path, "GET").is_some_and(|doc| doc.template == "/api/v1/endpoints/{id}")
}

// -------------------------------------------------------------- negotiate

// Content negotiation: the same route, the same data, a page instead of a body
// when the caller is a browser.
//
// A middleware rather than anything in the handlers, which is the point: they
// keep returning typed structs and know nothing about HTML, so a route added
// later is browsable without being told. It sits OUTSIDE the API key middleware
// (so it can render the 401 as a page rather than a JSON blob a browser shows
// as raw text) and INSIDE the compression layer (so the HTML is gzipped like
// everything else).
pub async fn negotiate(request: Request, next: Next) -> Response {
    // A browser sends `text/html` in Accept; curl sends `*/*` and AWX sends
    // either that or application/json. Asking for the literal substring is
    // enough to tell them apart and does not need a full Accept parser.
    let wants_html = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"));

    let query = request.uri().query().map(str::to_string);

    // The way back to the bytes from inside a browser.
    let forced_json = query
        .as_deref()
        .is_some_and(|q| q.split('&').any(|p| p == "format=json"));

    let path = request.uri().path().to_string();
    let method = request.method().as_str().to_string();

    // The theme, from the link that was just clicked or from the cookie that
    // remembers the last one.
    let cookies = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let picked = query.as_deref().and_then(|q| {
        q.split('&')
            .find_map(|p| p.strip_prefix("theme="))
            .filter(|t| THEMES.contains(t))
            .map(str::to_string)
    });
    let theme = picked.clone().unwrap_or_else(|| {
        cookies
            .as_deref()
            .and_then(|c| {
                crate::adapters::r#in::http::session::cookie_value(c, THEME_COOKIE)
                    .filter(|t| THEMES.contains(t))
            })
            .unwrap_or("auto")
            .to_string()
    });

    let response = next.run(request).await;

    // Vary on every response that passed through, not only the rendered ones:
    // a cache in front of this API must never hand a stored page to a consumer
    // that asked for JSON, and that consumer is AWX.
    let mut response = response;
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("accept"));

    if !wants_html || forced_json {
        return response;
    }

    // An output endpoint's body is the PRODUCT, not a resource of ours.
    //
    // Checking the content-type was not enough: `output: ansible` emits
    // application/json, which looked exactly like one of our own responses, so
    // a browser (or anything sending Accept: text/html — a wget with a browser
    // UA, a client library with a generous default) asking for an inventory got
    // a decorated page instead of the inventory. That is a silent breakage of
    // an integration, which is worse than an ugly page. The route is excluded
    // by identity, not by what its body happens to look like.
    if is_product_route(&path) {
        return response;
    }

    // Only what we produced as JSON. Raw YAML from the config routes is left
    // alone by the same check.
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/json"));

    if !is_json {
        return response;
    }

    let status = response.status();
    let (mut parts, body) = response.into_parts();

    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        // The body is gone either way at this point, so say so rather than
        // returning a truncated page pretending to be the resource.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "response body too large to render",
        )
            .into_response();
    };

    // Published by the auth middleware on the RESPONSE, because that middleware
    // runs inside this one and the request is gone by now.
    let identity = parts
        .extensions
        .get::<crate::adapters::r#in::http::auth::BrowserIdentity>()
        .cloned();

    let ctx = PageContext {
        path: &path,
        query: query.as_deref(),
        method: &method,
        status: status.as_u16(),
        headers: &parts.headers,
        theme: &theme,
        who: identity.as_ref().map(|i| i.key_name.as_str()),
        csrf: identity.as_ref().map(|i| i.csrf.as_str()),
    };

    // Too big to draw, and nobody asked for a size: send the browser to the
    // first page instead of explaining the problem to it.
    //
    // Only when the caller named NO window of their own. `?limit=900` is a
    // deliberate request for 900 hosts, and second-guessing it would make the
    // parameter advisory — so an explicit limit or offset renders whatever it
    // produces, however large. The redirect is the default, not the ceiling.
    if bytes.len() > MAX_RENDERED_BYTES
        && !asked_for_a_window(query.as_deref())
        && spec::accepts_limit(&path)
    {
        return axum::response::Redirect::to(&with_param(
            &path,
            query.as_deref(),
            "limit",
            &DEFAULT_PAGE.to_string(),
        ))
        .into_response();
    }

    // The size limit is a DEFAULT, not a ceiling. Having named a window, the
    // caller has said how much they want; `?limit=1000` on a thousand hosts is
    // a request to see a thousand hosts, and refusing it would make the
    // parameter a suggestion. Only a caller who named nothing gets protected
    // from a page their browser cannot draw.
    let rendered = if bytes.len() > MAX_RENDERED_BYTES && !asked_for_a_window(query.as_deref()) {
        page(&ctx, &oversized(&path, bytes.len()))
    } else {
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => page(&ctx, &value),
            // Valid JSON is what the handlers produce; if this ever fails, the
            // bytes are more useful than an error page about them.
            Err(_) => return Response::from_parts(parts, Body::from(bytes)),
        }
    };

    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // The page loads no scripts, no fonts, no images and no stylesheets of its
    // own, so everything can be forbidden except the inline <style> it carries.
    // This is the belt for whatever the escaping misses.
    parts.headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    // A rendered page carries the session's CSRF token in every action form,
    // and whatever inventory the route returned. Without this a shared cache
    // in front of the API is entitled to store one operator's page and hand it
    // to the next — token included, for a source the second one may not be
    // scoped to. `Vary: cookie` says the same thing to a cache that ignores
    // no-store.
    parts.headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    parts
        .headers
        .append(header::VARY, HeaderValue::from_static("cookie"));
    // Remember the theme for the next page. Not HttpOnly: it is a cosmetic
    // preference, not a credential, and nothing reads it but the renderer.
    if let Some(theme) = picked
        && let Ok(value) = HeaderValue::from_str(&format!(
            "{THEME_COOKIE}={theme}; Path=/; Max-Age=31536000; SameSite=Lax"
        ))
    {
        parts.headers.append(header::SET_COOKIE, value);
    }
    // The JSON's ETag does not validate the HTML rendering of it.
    parts.headers.remove(header::ETAG);
    parts.headers.remove(header::CONTENT_LENGTH);

    Response::from_parts(parts, Body::from(rendered))
}

// Reached only when paging is not on the table: a route with no `limit`, or a
// caller whose own `?limit=` still produced more than can be drawn. Both are
// cases where the honest answer is the size and a way to the bytes.
fn oversized(path: &str, bytes: usize) -> Value {
    serde_json::json!({
        "note": "this response is too large to render as a page",
        "size_bytes": bytes,
        "or_the_raw_bytes": format!("{path}?format=json"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx<'a>(path: &'a str, headers: &'a HeaderMap) -> PageContext<'a> {
        PageContext {
            path,
            query: None,
            method: "GET",
            status: 200,
            headers,
            theme: "auto",
            who: None,
            csrf: None,
        }
    }

    #[test]
    fn text_escapes_every_markup_character() {
        let escaped = Html::text(r#"<script>alert("x") & 'y'</script>"#);
        assert_eq!(
            escaped.as_str(),
            "&lt;script&gt;alert(&quot;x&quot;) &amp; &#39;y&#39;&lt;/script&gt;"
        );
    }

    // The case this module exists for: a connector's data reaching the page.
    #[test]
    fn injected_markup_in_a_value_renders_inert() {
        let headers = HeaderMap::new();
        let value = json!({ "description": "<img src=x onerror=alert(1)>" });
        let html = page(&ctx("/api/v1/sources/src-d42/dataset", &headers), &value);
        assert!(
            !html.contains("<img src=x"),
            "the tag must not survive as markup"
        );
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
    }

    #[test]
    fn injected_markup_in_a_key_renders_inert() {
        let headers = HeaderMap::new();
        let value = json!({ "<script>x</script>": 1 });
        let html = page(&ctx("/api/v1/sources", &headers), &value);
        assert!(!html.contains("<script>x</script>"));
        assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"));
    }

    #[test]
    fn our_own_paths_become_links() {
        let headers = HeaderMap::new();
        let value = json!({ "dataset": "/api/v1/sources/src-d42/dataset" });
        let html = page(&ctx("/api/v1/sources", &headers), &value);
        assert!(html.contains(r#"<a class="link" href="/api/v1/sources/src-d42/dataset">"#));
    }

    // A hostvar is free to contain a URL. It stays text.
    // The 405 trap, inside the JSON this time: `related.sync` is a POST route.
    // A view answers on the source routes but refuses every write, so its page
    // must offer no write buttons — the object's own `related` is what says so.
    #[test]
    fn a_view_gets_no_action_buttons() {
        let headers = HeaderMap::new();
        let mut c = ctx("/api/v1/sources/vw-facts", &headers);
        c.csrf = Some("tok");
        c.who = Some("Admin");

        let view = json!({
            "kind": "view",
            "related": {
                "dataset": "/api/v1/sources/vw-facts/dataset",
                "status": "/api/v1/sources/vw-facts/status",
            }
        });
        assert!(
            !page(&c, &view).contains("<button"),
            "a view accepts no writes, so it must offer no buttons"
        );

        let source = json!({
            "kind": "source",
            "related": {
                "dataset": "/api/v1/sources/vw-facts/dataset",
                "sync": "/api/v1/sources/vw-facts/sync",
                "evict": "/api/v1/sources/vw-facts",
            }
        });
        let html = page(&c, &source);
        assert!(html.contains("Sync now"));
        assert!(html.contains("Evict cache"));
    }

    #[test]
    fn a_post_only_route_is_shown_but_not_linked() {
        let headers = HeaderMap::new();
        let value = json!({
            "sync": "/api/v1/sources/src-d42/sync",
            "status": "/api/v1/sources/src-d42/status",
        });
        let html = page(&ctx("/api/v1/sources/src-d42", &headers), &value);

        assert!(
            !html.contains(r#"href="/api/v1/sources/src-d42/sync""#),
            "a POST route must not be rendered as a link"
        );
        assert!(
            html.contains("/api/v1/sources/src-d42/sync"),
            "but it must still be visible in the JSON"
        );
        assert!(
            html.contains(r#"href="/api/v1/sources/src-d42/status""#),
            "a GET route alongside it stays clickable"
        );
    }

    #[test]
    fn foreign_urls_never_become_links() {
        for hostile in [
            "javascript:alert(1)",
            "https://evil.example/pwn",
            "//evil.example/pwn",
            "/api/v1/../../etc/passwd",
            "http://x/api/v1/sources",
        ] {
            assert!(!is_internal_path(hostile), "must not link: {hostile}");
        }
    }

    #[test]
    fn internal_paths_are_recognised() {
        for ours in [
            "/api/v1/sources",
            "/api/v1/sources/src-d42/status",
            "/healthz",
            "/metrics",
        ] {
            assert!(is_internal_path(ours), "must link: {ours}");
        }
    }

    #[test]
    fn breadcrumbs_link_only_real_routes() {
        let crumbs = breadcrumbs("/api/v1/sources/src-d42/status");
        assert!(crumbs.contains(r#"href="/api/v1/sources""#));
        assert!(crumbs.contains(r#"href="/api/v1/sources/src-d42""#));
        assert!(crumbs.contains(r#"href="/api/v1/sources/src-d42/status""#));

        // The 405 trap: sync is a POST, so the trail shows it without linking.
        let crumbs = breadcrumbs("/api/v1/sources/src-d42/sync");
        assert!(crumbs.contains(r#"<span class="dead">sync</span>"#));
        assert!(!crumbs.contains(r#"href="/api/v1/sources/src-d42/sync""#));
    }

    // The help panel comes from the handler attributes, so this asserts the
    // wiring rather than the wording.
    #[test]
    fn the_help_panel_carries_the_specs_documentation() {
        let headers = HeaderMap::new();
        let page_html = page(&ctx("/api/v1/sources/src-d42/status", &headers), &json!({}));
        assert!(page_html.contains("what is this route"));
        assert!(
            page_html.contains("/api/v1/sources/{id}/status"),
            "the route template belongs in the help"
        );
        assert!(page_html.contains("Parameters"));
    }

    #[test]
    fn the_header_block_shows_which_methods_a_route_allows() {
        let headers = HeaderMap::new();
        let html = page(&ctx("/api/v1/sources/src-d42", &headers), &json!({}));
        assert!(html.contains("Allow:"));
        assert!(html.contains("DELETE"));
    }

    // Pressing Sync now lands on a POST-only path; the theme links there must
    // not be GETs at it.
    #[test]
    fn theme_links_from_an_action_result_go_to_the_resource() {
        assert_eq!(
            nearest_readable("/api/v1/sources/src-d42/sync"),
            "/api/v1/sources/src-d42"
        );
        assert_eq!(
            nearest_readable("/api/v1/sources/src-d42"),
            "/api/v1/sources/src-d42"
        );
        assert_eq!(
            nearest_readable("/api/v1/enrichers/enr-a/run"),
            "/api/v1/enrichers"
        );

        let picker = theme_picker("/api/v1/sources/src-d42/sync", None, "auto");
        assert!(
            !picker.contains("/sync?theme="),
            "a theme link must never point at a POST-only route"
        );
        assert!(picker.contains("/api/v1/sources/src-d42?theme=dracula"));
    }

    // An output endpoint emits application/json for `output: ansible`, so it
    // looks like one of ours. It is not: it is the inventory the consumer came
    // for, and it must survive Accept: text/html untouched.
    // Clickjacking defeats the CSRF token entirely if another site can frame
    // us: the form inside that frame is ours, submitting to our origin, with
    // the victim's real token in it. frame-ancestors is the only thing that
    // stops it, and it does NOT fall back to default-src.
    #[test]
    fn the_policy_forbids_framing() {
        assert!(CSP.contains("frame-ancestors 'none'"));
        assert!(CSP.contains("default-src 'none'"));
        assert!(CSP.contains("form-action 'self'"));
    }

    #[test]
    fn an_output_endpoint_is_never_rendered_as_a_page() {
        assert!(is_product_route("/api/v1/endpoints/ep-ansible-full"));
        // Our own metadata about endpoints is ours to render
        assert!(!is_product_route("/api/v1/endpoints"));
        assert!(!is_product_route("/api/v1/sources/src-d42/dataset"));
    }

    #[test]
    fn an_explicit_window_is_the_callers_decision() {
        assert!(asked_for_a_window(Some("limit=900")));
        assert!(asked_for_a_window(Some("offset=500")));
        assert!(asked_for_a_window(Some("theme=nord&limit=50")));
        // No window named: the page size is ours to choose
        assert!(!asked_for_a_window(None));
        assert!(!asked_for_a_window(Some("theme=nord")));
        assert!(!asked_for_a_window(Some("fields=rack")));
    }

    #[test]
    fn a_theme_link_keeps_the_page_and_its_query() {
        let url = with_param("/api/v1/sources", Some("limit=10"), "theme", "nord");
        assert_eq!(url, "/api/v1/sources?limit=10&theme=nord");
        // and replaces a previous choice rather than stacking
        let url = with_param("/api/v1/sources", Some("theme=dark"), "theme", "nord");
        assert_eq!(url, "/api/v1/sources?theme=nord");
    }

    #[test]
    fn every_theme_defines_the_whole_palette() {
        for theme in THEMES {
            let css = palette_css(theme);
            for var in [
                "--bg", "--fg", "--dim", "--key", "--str", "--num", "--link", "--rule", "--panel",
            ] {
                assert!(css.contains(var), "{theme} is missing {var}");
            }
        }
    }
}
