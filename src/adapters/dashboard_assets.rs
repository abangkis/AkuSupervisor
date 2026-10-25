//! Explicit embedded asset allowlist, shared by production and browser fixtures.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DashboardAsset {
    pub(crate) body: &'static [u8],
    pub(crate) content_type: &'static str,
}

pub(crate) fn dashboard_asset(target: &str) -> Option<DashboardAsset> {
    let (body, content_type): (&[u8], &str) = match target {
        "/" => (
            include_bytes!("../../dashboard/index.html"),
            "text/html; charset=utf-8",
        ),
        "/app.css" => (
            include_bytes!("../../dashboard/app.css"),
            "text/css; charset=utf-8",
        ),
        "/app.js" => (
            include_bytes!("../../dashboard/app.js"),
            "text/javascript; charset=utf-8",
        ),
        "/model.js" => (
            include_bytes!("../../dashboard/model.js"),
            "text/javascript; charset=utf-8",
        ),
        "/view.js" => (
            include_bytes!("../../dashboard/view.js"),
            "text/javascript; charset=utf-8",
        ),
        "/api.js" => (
            include_bytes!("../../dashboard/api.js"),
            "text/javascript; charset=utf-8",
        ),
        _ => return None,
    };
    Some(DashboardAsset { body, content_type })
}
